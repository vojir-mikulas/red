//! The cell detail inspector — a focused, right-docked pane that shows the
//! *whole* value under the grid cursor (Track B1). The grid intentionally
//! truncates: fat cells arrive as [`Value::Capped`] and even resident whole cells
//! are bounded by the driver's display cap. The inspector is where you see a long
//! `TEXT` wrapped, a JSON document pretty-printed, or a `BLOB` as a hex dump.
//!
//! It is **non-modal**: the grid keeps focus, so arrowing through cells updates
//! the pane live. A small resident cell renders straight from the buffer — **no**
//! backend work. Only a *capped* or *evicted* cell needs its full value, and that
//! is fetched **on demand** behind an explicit "Load full value", reusing the
//! existing full-fidelity `CopyRows` path (`PageCap::Full`); the bytes live only
//! while the pane is open and are dropped when the cursor moves or it closes.
//!
//! The value→view formatting (JSON pretty-print, hex dump) is RED-domain and pure;
//! the generic scrollable viewer is a candidate to push down into Flint later.

use flint::prelude::*;
use gpui::{div, prelude::*, px, AnyElement, ClipboardItem, Context, ScrollHandle, SharedString};
use red_core::{CappedCell, Value};
use red_service::Command;

use crate::app::{ActiveConn, AppState, Phase};
use crate::result::group_digits;

/// Bytes of a blob rendered as hex before the dump is cut with a "more bytes"
/// note — the *display* stays bounded even when a "Load full value" pulled a big
/// blob, so the pane never pays to lay out megabytes of hex.
const HEX_MAX: usize = 4 * 1024;

/// Bytes per hex row. Eight (not the conventional sixteen) keeps a row narrow
/// enough to fit the pane without wrapping, which would shear the columns apart.
const HEX_COLS: usize = 8;

/// All the inspector's persistent state. Present iff the pane is open.
pub(crate) struct InspectorState {
    /// Scroll position of the value body, kept across frames (the body is
    /// otherwise stateless, like the grid's own scroll handles).
    pub(crate) scroll: ScrollHandle,
    /// The full value fetched for one capped/evicted cell, formatted once on
    /// arrival so a big value isn't re-formatted (or cloned) every frame. Cleared
    /// when the cursor moves to a different cell (see [`AppState::reconcile_inspector`]).
    full: Option<InspectedFull>,
    /// The in-flight full-fetch, if any — its reply is matched by `id`.
    pending: Option<PendingInspect>,
}

impl InspectorState {
    fn new() -> Self {
        Self {
            scroll: ScrollHandle::new(),
            full: None,
            pending: None,
        }
    }
}

/// A loaded full value, addressed by the cell it belongs to so a stale one (the
/// cursor moved) is recognised and dropped. Holds the *formatted* body, not the
/// raw [`Value`], so rendering never re-formats or clones the (possibly large) value.
struct InspectedFull {
    epoch: u64,
    row: usize,
    col: usize,
    view: ValueView,
}

/// A `CopyRows` re-fetch issued for the inspector, awaiting its `CopyRowsLoaded`.
struct PendingInspect {
    id: u64,
    epoch: u64,
    row: usize,
    col: usize,
}

/// A value formatted for the pane: the body text, a one-line type/size summary,
/// and whether the body should soft-wrap (prose) or stay on fixed lines (hex/JSON).
#[derive(Clone)]
struct ValueView {
    body: SharedString,
    summary: String,
    wrap: bool,
}

/// What the focused cell resolves to right now.
enum CellState {
    /// A value ready to show — a small resident cell, or a loaded full value.
    Ready(ValueView),
    /// Resident but display-capped: only a head/length is known until loaded.
    Capped(CappedCell),
    /// Scrolled out of the resident window — load it to see anything.
    Evicted,
}

/// The fully-resolved inspector target for one frame.
struct InspectorView {
    col_name: String,
    decl_type: Option<String>,
    row: usize,
    state: CellState,
}

impl AppState {
    /// ⌘I / the toolbar button: open the detail inspector, or close it if open.
    pub(crate) fn toggle_inspector(&mut self, cx: &mut Context<Self>) {
        self.inspector = match self.inspector {
            Some(_) => None,
            None => Some(InspectorState::new()),
        };
        cx.notify();
    }

    /// Open the inspector if it isn't already (the double-click-a-cell entry —
    /// double-click should reveal, never toggle-shut).
    pub(crate) fn open_inspector(&mut self, cx: &mut Context<Self>) {
        if self.inspector.is_none() {
            self.inspector = Some(InspectorState::new());
            cx.notify();
        }
    }

    /// Esc: close the inspector if open (a no-op otherwise, so Esc keeps doing
    /// nothing from the grid when the pane is closed).
    pub(crate) fn close_inspector(&mut self, cx: &mut Context<Self>) {
        if self.inspector.is_some() {
            self.inspector = None;
            cx.notify();
        }
    }

    /// The `(epoch, row, data-col)` of the cell under the grid cursor, mapping the
    /// selection focus through the gutter and clamping to the data columns. `None`
    /// when nothing is selected or no result is open.
    fn focused_cell(&self) -> Option<(u64, usize, usize)> {
        let Phase::Connected(active) = &self.phase else {
            return None;
        };
        let grid = active.active_result()?;
        let (row, col) = grid.cursor_cell(self.gutter())?;
        Some((grid.epoch, row, col))
    }

    /// Drop a loaded/in-flight full value once the cursor has moved off the cell it
    /// belonged to (or the result was replaced). Called once per frame before the
    /// shell renders, so the bytes of a big inspected value never outlive the
    /// cursor sitting on it. Keeps the "fetched bytes dropped when focus moves"
    /// budget promise without threading inspector state through every grid handler.
    pub(crate) fn reconcile_inspector(&mut self) {
        let cur = self.focused_cell();
        let Some(insp) = &mut self.inspector else {
            return;
        };
        let matches = |epoch, row, col| cur == Some((epoch, row, col));
        if let Some(full) = &insp.full {
            if !matches(full.epoch, full.row, full.col) {
                insp.full = None;
            }
        }
        if let Some(p) = &insp.pending {
            if !matches(p.epoch, p.row, p.col) {
                insp.pending = None;
            }
        }
    }

    /// "Load full value": re-fetch the focused cell's row in full (reusing the
    /// clipboard's `CopyRows` path — `PageCap::Full`) so a capped or evicted cell
    /// can show its whole value. One row, on demand, behind an explicit click.
    pub(crate) fn load_inspector_full(&mut self, cx: &mut Context<Self>) {
        let Some((epoch, row, col)) = self.focused_cell() else {
            return;
        };
        if self.inspector.is_none() {
            return;
        }
        let id = self.next_copy_id;
        self.next_copy_id += 1;
        if let Some(insp) = &mut self.inspector {
            insp.pending = Some(PendingInspect {
                id,
                epoch,
                row,
                col,
            });
        }
        // A single row at this ordinal, full fidelity — the driver's display cap
        // doesn't apply to `CopyRows`, so the whole cell comes back.
        self.send_active(Command::CopyRows {
            offset: row,
            limit: 1,
            epoch,
            id,
        });
        cx.notify();
    }

    /// A `CopyRows` reply whose id matches the inspector's in-flight request:
    /// format the one cell we care about and stash it. A stale reply (the cursor
    /// moved, clearing `pending`) finds no match and is dropped. Returns whether it
    /// claimed the reply, so the copy path only runs when it didn't.
    pub(crate) fn on_inspect_rows(&mut self, id: u64, rows: &[Vec<Value>]) -> bool {
        let Some(insp) = &mut self.inspector else {
            return false;
        };
        let Some(p) = insp.pending.take_if(|p| p.id == id) else {
            return false;
        };
        if let Some(value) = rows.first().and_then(|r| r.get(p.col)) {
            insp.full = Some(InspectedFull {
                epoch: p.epoch,
                row: p.row,
                col: p.col,
                view: format_value(value),
            });
        }
        true
    }

    /// Resolve the cell under the cursor into something renderable: a loaded full
    /// value, a small resident value formatted on the spot, a capped stand-in, or
    /// an evicted (off-window) cell.
    fn inspector_cell(&self, active: &ActiveConn) -> Option<InspectorView> {
        let grid = active.active_result()?;
        let (row, col) = grid.cursor_cell(self.gutter())?;
        let (col_name, decl_type) = grid.column_meta(col)?;

        // A loaded full value wins (formatted once, at load time).
        if let Some(full) = self.inspector.as_ref().and_then(|i| i.full.as_ref()) {
            if full.epoch == grid.epoch && full.row == row && full.col == col {
                return Some(InspectorView {
                    col_name,
                    decl_type,
                    row,
                    state: CellState::Ready(full.view.clone()),
                });
            }
        }

        // Otherwise read the resident window. Whole cells (under-cap, or the key
        // column) format straight away — they're bounded, so this is cheap.
        let state = match grid.cell_value(row, col) {
            Some(Value::Capped(c)) => CellState::Capped(c),
            Some(v) => CellState::Ready(format_value(&v)),
            None => CellState::Evicted,
        };
        Some(InspectorView {
            col_name,
            decl_type,
            row,
            state,
        })
    }

    /// The right-docked inspector pane. Rendered as a sibling of the grid (the grid
    /// narrows; it does not occlude it), so the cursor and its live updates stay
    /// visible. Only called while `self.inspector` is `Some` and a result is ready.
    pub(crate) fn render_inspector(
        &self,
        active: &ActiveConn,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (bg, border, text, muted, faint) = (
            theme.bg_panel,
            theme.border,
            theme.text,
            theme.text_muted,
            theme.text_faint,
        );
        let ui_family = theme.font_family.clone();
        let mono_family = theme.mono_family.clone();
        let (s11, s12) = (theme.scale(11.), theme.scale(12.));
        let body_size = theme.font_size;

        // Header: column name + type, a close ✕.
        let resolved = self.inspector_cell(active);
        let (title, subtitle) = match &resolved {
            Some(v) => {
                let ty = v
                    .decl_type
                    .as_deref()
                    .filter(|t| !t.is_empty())
                    .map(|t| t.to_lowercase());
                let sub = match (&v.state, ty) {
                    (CellState::Ready(view), Some(ty)) => format!("{ty} · {}", view.summary),
                    (CellState::Ready(view), None) => view.summary.clone(),
                    (CellState::Capped(c), ty) => {
                        let kind = if c.blob { "blob" } else { "text" };
                        let ty = ty.unwrap_or_else(|| kind.into());
                        format!("{ty} · {} bytes · capped", group_digits(c.len))
                    }
                    (CellState::Evicted, ty) => ty.unwrap_or_else(|| "—".into()),
                };
                (
                    v.col_name.clone(),
                    format!("row {} · {sub}", group_digits(v.row + 1)),
                )
            }
            None => (
                "Cell inspector".to_string(),
                "Select a cell to inspect".to_string(),
            ),
        };

        let header = div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py(px(6.))
            .border_b_1()
            .border_color(border)
            .font_family(ui_family.clone())
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .flex()
                    .flex_col()
                    .child(div().text_size(s12).text_color(text).child(title))
                    .child(div().text_size(s11).text_color(faint).child(subtitle)),
            )
            .child(
                Button::new("inspector-close", "✕")
                    .variant(ButtonVariant::Ghost)
                    .size(ButtonSize::Sm)
                    .on_click(cx.listener(|this, _, _, cx| this.close_inspector(cx))),
            );

        // Body + actions vary by state.
        let (body, action): (AnyElement, Option<AnyElement>) = match resolved.map(|v| v.state) {
            Some(CellState::Ready(view)) => {
                let copy = view.body.clone();
                (
                    self.inspector_body(&view, mono_family.clone(), body_size),
                    Some(
                        Button::new("inspector-copy", "Copy")
                            .variant(ButtonVariant::Ghost)
                            .size(ButtonSize::Sm)
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(copy.to_string()));
                            }))
                            .into_any_element(),
                    ),
                )
            }
            Some(CellState::Capped(c)) => {
                let note = if c.blob {
                    format!("{} bytes — load to view as hex.", group_digits(c.len))
                } else {
                    format!(
                        "Showing the first {} bytes of {}.",
                        group_digits(c.head.len()),
                        group_digits(c.len)
                    )
                };
                let preview = (!c.head.is_empty()).then(|| {
                    div()
                        .id("inspector-body")
                        .flex_1()
                        .min_h(px(0.))
                        .overflow_y_scroll()
                        .p_3()
                        .font_family(mono_family.clone())
                        .text_size(body_size)
                        .text_color(text)
                        .child(SharedString::from(c.head))
                });
                (
                    div()
                        .flex_1()
                        .min_h(px(0.))
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .flex_shrink_0()
                                .px_3()
                                .py_2()
                                .text_size(s11)
                                .text_color(faint)
                                .font_family(ui_family.clone())
                                .child(note),
                        )
                        .children(preview)
                        .into_any_element(),
                    Some(self.load_button(cx)),
                )
            }
            Some(CellState::Evicted) => (
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .p_3()
                    .text_size(s12)
                    .text_color(faint)
                    .font_family(ui_family.clone())
                    .child("This row scrolled out of view — load it to inspect.")
                    .into_any_element(),
                Some(self.load_button(cx)),
            ),
            None => (
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .p_3()
                    .text_size(s12)
                    .text_color(faint)
                    .font_family(ui_family.clone())
                    .child("Select a cell to inspect its full value.")
                    .into_any_element(),
                None,
            ),
        };

        let footer = action.map(|a| {
            div()
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_end()
                .gap_1()
                .px_3()
                .py(px(6.))
                .border_t_1()
                .border_color(border)
                .child(a)
        });

        div()
            .id("inspector")
            .flex_shrink_0()
            .w(px(440.))
            .h_full()
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(border)
            .bg(bg)
            .text_color(muted)
            .child(header)
            .child(body)
            .children(footer)
            .into_any_element()
    }

    /// The shared "Load full value" button — fetches the focused cell in full.
    fn load_button(&self, cx: &mut Context<Self>) -> AnyElement {
        let pending = self.inspector.as_ref().is_some_and(|i| i.pending.is_some());
        Button::new(
            "inspector-load",
            if pending {
                "Loading…"
            } else {
                "Load full value"
            },
        )
        .variant(ButtonVariant::Primary)
        .size(ButtonSize::Sm)
        .disabled(pending)
        .on_click(cx.listener(|this, _, _, cx| this.load_inspector_full(cx)))
        .into_any_element()
    }

    /// The scrollable value body: prose soft-wraps; hex/JSON stay on fixed lines
    /// (so columns line up) and scroll horizontally if a line overflows.
    fn inspector_body(
        &self,
        view: &ValueView,
        mono_family: SharedString,
        size: gpui::Pixels,
    ) -> AnyElement {
        let theme_scroll = self.inspector.as_ref().map(|i| &i.scroll);
        let mut body = div()
            .id("inspector-body")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .p_3()
            .font_family(mono_family)
            .text_size(size);
        if let Some(scroll) = theme_scroll {
            body = body.track_scroll(scroll);
        }
        if view.wrap {
            body.child(view.body.clone()).into_any_element()
        } else {
            // Fixed-line content (hex, pretty JSON): one non-shrinking row per line
            // inside a horizontally-scrollable column, so a long line scrolls rather
            // than wrapping and shearing the layout.
            body.overflow_x_scroll()
                .flex()
                .flex_col()
                .children(view.body.lines().map(|line| {
                    div()
                        .flex_shrink_0()
                        .child(SharedString::from(line.to_string()))
                }))
                .into_any_element()
        }
    }
}

/// Format a value for the pane: the body text, a one-line summary, and whether it
/// should soft-wrap. Pure and small — the RED-domain half of the inspector.
fn format_value(value: &Value) -> ValueView {
    match value {
        Value::Null => ValueView {
            body: "NULL".into(),
            summary: "null".into(),
            wrap: true,
        },
        Value::Integer(n) => ValueView {
            body: n.to_string().into(),
            summary: "integer".into(),
            wrap: true,
        },
        Value::Real(x) => ValueView {
            body: x.to_string().into(),
            summary: "real".into(),
            wrap: true,
        },
        Value::Text(s) => {
            let chars = group_digits(s.chars().count());
            match pretty_json(s) {
                Some(pretty) => ValueView {
                    body: pretty.into(),
                    summary: format!("{chars} chars · JSON"),
                    wrap: false,
                },
                None => ValueView {
                    body: s.clone().into(),
                    summary: format!("{chars} chars · text"),
                    wrap: true,
                },
            }
        }
        Value::Blob(b) => ValueView {
            body: hex_dump(b, HEX_MAX).into(),
            summary: format!("{} bytes · blob", group_digits(b.len())),
            wrap: false,
        },
        // A capped value only reaches here if formatted directly (defensive — the
        // pane normally branches on `CellState::Capped` before formatting).
        Value::Capped(c) if c.blob => ValueView {
            body: format!("<{} bytes>", c.len).into(),
            summary: format!("{} bytes · blob (capped)", group_digits(c.len)),
            wrap: true,
        },
        Value::Capped(c) => ValueView {
            body: format!("{}…", c.head).into(),
            summary: format!("{} bytes · text (capped)", group_digits(c.len)),
            wrap: true,
        },
    }
}

/// Re-indent JSON-looking text for readability. Returns `None` when the input
/// doesn't open like JSON (so the caller shows it as plain text). A tolerant
/// re-formatter, not a validator: it walks the text honoring string literals and
/// escapes, collapses existing whitespace, and re-emits newlines + indentation.
/// ~40 lines beats pulling in a JSON crate (the project ships none).
fn pretty_json(s: &str) -> Option<String> {
    let t = s.trim();
    if !(t.starts_with('{') || t.starts_with('[')) {
        return None;
    }
    let mut out = String::with_capacity(t.len() + t.len() / 4);
    let mut depth: usize = 0;
    let mut in_str = false;
    let mut esc = false;
    let mut chars = t.chars().peekable();
    while let Some(c) = chars.next() {
        if in_str {
            out.push(c);
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_str = true;
                out.push(c);
            }
            '{' | '[' => {
                out.push(c);
                // Keep an empty container on one line: `{}` / `[]`.
                if matches!(chars.peek(), Some('}') | Some(']')) {
                    out.push(chars.next().unwrap());
                } else {
                    depth += 1;
                    indent(&mut out, depth);
                }
            }
            '}' | ']' => {
                depth = depth.saturating_sub(1);
                indent(&mut out, depth);
                out.push(c);
            }
            ',' => {
                out.push(c);
                indent(&mut out, depth);
            }
            ':' => out.push_str(": "),
            c if c.is_whitespace() => {} // collapse; structure drives the layout
            c => out.push(c),
        }
    }
    Some(out)
}

/// Push a newline and `depth` levels of two-space indentation.
fn indent(out: &mut String, depth: usize) {
    out.push('\n');
    for _ in 0..depth {
        out.push_str("  ");
    }
}

/// A classic `offset  hex  |ascii|` hex dump over at most `max` bytes (the rest is
/// summarized), [`HEX_COLS`] bytes per row so a row fits the pane unwrapped.
fn hex_dump(bytes: &[u8], max: usize) -> String {
    let shown = bytes.len().min(max);
    let mut out = String::with_capacity(shown * 4);
    for (i, chunk) in bytes[..shown].chunks(HEX_COLS).enumerate() {
        out.push_str(&format!("{:08x}  ", i * HEX_COLS));
        for b in chunk {
            out.push_str(&format!("{b:02x} "));
        }
        for _ in chunk.len()..HEX_COLS {
            out.push_str("   ");
        }
        out.push_str(" |");
        for b in chunk {
            let c = if b.is_ascii_graphic() || *b == b' ' {
                *b as char
            } else {
                '.'
            };
            out.push(c);
        }
        out.push_str("|\n");
    }
    if bytes.len() > shown {
        out.push_str(&format!(
            "\n… {} more bytes (showing the first {shown})",
            bytes.len() - shown
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pretty_json_indents_objects_and_arrays() {
        let got = pretty_json(r#"{"a":1,"b":[2,3],"c":{}}"#).unwrap();
        assert_eq!(
            got,
            "{\n  \"a\": 1,\n  \"b\": [\n    2,\n    3\n  ],\n  \"c\": {}\n}"
        );
    }

    #[test]
    fn pretty_json_keeps_commas_and_colons_inside_strings_untouched() {
        // A `:` and `,` and brace inside a string must not trigger layout.
        let got = pretty_json(r#"{"k":"a,b: {x}"}"#).unwrap();
        assert_eq!(got, "{\n  \"k\": \"a,b: {x}\"\n}");
    }

    #[test]
    fn pretty_json_handles_escaped_quote_in_string() {
        let got = pretty_json(r#"{"k":"a\"b"}"#).unwrap();
        assert_eq!(got, "{\n  \"k\": \"a\\\"b\"\n}");
    }

    #[test]
    fn pretty_json_rejects_non_json() {
        assert!(pretty_json("hello world").is_none());
        // Leading whitespace is trimmed; this still opens with a non-brace → None.
        assert!(pretty_json("  not json {nope").is_none());
        assert!(pretty_json("42").is_none());
    }

    #[test]
    fn hex_dump_formats_offset_hex_and_ascii() {
        let dump = hex_dump(b"AB\x00\xff", 64);
        // 4 of 8 columns filled, then 4 columns of blank padding before the ascii.
        assert_eq!(dump, "00000000  41 42 00 ff              |AB..|\n");
    }

    #[test]
    fn hex_dump_caps_and_notes_the_remainder() {
        let bytes = vec![0u8; 20];
        let dump = hex_dump(&bytes, 8);
        assert!(dump.contains("12 more bytes (showing the first 8)"));
        // Only the first 8 bytes (one row) are dumped.
        assert_eq!(
            dump.lines().filter(|l| l.starts_with("00000000")).count(),
            1
        );
    }

    #[test]
    fn format_value_classifies_text_json_and_blob() {
        assert!(
            matches!(format_value(&Value::Text(r#"{"a":1}"#.into())), v if v.summary.contains("JSON") && !v.wrap)
        );
        assert!(
            matches!(format_value(&Value::Text("plain".into())), v if v.summary.contains("text") && v.wrap)
        );
        assert!(
            matches!(format_value(&Value::Blob(vec![1, 2, 3])), v if v.summary.contains("blob") && !v.wrap)
        );
        assert!(matches!(format_value(&Value::Null), v if v.body.as_ref() == "NULL"));
    }
}
