// SPDX-License-Identifier: GPL-3.0-or-later

//! The result grid (M5): a virtualized, horizontally-scrolling table backed by a
//! random-access window buffer. The grid never holds the whole result — its
//! load-on-scroll callback fetches the pages around the viewport and evicts the
//! rest, so memory stays flat over a multi-million-row result. Cell ranges select
//! and copy as TSV; clicking a column header sorts (re-running with `ORDER BY`).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;

use std::path::PathBuf;

use flint::prelude::*;
use gpui::{div, prelude::*, px, ClipboardItem, Context, Hsla, UniformListScrollHandle};
use red_core::{Column as ResultColumn, ExportFormat, Value};
use red_service::{Command, CommandSender};

use crate::app::{ActiveConn, AppState, Phase};
use crate::assets::FONT_MONO;

/// Rows per fetched page, and how far beyond the viewport to keep resident before
/// evicting. The buffer holds at most ~`2*MARGIN` rows regardless of total.
const PAGE: usize = 200;
const MARGIN: usize = 400;

/// The sparse row buffer: absolute row index → cells, plus the set of in-flight
/// page requests (so the same page isn't fetched twice).
#[derive(Default)]
struct GridBuffer {
    rows: HashMap<usize, Vec<Value>>,
    requested: HashSet<usize>,
}

impl GridBuffer {
    /// Drop a freshly-arrived page in and clear its in-flight mark.
    fn insert_page(&mut self, offset: usize, rows: Vec<Vec<Value>>) {
        self.requested.remove(&(offset / PAGE));
        for (i, row) in rows.into_iter().enumerate() {
            self.rows.insert(offset + i, row);
        }
    }

    /// Ensure the pages covering `range` are loaded, evict everything beyond a
    /// margin around it, and request any missing pages. Called per paint with the
    /// about-to-render window, so the buffer tracks the viewport.
    fn ensure(&mut self, range: Range<usize>, total: usize, sender: &CommandSender) {
        let lo = range.start.saturating_sub(MARGIN);
        let hi = (range.end + MARGIN).min(total);
        self.rows.retain(|k, _| *k >= lo && *k < hi);
        self.requested
            .retain(|p| p * PAGE + PAGE > lo && p * PAGE < hi);

        if range.is_empty() {
            return;
        }
        let first = range.start / PAGE;
        let last = (range.end - 1) / PAGE;
        for page in first..=last {
            let offset = page * PAGE;
            if offset >= total || self.requested.contains(&page) {
                continue;
            }
            let end = (offset + PAGE).min(total);
            if (offset..end).all(|i| self.rows.contains_key(&i)) {
                continue;
            }
            self.requested.insert(page);
            sender.send(Command::FetchPage {
                offset,
                limit: PAGE,
            });
        }
    }
}

/// All the state for one open result. The grid columns include a leading row-
/// number gutter, so table-column index `0` is the gutter and data column `n` is
/// table column `n + 1`.
pub(crate) struct ResultGrid {
    pub label: String,
    base_sql: String,
    columns: Vec<ResultColumn>,
    total: usize,
    ready: bool,
    error: Option<String>,
    /// `(data column, ascending)` — `None` is unsorted.
    sort: Option<(usize, bool)>,
    selection: Option<CellRange>,
    buffer: Rc<RefCell<GridBuffer>>,
    sender: CommandSender,
    scroll: UniformListScrollHandle,
}

impl ResultGrid {
    pub fn new(label: String, base_sql: String, sender: CommandSender) -> Self {
        Self {
            label,
            base_sql,
            columns: Vec::new(),
            total: 0,
            ready: false,
            error: None,
            sort: None,
            selection: None,
            buffer: Rc::new(RefCell::new(GridBuffer::default())),
            sender,
            scroll: UniformListScrollHandle::new(),
        }
    }

    /// The SQL to open: the base query, wrapped with `ORDER BY <pos>` when sorted.
    /// Ordering by output position is robust to column-name quoting/aliases.
    fn effective_sql(&self) -> String {
        match self.sort {
            Some((col, asc)) => format!(
                "SELECT * FROM ({}) ORDER BY {} {}",
                strip_trailing(&self.base_sql),
                col + 1,
                if asc { "ASC" } else { "DESC" }
            ),
            None => self.base_sql.clone(),
        }
    }

    /// `(rows, columns)` once the result is ready — for the shell's status bar.
    pub fn status_counts(&self) -> Option<(usize, usize)> {
        self.ready
            .then_some((self.total, self.columns.len()))
            .filter(|_| self.error.is_none())
    }

    fn on_ready(&mut self, columns: Vec<ResultColumn>, total: usize) {
        self.columns = columns;
        self.total = total;
        self.ready = true;
        self.error = None;
    }

    fn reset_buffer(&mut self) {
        let mut buffer = self.buffer.borrow_mut();
        buffer.rows.clear();
        buffer.requested.clear();
    }

    /// The current selection as TSV (NULL → empty), skipping the gutter column.
    /// Unloaded cells contribute blanks rather than blocking the copy.
    fn selection_tsv(&self) -> Option<String> {
        let (r0, c0, r1, c1) = self.selection?.bounds();
        let ncol = self.columns.len();
        let dc0 = c0.max(1);
        if dc0 > c1 {
            return None;
        }
        let buffer = self.buffer.borrow();
        let mut out = String::new();
        for r in r0..=r1 {
            for (i, tc) in (dc0..=c1).enumerate() {
                let dcol = tc - 1;
                if dcol >= ncol {
                    continue;
                }
                if i > 0 {
                    out.push('\t');
                }
                if let Some(value) = buffer.rows.get(&r).and_then(|row| row.get(dcol)) {
                    out.push_str(&cell_string(value));
                }
            }
            out.push('\n');
        }
        Some(out)
    }
}

/// Trim trailing whitespace + a single terminator, so the SQL nests as a subquery.
fn strip_trailing(sql: &str) -> &str {
    sql.trim().strip_suffix(';').unwrap_or(sql.trim()).trim()
}

/// A value as a plain TSV/clipboard string (NULL → empty).
fn cell_string(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Integer(n) => n.to_string(),
        Value::Real(x) => x.to_string(),
        Value::Text(s) => s.clone(),
        Value::Blob(b) => format!("<{} bytes>", b.len()),
    }
}

/// Colors a result cell carries, keyed by value kind (so the grid reads at a
/// glance the way the design does: numbers orange, UUIDs dimmed, JSON cyan).
#[derive(Clone, Copy)]
struct CellColors {
    text: Hsla,
    muted: Hsla,
    num: Hsla,
    cyan: Hsla,
    faint: Hsla,
}

/// True for a canonical `8-4-4-4-12` hex UUID — dimmed like the design's id columns.
fn is_uuid(s: &str) -> bool {
    s.len() == 36
        && s.as_bytes().iter().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => *b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

/// One grid cell, colored by value kind (NULL italic-faint, numbers accented,
/// UUIDs dimmed, JSON-ish text cyan — mirroring the design's typed cells).
fn render_cell(value: Option<&Value>, c: CellColors) -> gpui::AnyElement {
    match value {
        None | Some(Value::Null) => div()
            .italic()
            .text_color(c.faint)
            .child("NULL")
            .into_any_element(),
        Some(Value::Integer(n)) => div()
            .text_color(c.num)
            .child(n.to_string())
            .into_any_element(),
        Some(Value::Real(x)) => div()
            .text_color(c.num)
            .child(x.to_string())
            .into_any_element(),
        Some(Value::Text(s)) => {
            let trimmed = s.trim_start();
            let color = if is_uuid(s) {
                c.muted
            } else if trimmed.starts_with('{') || trimmed.starts_with('[') {
                c.cyan
            } else {
                c.text
            };
            div().text_color(color).child(s.clone()).into_any_element()
        }
        Some(Value::Blob(b)) => div()
            .text_color(c.faint)
            .child(format!("<{} bytes>", b.len()))
            .into_any_element(),
    }
}

impl AppState {
    /// Open `base_sql` as the grid's result (preview or editor run). Resets sort +
    /// selection and asks the backend for the row count + columns.
    pub(crate) fn open_result(
        &mut self,
        label: impl Into<String>,
        base_sql: String,
        cx: &mut Context<Self>,
    ) {
        let sender = self.service.command_sender();
        let sql = match &mut self.phase {
            Phase::Connected(active) => {
                let grid = ResultGrid::new(label.into(), base_sql, sender);
                let sql = grid.effective_sql();
                active.result = Some(grid);
                sql
            }
            _ => return,
        };
        self.service.send(Command::OpenResult { sql });
        cx.notify();
    }

    /// Backend reported the open result's columns + total.
    pub(crate) fn on_result_ready(
        &mut self,
        columns: Vec<ResultColumn>,
        total: usize,
        cx: &mut Context<Self>,
    ) {
        if let Phase::Connected(active) = &mut self.phase {
            if let Some(grid) = &mut active.result {
                grid.reset_buffer();
                grid.on_ready(columns, total);
            }
        }
        cx.notify();
    }

    /// A page arrived — drop it into the buffer and repaint.
    pub(crate) fn on_result_page(
        &mut self,
        offset: usize,
        rows: Vec<Vec<Value>>,
        cx: &mut Context<Self>,
    ) {
        if let Phase::Connected(active) = &self.phase {
            if let Some(grid) = &active.result {
                grid.buffer.borrow_mut().insert_page(offset, rows);
            }
        }
        cx.notify();
    }

    /// Record a result error against the open grid (also surfaced as a toast).
    pub(crate) fn on_result_error(&mut self, message: &str) {
        if let Phase::Connected(active) = &mut self.phase {
            if let Some(grid) = &mut active.result {
                grid.error = Some(message.to_string());
                grid.ready = true;
            }
        }
    }

    /// Header click on a data column: toggle / set sort and re-open the result.
    pub(crate) fn result_sort(&mut self, table_col: usize, cx: &mut Context<Self>) {
        if table_col == 0 {
            return; // the row-number gutter isn't sortable
        }
        let dcol = table_col - 1;
        let sql = match &mut self.phase {
            Phase::Connected(active) => match &mut active.result {
                Some(grid) => {
                    grid.sort = match grid.sort {
                        Some((c, asc)) if c == dcol => Some((c, !asc)),
                        _ => Some((dcol, true)),
                    };
                    grid.selection = None;
                    grid.ready = false;
                    grid.reset_buffer();
                    Some(grid.effective_sql())
                }
                None => None,
            },
            _ => None,
        };
        if let Some(sql) = sql {
            self.service.send(Command::OpenResult { sql });
        }
        cx.notify();
    }

    /// Cell click: set the selection anchor, or extend it on shift-click.
    pub(crate) fn result_select(
        &mut self,
        row: usize,
        table_col: usize,
        extend: bool,
        cx: &mut Context<Self>,
    ) {
        if table_col == 0 {
            return;
        }
        if let Phase::Connected(active) = &mut self.phase {
            if let Some(grid) = &mut active.result {
                grid.selection = match (extend, grid.selection) {
                    (true, Some(mut range)) => {
                        range.focus = (row, table_col);
                        Some(range)
                    }
                    _ => Some(CellRange::single(row, table_col)),
                };
            }
        }
        cx.notify();
    }

    /// Prompt for a save path, then stream the open result there in `format`.
    pub(crate) fn export_result(&mut self, format: ExportFormat, cx: &mut Context<Self>) {
        let open = matches!(&self.phase, Phase::Connected(a) if a.result.is_some());
        if !open {
            return;
        }
        let name = match format {
            ExportFormat::Csv => "red-export.csv",
            ExportFormat::Json => "red-export.json",
        };
        let dir = dirs::download_dir()
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("."));
        let rx = cx.prompt_for_new_path(&dir, Some(name));
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(path))) = rx.await {
                this.update(cx, |this, cx| {
                    this.service.send(Command::Export { format, path });
                    this.toast = Some(("Exporting…".into(), ToastVariant::Info));
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    pub(crate) fn copy_result_selection(&mut self, cx: &mut Context<Self>) {
        let tsv = match &self.phase {
            Phase::Connected(active) => active.result.as_ref().and_then(ResultGrid::selection_tsv),
            _ => None,
        };
        if let Some(tsv) = tsv {
            cx.write_to_clipboard(ClipboardItem::new_string(tsv));
        }
    }

    /// The results pane: an empty state, an error, or the live windowed grid.
    pub(crate) fn render_result(
        &self,
        active: &ActiveConn,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let (bg, bg_app, border, border_soft) = (
            theme.bg_panel,
            theme.bg_app,
            theme.border,
            theme.border_soft,
        );
        let (muted, faint, dim, text) = (
            theme.text_muted,
            theme.text_faint,
            theme.text_dim,
            theme.text,
        );
        let (num, cyan, red, accent) = (theme.orange, theme.cyan, theme.red, theme.accent);
        let cell_colors = CellColors {
            text,
            muted,
            num,
            cyan,
            faint,
        };
        let container = div().size_full().flex().flex_col().bg(bg);

        let grid = match &active.result {
            Some(grid) => grid,
            None => {
                return container.child(
                    div()
                        .flex_1()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(12.))
                        .text_color(faint)
                        .child("Double-click a table or run a query to see rows"),
                );
            }
        };

        let status = if let Some(err) = &grid.error {
            div().text_color(red).child(err.clone())
        } else if !grid.ready {
            div().text_color(faint).child("running…".to_string())
        } else {
            div()
                .text_color(faint)
                .child(format!("{} rows", grid.total))
        };
        let view = cx.entity().downgrade();
        let toolbar = div()
            .flex_shrink_0()
            .h(px(26.))
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .border_b_1()
            .border_color(border)
            .font_family(FONT_MONO)
            .text_size(px(11.))
            .child(div().text_color(muted).child(grid.label.clone()))
            .child(status)
            .child(
                div()
                    .ml_auto()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(
                        Button::new("result-copy", "Copy")
                            .variant(ButtonVariant::Ghost)
                            .size(ButtonSize::Sm)
                            .on_click(cx.listener(|this, _, _, cx| this.copy_result_selection(cx))),
                    )
                    .child(
                        Button::new("result-csv", "CSV")
                            .variant(ButtonVariant::Ghost)
                            .size(ButtonSize::Sm)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.export_result(ExportFormat::Csv, cx)
                            })),
                    )
                    .child(
                        Button::new("result-json", "JSON")
                            .variant(ButtonVariant::Ghost)
                            .size(ButtonSize::Sm)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.export_result(ExportFormat::Json, cx)
                            })),
                    ),
            );

        if !grid.ready {
            return container.child(toolbar);
        }

        // Row-number gutter + one fixed-width, sortable column per result column.
        // Each header carries the engine's declared type as a dim subtitle, like
        // the design's typed headers (`email` + `text`).
        let mut columns = vec![Column::new("#").width(px(56.)).align_end()];
        for c in &grid.columns {
            let mut col = Column::new(c.name.clone()).width(px(180.)).sortable();
            if let Some(t) = &c.decl_type {
                if !t.is_empty() {
                    col = col.subtitle(t.to_lowercase());
                }
            }
            columns.push(col);
        }
        let sort = grid.sort.map(|(c, asc)| (c + 1, asc));
        let total = grid.total;
        let ncols = grid.columns.len();
        let buffer_range = grid.buffer.clone();
        let buffer_row = grid.buffer.clone();
        let sender = grid.sender.clone();
        let (sort_view, cell_view) = (view.clone(), view.clone());

        let table = Table::<()>::new("result-grid", columns)
            .row_count(total)
            .row_height(self.settings.density().row_height())
            .font_family(FONT_MONO)
            .grid_lines(true)
            .track_scroll(&grid.scroll)
            .horizontal(true)
            .selected_cells(grid.selection)
            .sort(sort)
            .sort_carets(
                move || crate::icons::icon("sort-asc", px(9.), accent).into_any_element(),
                move || crate::icons::icon("sort-desc", px(9.), accent).into_any_element(),
            )
            .on_visible_range(move |range, _, _| {
                buffer_range.borrow_mut().ensure(range, total, &sender)
            })
            .on_sort(move |table_col, _, cx| {
                sort_view
                    .update(cx, |this, cx| this.result_sort(table_col, cx))
                    .ok();
            })
            .on_cell_click(move |row, table_col, event, _, cx| {
                let extend = event.modifiers().shift;
                cell_view
                    .update(cx, |this, cx| {
                        this.result_select(row, table_col, extend, cx)
                    })
                    .ok();
            })
            .render_row(move |ix, _, _| {
                let mut out = Vec::with_capacity(ncols + 1);
                out.push(
                    div()
                        .text_color(faint)
                        .child((ix + 1).to_string())
                        .into_any_element(),
                );
                let buffer = buffer_row.borrow();
                match buffer.rows.get(&ix) {
                    Some(row) => {
                        for c in 0..ncols {
                            out.push(render_cell(row.get(c), cell_colors));
                        }
                    }
                    None => {
                        for _ in 0..ncols {
                            out.push(div().text_color(faint).child("·").into_any_element());
                        }
                    }
                }
                out
            });

        // Footer: a strong row count, the column count, and the result's label —
        // the design's "N rows · K columns" status strip under the grid.
        let footer = div()
            .flex_shrink_0()
            .h(px(28.))
            .flex()
            .items_center()
            .gap_2()
            .px_3p5()
            .bg(bg)
            .border_t_1()
            .border_color(border)
            .font_family(FONT_MONO)
            .text_size(px(11.))
            .child(div().text_color(text).child(format!("{}", grid.total)))
            .child(div().text_color(dim).child("rows"))
            .child(div().text_color(border_soft).child("·"))
            .child(div().text_color(dim).child(format!("{ncols} columns")))
            .child(div().ml_auto().text_color(dim).child(grid.label.clone()));

        container
            .child(toolbar)
            .child(div().flex_1().min_h(px(0.)).bg(bg_app).child(table))
            .child(footer)
    }
}
