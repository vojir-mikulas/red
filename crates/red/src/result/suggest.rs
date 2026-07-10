//! Track B8: the in-cell foreign-key suggestion picker.
//!
//! When the open inline editor ([`GridEdit`](super::GridEdit)) targets a single-column
//! foreign-key cell, we hang a searchable dropdown of the referenced table's existing
//! ids off the field: the user can pick an id (optionally shown with a human label) or
//! keep typing their own value. The list is a bounded `SELECT DISTINCT id[, label]`
//! fetched once per FK target (`Command::FetchLookup` → `Event::LookupReady`), cached on
//! the connection, and filtered **client-side** — no user text ever reaches the SQL, so
//! there is no injection surface. Keyboard nav (↑/↓/Enter) rides the field's opt-in
//! `emit_nav`/`emit_tab` events; a click picks directly.

use flint::prelude::*;
use gpui::{
    canvas, div, point, prelude::*, px, Anchor, AnyElement, Bounds, Context, Entity, Pixels,
    ScrollHandle, SharedString, Window,
};
use red_core::{ColumnMeta, LookupRow, TableRef, Value};
use red_service::{Command, SessionId};

use super::edit::EditSlot;
use crate::app::{AppState, Phase};

/// How many referenced-table rows the picker fetches. The list is distinct + capped;
/// the user filters within it client-side, or types an id outside it directly.
const LOOKUP_LIMIT: usize = 500;

/// The open in-cell FK picker: the fetched id/label rows for one FK target, plus the
/// live filter query and highlighted row. Single-instance app state (one editor open),
/// held on [`AppState::cell_suggest`] beside the editor it decorates.
pub(crate) struct CellSuggest {
    /// The result epoch the editor belongs to (drops a stale `LookupReady`).
    pub(crate) epoch: u64,
    /// The referenced `(schema, table)` — the cache key and `LookupReady` match key.
    /// `schema` is kept raw (possibly empty) so it round-trips through the command.
    pub(crate) target: (String, String),
    /// The fetched rows (from cache or the reply); empty while `loading`.
    pub(crate) items: Vec<LookupRow>,
    /// Indices into `items` that match the current query, in list order.
    pub(crate) filtered: Vec<usize>,
    /// The highlighted position *within `filtered`* (↑/↓/hover), or `None` — then
    /// Enter commits the typed text rather than a suggestion.
    pub(crate) selected: Option<usize>,
    /// The current filter text (mirrors the editor field).
    pub(crate) query: String,
    /// Whether the id list is still in flight (shows a "Loading…" row).
    pub(crate) loading: bool,
    /// `true` for an FK picker (fed by `LookupReady`), `false` for an enum picker (fed
    /// synchronously from the enum cache). Guards `on_lookup_ready` from overwriting an
    /// enum picker if a same-epoch FK reply for a colliding target ever arrives.
    pub(crate) from_lookup: bool,
    /// Scroll position of the dropdown list, so ↑/↓ can scroll the highlight into view
    /// (the wheel rides this handle too, keeping the scroll inside the popup).
    pub(crate) scroll: ScrollHandle,
}

impl CellSuggest {
    /// Recompute [`filtered`](Self::filtered) for `query`: a case-insensitive substring
    /// match over each row's display string. Auto-highlights the top match (so Enter/Tab
    /// accepts it, IDE-completion style) and resets the list scroll to the top.
    fn recompute(&mut self, query: &str) {
        let q = query.trim().to_ascii_lowercase();
        self.query = query.to_string();
        self.filtered = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, r)| q.is_empty() || display(r).to_ascii_lowercase().contains(&q))
            .map(|(i, _)| i)
            .collect();
        self.selected = (!self.filtered.is_empty()).then_some(0);
        self.scroll.set_offset(point(px(0.), px(0.)));
    }

    /// The `LookupRow` at the highlighted position, if any.
    fn highlighted(&self) -> Option<&LookupRow> {
        let i = *self.filtered.get(self.selected?)?;
        self.items.get(i)
    }
}

/// One row's display string: `"<id> — <label>"`, or just the id when there's no label.
fn display(row: &LookupRow) -> String {
    match &row.label {
        Some(l) if !matches!(l, Value::Null) => format!("{} — {}", row.id, l),
        _ => row.id.to_string(),
    }
}

/// The referenced table + columns to fetch for an FK cell, resolved from the FK graph.
struct LookupSpec {
    target: (String, String),
    id_column: String,
    label_column: Option<String>,
}

impl AppState {
    /// Resolve the FK lookup target for the editor's `data_col`, or `None` when the
    /// column isn't a single-column foreign key of the browse's base table. Independent
    /// of the cursor (works for a draft row's cell too), mirroring [`reference_menu`].
    fn fk_lookup_spec(&self, data_col: usize) -> Option<LookupSpec> {
        let Phase::Connected(active) = &self.phase else {
            return None;
        };
        let grid = active.active_result()?;
        let (schema, table) = grid.table.as_ref()?;
        let cname = grid.columns().get(data_col)?.name.clone();
        let edge = active.fk_graph.iter().find(|e| {
            e.columns.len() == 1
                && e.from_table == *table
                && e.from_schema.as_deref() == Some(schema.as_str())
                && e.columns[0].0 == cname
        })?;
        let ref_schema = edge.to_schema.clone().unwrap_or_else(|| schema.clone());
        let ref_table = edge.to_table.clone();
        let id_column = edge.columns[0].1.clone();
        // The label rides along only if the referenced table's detail is prefetched;
        // otherwise the picker shows bare ids (still selectable/searchable).
        let label_column = active
            .schema
            .details
            .get(&(ref_schema.clone(), ref_table.clone()))
            .and_then(|d| pick_label_column(&d.columns, &id_column));
        Some(LookupSpec {
            target: (ref_schema, ref_table),
            id_column,
            label_column,
        })
    }

    /// Open (or clear) the in-cell picker for the editor now targeting `data_col`: a
    /// foreign-key id list (fetched/cached per target) if the column is an FK, else an
    /// enum value list (from the per-table enum cache) if the column is an enum, else
    /// nothing. For a not-yet-loaded enum table it triggers the one-time load and shows
    /// the picker when [`on_enums_loaded`] arrives. Called by `open_cell_editor` for
    /// every cell.
    pub(crate) fn open_cell_suggest(
        &mut self,
        epoch: u64,
        data_col: usize,
        cx: &mut Context<Self>,
    ) {
        // The current field text seeds the filter so the list reflects the cell.
        let seed = self
            .grid_edit
            .as_ref()
            .map(|e| e.input.read(cx).content().to_string())
            .unwrap_or_default();

        // 1) Foreign key: a searchable id/label list, fetched + cached per target.
        if let Some(spec) = self.fk_lookup_spec(data_col) {
            let cached = match &self.phase {
                Phase::Connected(active) => active.lookup_cache.get(&spec.target).cloned(),
                _ => None,
            };
            let loading = cached.is_none();
            let mut suggest = CellSuggest {
                epoch,
                target: spec.target.clone(),
                items: cached.unwrap_or_default(),
                filtered: Vec::new(),
                selected: None,
                query: String::new(),
                loading,
                from_lookup: true,
                scroll: ScrollHandle::new(),
            };
            suggest.recompute(&seed);
            self.cell_suggest = Some(suggest);
            self.cell_suggest_bounds.update(cx, |b, _| *b = None);
            if loading {
                self.request_lookup(spec, epoch);
            }
            cx.notify();
            return;
        }

        // 2) Enum: the column's allowed values, straight from the per-table cache.
        if let Some(values) = self.enum_values_for(data_col) {
            let items = values
                .into_iter()
                .map(|v| LookupRow {
                    id: Value::Text(v),
                    label: None,
                })
                .collect();
            let mut suggest = CellSuggest {
                epoch,
                target: (String::new(), String::new()),
                items,
                filtered: Vec::new(),
                selected: None,
                query: String::new(),
                loading: false,
                from_lookup: false,
                scroll: ScrollHandle::new(),
            };
            suggest.recompute(&seed);
            self.cell_suggest = Some(suggest);
            self.cell_suggest_bounds.update(cx, |b, _| *b = None);
            cx.notify();
            return;
        }

        // 3) Neither (yet): no picker, but make sure the table's enums are loading so a
        // re-check on `EnumsLoaded` can surface an enum column's values.
        self.cell_suggest = None;
        self.ensure_enums_requested();
        cx.notify();
    }

    /// The enum values for the editor's `data_col`, if the browse's base table has them
    /// cached and that column is an enum. `None` otherwise (not an enum, or not loaded).
    fn enum_values_for(&self, data_col: usize) -> Option<Vec<String>> {
        let Phase::Connected(active) = &self.phase else {
            return None;
        };
        let grid = active.active_result()?;
        let (schema, table) = grid.table.as_ref()?;
        let cname = grid.columns().get(data_col)?.name.clone();
        let values = active
            .enum_cache
            .get(&(schema.clone(), table.clone()))?
            .get(&cname)?;
        (!values.is_empty()).then(|| values.clone())
    }

    /// Request the browse's base-table enum columns once (idempotent per table), so a
    /// later edit of an enum cell can show the value picker.
    fn ensure_enums_requested(&mut self) {
        let table = match &self.phase {
            Phase::Connected(active) => active.active_result().and_then(|g| g.table.clone()),
            _ => None,
        };
        let Some((schema, name)) = table else {
            return;
        };
        let key = (schema.clone(), name.clone());
        let need = if let Phase::Connected(active) = &mut self.phase {
            !active.enum_cache.contains_key(&key) && active.enum_requested.insert(key)
        } else {
            false
        };
        if need {
            self.send_active(Command::LoadEnums {
                table: TableRef {
                    schema: (!schema.is_empty()).then_some(schema),
                    name,
                },
            });
        }
    }

    /// An `EnumsLoaded` reply: cache the table's enum columns, then, if an editor is open
    /// without a picker, re-resolve it (the just-loaded values may now surface one).
    pub(crate) fn on_enums_loaded(
        &mut self,
        session: Option<SessionId>,
        table: TableRef,
        columns: std::collections::HashMap<String, Vec<String>>,
        cx: &mut Context<Self>,
    ) {
        let key = (table.schema.unwrap_or_default(), table.name);
        if let Some(active) = self.conn_mut(session) {
            active.enum_cache.insert(key, columns);
        }
        if self.cell_suggest.is_none() {
            if let Some((epoch, data_col)) = self.grid_edit.as_ref().map(|e| {
                let col = match &e.slot {
                    EditSlot::Row { data_col, .. } | EditSlot::Draft { data_col, .. } => *data_col,
                };
                (e.epoch, col)
            }) {
                self.open_cell_suggest(epoch, data_col, cx);
            }
        }
        cx.notify();
    }

    /// Send the `FetchLookup` for `spec` (an FK target not yet cached).
    fn request_lookup(&self, spec: LookupSpec, epoch: u64) {
        let (schema, name) = spec.target;
        self.send_active(Command::FetchLookup {
            epoch,
            target: TableRef {
                schema: (!schema.is_empty()).then_some(schema),
                name,
            },
            id_column: spec.id_column,
            label_column: spec.label_column,
            limit: LOOKUP_LIMIT,
        });
    }

    /// A `LookupReady` reply: cache the rows on the connection and, if the open picker
    /// is still waiting for this target, fill and re-filter it. A stale epoch/target is
    /// cached but doesn't disturb the current editor.
    pub(crate) fn on_lookup_ready(
        &mut self,
        session: Option<SessionId>,
        epoch: u64,
        target: TableRef,
        rows: Vec<LookupRow>,
        cx: &mut Context<Self>,
    ) {
        let key = (target.schema.unwrap_or_default(), target.name);
        // Cache on the reply's own connection (it may be a background session).
        if let Some(active) = self.conn_mut(session) {
            active.lookup_cache.insert(key.clone(), rows.clone());
        }
        // The epoch is process-unique, so an epoch+target match is unambiguously this
        // editor's picker regardless of which session replied.
        if let Some(s) = &mut self.cell_suggest {
            if s.from_lookup && s.epoch == epoch && s.target == key {
                s.items = rows;
                s.loading = false;
                let q = s.query.clone();
                s.recompute(&q);
            }
        }
        cx.notify();
    }

    /// A `LookupFailed` reply: just stop the loading spinner for the open picker (the
    /// user types the id). Pane-scoped, no toast.
    pub(crate) fn on_lookup_failed(
        &mut self,
        epoch: u64,
        target: TableRef,
        cx: &mut Context<Self>,
    ) {
        let key = (target.schema.unwrap_or_default(), target.name);
        if let Some(s) = &mut self.cell_suggest {
            if s.epoch == epoch && s.target == key {
                s.loading = false;
            }
        }
        cx.notify();
    }

    /// Re-filter the picker from the editor's current text (the field emitted `Change`).
    pub(crate) fn on_grid_edit_change(&mut self, cx: &mut Context<Self>) {
        if self.cell_suggest.is_none() {
            return;
        }
        let Some(text) = self
            .grid_edit
            .as_ref()
            .map(|e| e.input.read(cx).content().to_string())
        else {
            return;
        };
        if let Some(s) = &mut self.cell_suggest {
            s.recompute(&text);
        }
        cx.notify();
    }

    /// Move the picker's highlight by `delta` (↑/↓), wrapping; from no selection, Down
    /// lands on the first row and Up on the last.
    pub(crate) fn suggest_move(&mut self, delta: i32, cx: &mut Context<Self>) {
        if let Some(s) = &mut self.cell_suggest {
            let n = s.filtered.len();
            if n == 0 {
                return;
            }
            let next = match s.selected {
                None => {
                    if delta >= 0 {
                        0
                    } else {
                        n - 1
                    }
                }
                Some(c) => (c as i32 + delta).rem_euclid(n as i32) as usize,
            };
            s.selected = Some(next);
            s.scroll.scroll_to_item(next);
            cx.notify();
        }
    }

    /// The highlighted suggestion's id value, when one is highlighted — the value a
    /// commit/advance writes instead of coercing the typed text.
    pub(crate) fn suggest_selected_value(&self) -> Option<Value> {
        Some(self.cell_suggest.as_ref()?.highlighted()?.id.clone())
    }

    /// Esc from the cell editor: if the suggestion list is open, close *just the list*
    /// (so the typed value can then be committed as-is), IDE-completion style; otherwise
    /// cancel the whole edit.
    pub(crate) fn suggest_escape_or_cancel(&mut self, cx: &mut Context<Self>) {
        if self.cell_suggest.take().is_some() {
            cx.notify();
        } else {
            self.cancel_grid_edit(cx);
        }
    }

    /// Pick the suggestion at list position `pos` (a click): highlight it, then commit
    /// the editor, which reads it back through [`suggest_selected_value`]. The list is
    /// non-focusable, so the click doesn't blur the field before this runs.
    pub(crate) fn accept_suggest(&mut self, pos: usize, cx: &mut Context<Self>) {
        if let Some(s) = &mut self.cell_suggest {
            s.selected = Some(pos);
        }
        self.commit_grid_edit(cx);
    }

    /// The floating suggestion dropdown, anchored to the editor cell (mounted at the app
    /// root, over every overlay). `None` unless the picker is open, its anchor rect is
    /// known, and there's something to show. `window` is used to flip the list *above*
    /// the cell when there isn't room below (a draft row at the window's bottom edge).
    pub(crate) fn render_cell_suggest(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let s = self.cell_suggest.as_ref()?;
        let bounds: Bounds<Pixels> = (*self.cell_suggest_bounds.read(cx))?;
        if s.filtered.is_empty() && !s.loading {
            return None;
        }
        let theme = cx.theme();
        let (bg, border, text, dim, sel, hover) = (
            theme.bg_elevated,
            theme.border,
            theme.text,
            theme.text_dim,
            theme.bg_selected,
            theme.bg_hover,
        );
        let size = theme.font_size;

        let mut list = div()
            .id("cell-suggest-list")
            .max_h(px(240.))
            .overflow_y_scroll()
            // The wheel rides this handle (and ↑/↓ scroll the highlight into view), so
            // the list scrolls internally instead of the grid behind it.
            .track_scroll(&s.scroll)
            .text_size(size)
            .text_color(text);
        if s.loading && s.items.is_empty() {
            list = list.child(div().px_2p5().py_1p5().text_color(dim).child("Loading…"));
        } else {
            for (pos, &item_idx) in s.filtered.iter().enumerate() {
                let Some(row) = s.items.get(item_idx) else {
                    continue;
                };
                let label: SharedString = display(row).into();
                let selected = s.selected == Some(pos);
                list = list.child(
                    div()
                        .id(("cell-suggest-item", pos))
                        .px_2p5()
                        .py_1p5()
                        .cursor_pointer()
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .when(selected, |d| d.bg(sel))
                        .when(!selected, |d| d.hover(move |s| s.bg(hover)))
                        .child(label)
                        .on_click(cx.listener(move |this, _, _, cx| this.accept_suggest(pos, cx))),
                );
            }
        }

        let panel = div()
            .occlude()
            // Swallow any wheel the list didn't consume so it never reaches (and
            // scrolls) the data grid behind the popup.
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .w(px(320.))
            .bg(bg)
            .border_1()
            .border_color(border)
            .rounded_md()
            .shadow_lg()
            .py_1()
            .child(list);

        // Prefer below the cell; flip above when the list would run off the window's
        // bottom (covering the very cell being edited — e.g. a draft row at the edge).
        let below = bounds.bottom_left();
        let list_h = px(252.); // max_h(240) + chrome; an estimate for the flip decision
        let gap = px(3.);
        let float = if below.y + list_h + gap > window.viewport_size().height {
            floating(panel)
                .anchor(Anchor::BottomLeft)
                .at(bounds.origin) // top-left of the cell
                .offset(point(px(0.), -gap))
        } else {
            floating(panel).at(below).offset(point(px(0.), gap))
        };
        Some(float.into_any_element())
    }
}

/// A bounds-capturing overlay for the editor cell: records the cell's window rect into
/// `anchor` each frame so [`render_cell_suggest`](AppState::render_cell_suggest) can
/// place the dropdown below it (the ComboBox/CodeEditor `canvas` trick). Sized to fill
/// its (relative) parent; paints nothing.
pub(crate) fn anchor_canvas(anchor: Entity<Option<Bounds<Pixels>>>) -> impl IntoElement {
    canvas(
        move |_, _, _| (),
        move |bounds, _, _, cx| {
            anchor.update(cx, |stored, cx| {
                if *stored != Some(bounds) {
                    *stored = Some(bounds);
                    cx.notify();
                }
            });
        },
    )
    .absolute()
    .size_full()
}

/// Pick a human-readable label column of the referenced table: a well-known name
/// (`name`/`title`/…) first, else the first text-ish non-id, non-PK column, else none.
fn pick_label_column(columns: &[ColumnMeta], id_column: &str) -> Option<String> {
    const PREFERRED: &[&str] = &[
        "name",
        "title",
        "label",
        "display_name",
        "full_name",
        "username",
        "email",
        "description",
        "slug",
        "code",
    ];
    for pref in PREFERRED {
        if let Some(c) = columns
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(pref) && !c.name.eq_ignore_ascii_case(id_column))
        {
            return Some(c.name.clone());
        }
    }
    columns
        .iter()
        .find(|c| {
            !c.name.eq_ignore_ascii_case(id_column)
                && !c.primary_key
                && c.type_name.as_deref().is_some_and(is_texty)
        })
        .map(|c| c.name.clone())
}

/// Whether a declared type looks like text (a reasonable label column).
fn is_texty(t: &str) -> bool {
    let t = t.to_ascii_lowercase();
    ["char", "text", "string", "clob", "citext"]
        .iter()
        .any(|k| t.contains(k))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, ty: &str, pk: bool) -> ColumnMeta {
        ColumnMeta {
            name: name.to_string(),
            type_name: Some(ty.to_string()),
            not_null: false,
            primary_key: pk,
            default: None,
            auto_increment: false,
        }
    }

    #[test]
    fn label_prefers_well_known_name_over_first_text() {
        let cols = vec![
            col("id", "integer", true),
            col("slug", "varchar", false),
            col("name", "text", false),
        ];
        // "name" wins even though "slug" is an earlier text column.
        assert_eq!(pick_label_column(&cols, "id").as_deref(), Some("name"));
    }

    #[test]
    fn label_falls_back_to_first_text_non_id() {
        let cols = vec![
            col("id", "integer", true),
            col("created_at", "timestamp", false),
            col("title_line", "varchar", false),
        ];
        assert_eq!(
            pick_label_column(&cols, "id").as_deref(),
            Some("title_line")
        );
    }

    #[test]
    fn label_none_when_only_id_and_non_text() {
        let cols = vec![col("id", "integer", true), col("qty", "integer", false)];
        assert_eq!(pick_label_column(&cols, "id"), None);
    }

    #[test]
    fn display_joins_id_and_label_else_bare_id() {
        let row = LookupRow {
            id: Value::Integer(7),
            label: Some(Value::Text("Alice".into())),
        };
        assert_eq!(display(&row), "7 — Alice");
        let bare = LookupRow {
            id: Value::Integer(7),
            label: None,
        };
        assert_eq!(display(&bare), "7");
        // A NULL label collapses to just the id.
        let nulled = LookupRow {
            id: Value::Integer(7),
            label: Some(Value::Null),
        };
        assert_eq!(display(&nulled), "7");
    }

    #[test]
    fn recompute_filters_case_insensitively_over_id_and_label() {
        let mut s = CellSuggest {
            epoch: 1,
            target: ("s".into(), "t".into()),
            items: vec![
                LookupRow {
                    id: Value::Integer(1),
                    label: Some(Value::Text("Alice".into())),
                },
                LookupRow {
                    id: Value::Integer(2),
                    label: Some(Value::Text("Bob".into())),
                },
            ],
            filtered: Vec::new(),
            selected: None,
            query: String::new(),
            loading: false,
            from_lookup: true,
            scroll: ScrollHandle::new(),
        };
        s.recompute("ali");
        assert_eq!(s.filtered, vec![0]); // matches "Alice" (case-insensitive)
        s.recompute("2");
        assert_eq!(s.filtered, vec![1]); // matches the id
        s.recompute("");
        assert_eq!(s.filtered, vec![0, 1]); // empty query shows all
    }
}
