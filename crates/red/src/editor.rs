//! The SQL editor pane: a toolbar (Run · history · read-only badge) over
//! Flint's `CodeEditor`. RED owns the domain bits — the SQL highlighter, the
//! completion candidates fed into the editor's generic completion seam, running
//! the current statement (or selection), and the query history. Results land in
//! the result grid.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use flint::prelude::*;
use gpui::{div, prelude::*, px, Context, Hsla, Pixels, Point, SharedString, Window};
use red_service::Command;

use crate::app::{ActiveConn, AppState, Phase};
use crate::schema::SchemaState;
use crate::sql::CompletionContext;

/// How many candidates the popup ever shows — the editor renders at most 8, but
/// we hand it a few more so prefix-narrowing has headroom.
const MAX_CANDIDATES: usize = 20;

/// In-app drag payload for the tab strip: the source tab's index. A tab drop
/// target reads it to reorder via [`AppState::move_tab`].
#[derive(Clone, Copy)]
struct TabDrag(usize);

/// The floating chip rendered under the cursor while a tab is being dragged.
/// GPUI's `on_drag` wants an `Entity<impl Render>`, so the tab strip mints one
/// of these with the dragged tab's label.
struct TabDragPreview {
    title: SharedString,
    /// Grab offset within the tab, so the chip tracks the pointer (not the
    /// tab's top-left, where GPUI anchors the preview).
    offset: Point<Pixels>,
    bg: Hsla,
    border: Hsla,
    text: Hsla,
}

impl Render for TabDragPreview {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        div().pl(self.offset.x).pt(self.offset.y).child(
            div()
                .flex()
                .items_center()
                .h(px(28.))
                .px_2p5()
                .bg(self.bg)
                .border_1()
                .border_color(self.border)
                .rounded(px(4.))
                .font_family(theme.font_family.clone())
                .text_size(theme.scale(12.))
                .text_color(self.text)
                .child(self.title.clone()),
        )
    }
}

/// The completion candidates derived from the loaded schema, grouped so the
/// provider can rank them by the cursor's context. Rebuilt as the schema grows.
struct CompletionIndex {
    /// Every object (table/view) name, sorted + deduped.
    tables: Vec<SharedString>,
    /// Columns keyed by lower-cased table name, for `table.`/`alias.` completion.
    columns_by_table: HashMap<String, Vec<SharedString>>,
    /// Every column name across the schema, sorted + deduped.
    all_columns: Vec<SharedString>,
    /// The upper-cased SQL keywords.
    keywords: Vec<SharedString>,
}

fn build_index(schema: &SchemaState) -> CompletionIndex {
    let mut tables: Vec<SharedString> = Vec::new();
    for sc in &schema.schemas {
        for obj in &sc.objects {
            tables.push(obj.name.clone().into());
        }
    }

    let mut columns_by_table: HashMap<String, Vec<SharedString>> = HashMap::new();
    let mut all_columns: Vec<SharedString> = Vec::new();
    for ((_, table), detail) in &schema.details {
        let entry = columns_by_table.entry(table.to_lowercase()).or_default();
        for col in &detail.columns {
            entry.push(col.name.clone().into());
            all_columns.push(col.name.clone().into());
        }
    }

    tables.sort();
    tables.dedup();
    all_columns.sort();
    all_columns.dedup();
    for cols in columns_by_table.values_mut() {
        cols.sort();
        cols.dedup();
    }

    let keywords = crate::sql::KEYWORDS
        .iter()
        .map(|kw| SharedString::from(kw.to_uppercase()))
        .collect();

    CompletionIndex {
        tables,
        columns_by_table,
        all_columns,
        keywords,
    }
}

/// The provider closure handed to the editor's completion seam. It reads the
/// cursor's context (member access, a table position, a column expression, or a
/// statement start) and offers the matching candidates, most-relevant first.
fn completion_provider(
    index: Rc<CompletionIndex>,
) -> impl Fn(&str, usize) -> Vec<SharedString> + 'static {
    move |content, cursor| {
        let prefix = crate::sql::word_prefix(content, cursor).to_lowercase();
        let context = crate::sql::analyze(content, cursor);

        // Only member access (`table.`) suggests with nothing typed; elsewhere we
        // wait for a prefix so the popup doesn't open on every space.
        if prefix.is_empty() && !matches!(context, CompletionContext::Dot { .. }) {
            return Vec::new();
        }

        // Candidate sources in priority order — earlier groups win ties.
        let mut ordered: Vec<SharedString> = Vec::new();
        match &context {
            CompletionContext::Dot { qualifier } => {
                let q = qualifier.to_lowercase();
                let real = crate::sql::referenced_tables_at(content, cursor)
                    .into_iter()
                    .find(|(alias, _)| *alias == q)
                    .map(|(_, table)| table.to_lowercase())
                    .or_else(|| {
                        index
                            .tables
                            .iter()
                            .find(|t| t.to_lowercase() == q)
                            .map(|t| t.to_lowercase())
                    });
                if let Some(cols) = real.and_then(|r| index.columns_by_table.get(&r)) {
                    ordered.extend(cols.iter().cloned());
                }
            }
            CompletionContext::Table => ordered.extend(index.tables.iter().cloned()),
            CompletionContext::Column => {
                // Columns of the tables this statement actually references rank
                // first, then the rest of the schema, then tables and keywords.
                for (_, table) in crate::sql::referenced_tables_at(content, cursor) {
                    if let Some(cols) = index.columns_by_table.get(&table.to_lowercase()) {
                        ordered.extend(cols.iter().cloned());
                    }
                }
                ordered.extend(index.all_columns.iter().cloned());
                ordered.extend(index.tables.iter().cloned());
                ordered.extend(index.keywords.iter().cloned());
            }
            CompletionContext::Keyword => {
                ordered.extend(index.keywords.iter().cloned());
                ordered.extend(index.tables.iter().cloned());
            }
        }

        let mut seen: HashSet<String> = HashSet::new();
        ordered
            .into_iter()
            .filter(|c| {
                let cl = c.to_lowercase();
                if !cl.starts_with(&prefix) || (!prefix.is_empty() && cl == prefix) {
                    return false;
                }
                seen.insert(cl)
            })
            .take(MAX_CANDIDATES)
            .collect()
    }
}

/// First non-empty, non-comment line of a query, truncated — the history label,
/// and the suggested name when saving a query (B3).
pub(crate) fn history_label(sql: &str) -> String {
    let line = sql
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("--"))
        .unwrap_or("");
    let truncated: String = line.chars().take(72).collect();
    if line.chars().count() > 72 {
        format!("{truncated}…")
    } else {
        truncated
    }
}

impl AppState {
    /// The editor pane: toolbar + the `CodeEditor` surface + a history popover.
    pub(crate) fn render_editor(
        &self,
        active: &ActiveConn,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let (bg_app, bg_panel, bg_elevated, bg_hover) = (
            theme.bg_app,
            theme.bg_panel,
            theme.bg_elevated,
            theme.bg_hover,
        );
        let (border, border_soft, radius) = (theme.border, theme.border_soft, theme.radius);
        let (text, muted, faint, dim) = (
            theme.text,
            theme.text_muted,
            theme.text_faint,
            theme.text_dim,
        );
        let (yellow, on_accent) = (theme.yellow, theme.on_accent);
        let accent = theme.accent;
        // UI-chrome icon size scaled with the general UI font (not the editor
        // mono font). Snapshotted here (Pixels is Copy) so it survives into the
        // lazy per-tab `.map` closure without re-borrowing `theme`.
        let icon_close = theme.scale(9.);
        // UI font + chrome sizes snapshotted (SharedString clones / Copy `Pixels`)
        // so the tab + breadcrumb chrome tracks the UI font even inside the lazy
        // per-tab `.map` closure. The editor *surface* keeps its own mono font.
        let ui_family = theme.font_family.clone();
        let mono_family = theme.mono_family.clone();
        let (size_11, size_12) = (theme.scale(11.), theme.scale(12.));
        let view = cx.entity().downgrade();

        // --- tab strip: one tab per open query + a "new query" affordance ---
        let active_idx = active.active_tab;
        // Drop-indicator state: the gap a dragged tab would land in, gated on an
        // actual drag so a stale target never paints once the drag ends.
        let tab_count = active.tabs.len();
        let drop_target = active.tab_drop_target;
        let dragging = cx.has_active_drag();
        let tabs = active.tabs.iter().enumerate().map(|(i, t)| {
            let is_active = i == active_idx;
            let (tab_bg, tab_text) = if is_active {
                (bg_app, text)
            } else {
                (bg_panel, muted)
            };
            let drag_title: SharedString = t.title.clone().into();
            let drop_view = view.clone();
            let move_view = view.clone();
            // Group so the close button reveals only on this tab's hover.
            let group = SharedString::from(format!("sql-tab-{i}"));
            // The dragged tab lands before this tab (gap == i) or after it
            // (gap == i+1); the bar paints on whichever edge the gap names.
            let bar_before = dragging && drop_target == Some(i);
            let bar_after = dragging && i + 1 == tab_count && drop_target == Some(tab_count);
            div()
                .id(("sql-tab", i))
                .group(group.clone())
                .relative()
                .flex()
                .flex_shrink_0()
                .items_center()
                .justify_center()
                // Stretch with the title between a comfortable min and a cap;
                // past the cap the label ellipsizes (see the title's `truncate`).
                .min_w(px(96.))
                .max_w(px(200.))
                // Symmetric horizontal room: the hover close button lives in the
                // right inset (right: 4px + 15px wide); mirror it on the left so
                // the centered title clears the button and stays balanced.
                .px(px(23.))
                .bg(tab_bg)
                .border_r_1()
                .border_color(border)
                .cursor_pointer()
                .when(!is_active, |d| d.hover(|s| s.bg(bg_elevated)))
                .on_click(cx.listener(move |this, _, _, cx| this.set_active_tab(i, cx)))
                // Drag this tab to reorder; the chip below tracks the cursor.
                .on_drag(TabDrag(i), move |_, offset, _window, cx| {
                    let title = drag_title.clone();
                    cx.new(move |_| TabDragPreview {
                        title,
                        offset,
                        bg: bg_elevated,
                        border,
                        text,
                    })
                })
                // Track the cursor across this tab to aim the drop gap at the
                // nearer edge, then commit the reorder on release. GPUI fires
                // this on *every* tab per mouse move (capture phase, no hover
                // gate), so we ignore moves whose cursor isn't over this tab —
                // only the hovered tab gets to set the gap.
                .on_drag_move::<TabDrag>(move |e, _window, cx| {
                    let b = e.bounds;
                    let p = e.event.position;
                    // Must be over this tab in *both* axes — checking x alone
                    // would keep re-setting the gap while dragging straight down
                    // off the strip, leaving a stale indicator.
                    if p.x < b.origin.x
                        || p.x >= b.origin.x + b.size.width
                        || p.y < b.origin.y
                        || p.y >= b.origin.y + b.size.height
                    {
                        return;
                    }
                    let gap = if p.x < b.origin.x + b.size.width / 2. {
                        i
                    } else {
                        i + 1
                    };
                    move_view
                        .update(cx, |this, cx| this.set_tab_drop_target(gap, cx))
                        .ok();
                })
                .on_drop::<TabDrag>(move |drag, _window, cx| {
                    let from = drag.0;
                    drop_view
                        .update(cx, |this, cx| this.drop_tab(from, cx))
                        .ok();
                })
                .when(bar_before, |d| {
                    d.child(
                        div()
                            .absolute()
                            .left_0()
                            .top_0()
                            .bottom_0()
                            .w(px(2.))
                            .bg(accent),
                    )
                })
                .when(bar_after, |d| {
                    d.child(
                        div()
                            .absolute()
                            .right_0()
                            .top_0()
                            .bottom_0()
                            .w(px(2.))
                            .bg(accent),
                    )
                })
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .font_family(ui_family.clone())
                        .text_size(size_12)
                        .text_color(tab_text)
                        .child(t.title.clone()),
                )
                // Close button: pinned to the right, revealed only on tab hover
                // so it never crowds the centered title at rest. The outer div
                // positions + vertically centers; the inner one is the hitbox.
                .child(
                    div()
                        .absolute()
                        .right(px(4.))
                        .top_0()
                        .bottom_0()
                        .flex()
                        .items_center()
                        .invisible()
                        .group_hover(group, |s| s.visible())
                        .child(
                            div()
                                .id(("sql-tab-close", i))
                                .flex()
                                .items_center()
                                .justify_center()
                                .size(px(15.))
                                .rounded(px(3.))
                                .text_color(faint)
                                .hover(|s| s.bg(bg_hover).text_color(text))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.request_close_tab(i, cx);
                                }))
                                .child(crate::icons::icon("close", icon_close, faint)),
                        ),
                )
        });
        let strip_drop_view = view.clone();
        let strip_move_view = view.clone();
        // The tabs live in a horizontally scrollable viewport, so a crowded
        // strip scrolls instead of squashing the tabs. `min_w(0)` lets the
        // flex child shrink below its content width so the overflow engages.
        let tab_viewport = div()
            .id("sql-tabstrip")
            .flex_1()
            .min_w(px(0.))
            .h_full()
            .flex()
            .items_stretch()
            .overflow_x_scroll()
            .track_scroll(&active.tab_scroll)
            // Capture phase runs the viewport before its tabs, so this clears
            // the indicator whenever the cursor isn't over the strip; a tab the
            // cursor *is* over then re-sets the gap. Net: the indicator only
            // shows while dragging within the tab bar.
            .on_drag_move::<TabDrag>(move |e, _window, cx| {
                let b = e.bounds;
                let p = e.event.position;
                let outside = p.x < b.origin.x
                    || p.x >= b.origin.x + b.size.width
                    || p.y < b.origin.y
                    || p.y >= b.origin.y + b.size.height;
                if outside {
                    strip_move_view
                        .update(cx, |this, cx| this.clear_tab_drop_target(cx))
                        .ok();
                }
            })
            // Release anywhere in the strip (incl. the trailing space) commits
            // using the gap the hovered tab last set. Harmless if a tab already
            // handled the drop — `drop_tab` consumes the target once.
            .on_drop::<TabDrag>(move |drag, _window, cx| {
                let from = drag.0;
                strip_drop_view
                    .update(cx, |this, cx| this.drop_tab(from, cx))
                    .ok();
            })
            .children(tabs);
        // The "＋" stays pinned right of the scrolling tabs, always reachable.
        let tabstrip = div()
            .flex_shrink_0()
            .h(px(35.))
            .flex()
            .items_stretch()
            .bg(bg_panel)
            .border_b_1()
            .border_color(border)
            .child(tab_viewport)
            .child(
                div()
                    .id("sql-new")
                    .flex_shrink_0()
                    .w(px(34.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .border_l_1()
                    .border_color(border)
                    .cursor_pointer()
                    .tooltip(Tooltip::text("New tab  ⌘T"))
                    .text_color(faint)
                    .hover(|s| s.bg(bg_elevated).text_color(text))
                    .on_click(cx.listener(|this, _, _, cx| this.new_query(cx)))
                    .child(crate::icons::icon("plus", theme.scale(13.), faint)),
            );

        // No open tab (user closed the last one): keep the strip — its ＋ opens
        // a new query — over an empty pane, and skip the editor/run/breadcrumb.
        let Some(tab) = active.active() else {
            let empty = div()
                .flex_1()
                .min_h(px(0.))
                .flex()
                .items_center()
                .justify_center()
                .text_size(size_12)
                .text_color(faint)
                .child("No query tab open — press ＋ to start");
            return div()
                .relative()
                .size_full()
                .flex()
                .flex_col()
                .bg(bg_app)
                .child(tabstrip)
                .child(empty);
        };

        // --- breadcrumb: connection › query ---
        let breadcrumb = div()
            .flex_shrink_0()
            .h(px(26.))
            .flex()
            .items_center()
            .gap_1p5()
            .px_3p5()
            .bg(bg_app)
            .border_b_1()
            .border_color(border_soft)
            .font_family(ui_family.clone())
            .text_size(size_11)
            .text_color(muted)
            .child(active.config.name.clone())
            .child(div().text_color(dim).child("/"))
            .child(div().text_color(text).child(tab.title.clone()));

        // The editor's own typography, applied here: the `CodeEditor` shapes its
        // text with `window.text_style()` / `window.line_height()`, both inherited
        // from this container — so setting them here drives the editor font without
        // it (or Flint) knowing about settings.
        let ed = &self.settings.editor;
        let surface = div()
            .flex_1()
            .min_h(px(0.))
            .font_family(ed.font_family.clone())
            .text_size(px(ed.font_size))
            .line_height(px(ed.font_size * ed.line_height))
            .child(tab.editor.clone());

        // --- bottom run bar: Run · history · ……… · read-only ---
        let ro_chip = active.config.read_only.then(|| {
            div()
                .ml_auto()
                .flex()
                .items_center()
                .px_2()
                .py(px(2.))
                .gap_1()
                .rounded(theme.radius_sm)
                .bg(yellow.opacity(0.1))
                .text_size(size_11)
                .text_color(yellow)
                .child(crate::icons::icon("lock", theme.scale(11.), yellow))
                .child("read-only")
        });
        let run_bar = div()
            .flex_shrink_0()
            // No fixed height: the 24px buttons define the strip and the equal
            // padding brackets them evenly. A fixed height taller than the
            // buttons left slack that GPUI distributed unevenly, sinking the
            // buttons off-center.
            .py(px(5.))
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .bg(bg_panel)
            .border_t_1()
            .border_color(border)
            .child(
                Button::new("sql-run", "Run  ⌘↵")
                    .variant(ButtonVariant::Primary)
                    .size(ButtonSize::Sm)
                    .icon(crate::icons::icon("play", theme.scale(11.), on_accent))
                    .on_click(cx.listener(|this, _, _, cx| this.run_editor_query(cx))),
            )
            .child(
                Button::new("sql-history", "History")
                    .variant(ButtonVariant::Ghost)
                    .size(ButtonSize::Sm)
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_history(cx))),
            )
            .child(
                Button::new("sql-explain", "Explain")
                    .variant(ButtonVariant::Ghost)
                    .size(ButtonSize::Sm)
                    .on_click(cx.listener(|this, _, _, cx| this.explain_query(false, cx))),
            )
            .child(
                Button::new("sql-save", "Save")
                    .variant(ButtonVariant::Ghost)
                    .size(ButtonSize::Sm)
                    .on_click(cx.listener(|this, _, _, cx| this.open_save_prompt(cx))),
            )
            .children(ro_chip);

        let history = active.history_open.then(|| {
            let list: Vec<_> = active.history.clone();
            let selected = active.history_sel;
            let inner = if list.is_empty() {
                div()
                    .px_2()
                    .py_1()
                    .text_size(size_11)
                    .text_color(faint)
                    .child("No history yet")
                    .into_any_element()
            } else {
                div()
                    .id("sql-history-list")
                    .max_h(px(260.))
                    .overflow_y_scroll()
                    .children(list.into_iter().enumerate().map(|(i, q)| {
                        let v = view.clone();
                        let sql = q.clone();
                        let is_sel = i == selected;
                        div()
                            .id(("hist", i))
                            .px_2()
                            .py_1()
                            .cursor_pointer()
                            .font_family(mono_family.clone())
                            .text_size(size_11)
                            .text_color(text)
                            .when(is_sel, |d| d.bg(bg_hover))
                            .hover(move |s| s.bg(bg_hover))
                            .on_click(move |_, _, cx| {
                                let sql = sql.clone();
                                v.update(cx, |this, cx| this.load_history(sql, cx)).ok();
                            })
                            .child(history_label(&q))
                    }))
                    .into_any_element()
            };
            // Anchored to the bottom run bar, opening upward. Focusable so its
            // ↑/↓ move the highlight, Enter loads it, Esc closes — back to editor.
            div()
                .id("sql-history")
                .key_context("History")
                .track_focus(&active.history_focus)
                .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _w, cx| {
                    match event.keystroke.key.as_str() {
                        "up" => this.history_move(-1, cx),
                        "down" => this.history_move(1, cx),
                        "enter" => this.history_accept(cx),
                        "escape" => this.close_history(cx),
                        _ => return,
                    }
                    cx.stop_propagation();
                }))
                .absolute()
                .bottom(px(38.))
                .right(px(8.))
                .w(px(380.))
                .bg(bg_elevated)
                .border_1()
                .border_color(border)
                .rounded(radius)
                .overflow_hidden()
                .child(inner)
        });

        div()
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(bg_app)
            .child(tabstrip)
            .child(breadcrumb)
            .child(surface)
            .child(run_bar)
            .children(history)
    }

    /// Run the selection if any, else the whole buffer. Pushes to history and
    /// streams the first window into the results pane.
    pub(crate) fn run_editor_query(&mut self, cx: &mut Context<Self>) {
        let sql = match &self.phase {
            Phase::Connected(active) => match active.active() {
                Some(tab) => {
                    let editor = tab.editor.read(cx);
                    editor.selected_text().unwrap_or_else(|| editor.content())
                }
                None => return,
            },
            _ => return,
        };
        let sql = sql.trim().to_string();
        if sql.is_empty() {
            return;
        }

        if let Phase::Connected(active) = &mut self.phase {
            // De-dupe consecutive identical runs at the head of the history.
            if active.history.first() != Some(&sql) {
                active.history.insert(0, sql.clone());
                active.history.truncate(50);
            }
            active.history_open = false;
        }

        // Row-returning statements stream into the grid; writes execute in a
        // transaction; destructive writes ask for confirmation first.
        let kind = crate::sql::classify(&sql);

        // On a read-only connection, refuse writes up front instead of letting
        // them round-trip to the engine and bounce back as a cryptic error. The
        // engine still rejects writes as a backstop; this is the friendly gate.
        let read_only = matches!(&self.phase, Phase::Connected(active) if active.config.read_only);
        if read_only && !matches!(kind, crate::sql::StatementKind::Query) {
            self.notify(
                ToastVariant::Error,
                "Connection is read-only — write statements are disabled.",
                cx,
            );
            return;
        }

        match kind {
            crate::sql::StatementKind::Query => {
                // Guard a bare `SELECT *` against flooding the grid: append the
                // configured `LIMIT` unless the user wrote their own.
                let sql =
                    crate::sql::auto_limit(&sql, self.settings.query.auto_limit).unwrap_or(sql);
                self.open_result("query", sql, None, cx)
            }
            crate::sql::StatementKind::Write => self.execute_sql(sql, cx),
            crate::sql::StatementKind::Destructive => {
                // The safety rail is opt-out in settings; when off, run immediately.
                if self.settings.query.confirm_destructive {
                    self.confirm_exec = Some(crate::app::PendingWrite::EditorSql(sql));
                    // Focus the modal so its own Enter/Esc handling is heard.
                    self.focus_modal = true;
                    cx.notify();
                } else {
                    self.execute_sql(sql, cx);
                }
            }
        }
    }

    /// Run a write/DDL statement in a transaction; refresh the schema tree after,
    /// since it may have created or dropped objects. The single seam through which
    /// writes leave the UI — so it also enforces the read-only gate, catching any
    /// caller that didn't pre-check (e.g. future inline-edit paths).
    pub(crate) fn execute_sql(&mut self, sql: String, cx: &mut Context<Self>) {
        if matches!(&self.phase, Phase::Connected(active) if active.config.read_only) {
            self.notify(
                ToastVariant::Error,
                "Connection is read-only — write statements are disabled.",
                cx,
            );
            return;
        }
        self.send_active(Command::Execute { sql });
        cx.notify();
    }

    pub(crate) fn toggle_history(&mut self, cx: &mut Context<Self>) {
        let opened = if let Phase::Connected(active) = &mut self.phase {
            active.history_open = !active.history_open;
            // Reset the keyboard highlight to the top whenever it opens.
            if active.history_open {
                active.history_sel = 0;
            }
            Some(active.history_open)
        } else {
            None
        };
        match opened {
            // Focus the popover so its arrow keys work; closing returns to the editor.
            Some(true) => self.focus_history = true,
            Some(false) => self.pending_focus = Some(crate::app::Pane::Editor),
            None => {}
        }
        cx.notify();
    }

    /// Move the history popover's highlight (↑/↓). No-op with an empty history.
    pub(crate) fn history_move(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let Phase::Connected(active) = &mut self.phase {
            let len = active.history.len();
            if len == 0 {
                return;
            }
            let sel = active.history_sel as isize + delta;
            active.history_sel = sel.clamp(0, len as isize - 1) as usize;
            cx.notify();
        }
    }

    /// Load the highlighted history entry into the editor (Enter in the popover).
    pub(crate) fn history_accept(&mut self, cx: &mut Context<Self>) {
        let sql = match &self.phase {
            Phase::Connected(active) => active.history.get(active.history_sel).cloned(),
            _ => None,
        };
        if let Some(sql) = sql {
            // `load_history` closes the popover and fills the editor.
            self.load_history(sql, cx);
        }
        self.pending_focus = Some(crate::app::Pane::Editor);
    }

    /// Close the history popover (Esc) and return focus to the editor.
    pub(crate) fn close_history(&mut self, cx: &mut Context<Self>) {
        if let Phase::Connected(active) = &mut self.phase {
            active.history_open = false;
        }
        self.pending_focus = Some(crate::app::Pane::Editor);
        cx.notify();
    }

    pub(crate) fn load_history(&mut self, sql: String, cx: &mut Context<Self>) {
        let editor = match &mut self.phase {
            Phase::Connected(active) => {
                active.history_open = false;
                match active.active_mut() {
                    Some(tab) => tab.editor.clone(),
                    None => return,
                }
            }
            _ => return,
        };
        editor.update(cx, |editor, cx| editor.set_content(sql, cx));
        cx.notify();
    }

    /// Rebuild every tab's editor completion candidates from the current schema.
    /// Called when the skeleton or a table's detail arrives, or a tab is opened.
    pub(crate) fn refresh_completions(&mut self, cx: &mut Context<Self>) {
        let (editors, index) = match &self.phase {
            Phase::Connected(active) => (
                active
                    .tabs
                    .iter()
                    .map(|t| t.editor.clone())
                    .collect::<Vec<_>>(),
                Rc::new(build_index(&active.schema)),
            ),
            _ => return,
        };
        for editor in editors {
            let index = index.clone();
            editor.update(cx, |editor, cx| {
                editor.set_completions(completion_provider(index), cx)
            });
        }
    }
}
