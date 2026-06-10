//! Result-cell rendering and the results-pane view: colors cells by value kind
//! (numbers accented, UUIDs dimmed, JSON-ish text cyan) and assembles the
//! toolbar · grid · footer · scrollbar that make up the pane.

use flint::prelude::*;
use gpui::{div, point, prelude::*, px, Hsla};
use red_core::{ExportFormat, Value};

use super::buffer::WINDOW;
use crate::app::{ActiveConn, AppState};
use crate::assets::FONT_MONO;

/// Group a number's digits in threes (`1234567` → `1,234,567`) so large row
/// numbers and totals read at a glance.
pub(crate) fn group_digits(n: usize) -> String {
    let digits = n.to_string();
    let bytes = digits.as_bytes();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

/// A query duration as a compact label: sub-second in milliseconds, otherwise
/// seconds with two decimals (`842 ms`, `1.27 s`).
fn format_duration(d: std::time::Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        format!("{ms} ms")
    } else {
        format!("{:.2} s", d.as_secs_f64())
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

        let grid = match &active.active().result {
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

        let elapsed = format_duration(grid.query_time());

        // A failed query gets a full-pane panel rather than the cramped toolbar
        // status slot — syntax errors are multi-line and would otherwise clip.
        if let Some(err) = &grid.error {
            return container.child(
                div()
                    .id("result-error")
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_4()
                    .overflow_y_scroll()
                    .font_family(FONT_MONO)
                    .child(
                        div()
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .gap_2()
                            .text_size(px(11.))
                            .text_color(red)
                            .child("Query failed")
                            .child(div().text_color(faint).child(format!("· {elapsed}"))),
                    )
                    .child(div().text_size(px(12.)).text_color(text).child(err.clone())),
            );
        }

        let status = if !grid.ready {
            div().text_color(faint).child(format!("running… {elapsed}"))
        } else {
            div()
                .text_color(faint)
                .child(format!("{} rows · {elapsed}", grid.total))
        };
        let view = cx.entity().downgrade();
        let toolbar = div()
            .flex_shrink_0()
            // No fixed height: the 24px buttons define the strip and the equal
            // padding brackets them evenly. A fixed height taller than the
            // buttons left slack that GPUI distributed unevenly, sinking the
            // buttons off-center.
            .py(px(3.))
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
        let epoch = grid.epoch;
        let (sort_view, cell_view) = (view.clone(), view.clone());

        // Resolve (and possibly re-center) the virtual-scroll window for this
        // frame; everything below works in list-local coordinates offset by
        // `base`, so the list only ever lays out `win.len` rows.
        let row_height = self.settings.density().row_height();
        let win = grid.prepare_window(row_height);
        let base = win.base;
        // The selection is stored in absolute ordinals; translate it into the
        // window's local rows for highlighting (off-window rows just aren't
        // painted). The TSV copy reads the buffer in absolute space, so it stays
        // correct regardless.
        let local_selection = grid.selection.map(|mut r| {
            r.anchor.0 = r.anchor.0.saturating_sub(base);
            r.focus.0 = r.focus.0.saturating_sub(base);
            r
        });

        let table = Table::<()>::new("result-grid", columns)
            .row_count(win.len)
            .row_height(row_height)
            .font_family(FONT_MONO)
            .grid_lines(true)
            .track_scroll(&grid.scroll)
            .track_horizontal_scroll(&grid.h_scroll)
            .horizontal(true)
            .selected_cells(local_selection)
            .sort(sort)
            .sort_carets(
                move || crate::icons::icon("sort-asc", px(9.), accent).into_any_element(),
                move || crate::icons::icon("sort-desc", px(9.), accent).into_any_element(),
            )
            .on_visible_range(move |range, window, _| {
                // `range` is list-local; the buffer is keyed by absolute ordinal.
                let abs = (base + range.start)..(base + range.end);
                let settled = buffer_range.borrow_mut().ensure(abs, total, epoch, &sender);
                // Mid-fling we skipped fetching; ask for another paint so the
                // window that the scroll settles on still gets loaded.
                if !settled {
                    window.refresh();
                }
            })
            .on_sort(move |table_col, _, cx| {
                sort_view
                    .update(cx, |this, cx| this.result_sort(table_col, cx))
                    .ok();
            })
            .on_cell_click(move |row, table_col, event, _, cx| {
                let extend = event.modifiers().shift;
                let abs_row = base + row;
                cell_view
                    .update(cx, |this, cx| {
                        this.result_select(abs_row, table_col, extend, cx)
                    })
                    .ok();
            })
            .render_row(move |ix, _, _| {
                // `ix` is list-local; the gutter and buffer are absolute.
                let abs = base + ix;
                let mut out = Vec::with_capacity(ncols + 1);
                let buffer = buffer_row.borrow();
                // After an interpolated jump the run's ordinals are estimates;
                // the gutter marks them `≈` until a true end pins them exact.
                let gutter = if buffer.is_estimated() {
                    format!("≈{}", group_digits(abs + 1))
                } else {
                    group_digits(abs + 1)
                };
                out.push(div().text_color(faint).child(gutter).into_any_element());
                match buffer.row(abs) {
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
            .child(div().text_color(border_soft).child("·"))
            // Which paging mode this result got (keyset = seek key resolved;
            // offset = the O(offset) fallback) — the at-a-glance diagnostic.
            .child(
                div()
                    .text_color(dim)
                    .child(if grid.buffer.borrow().is_keyed() {
                        "keyset"
                    } else {
                        "offset"
                    }),
            )
            .child(div().ml_auto().text_color(dim).child(grid.label.clone()));

        // The draggable, fraction-mapped scrollbar: the thumb mirrors the list's
        // position; a scrub jumps the viewport, and the buffer's `ensure` turns
        // the far jump into one key-space seek (keyed results) or one OFFSET page
        // (fallback).
        let scrub_scroll = grid.scroll.clone();
        let scrub_window = grid.window_base.clone();
        let scrub_view = view.clone();
        let rh = f32::from(row_height);
        let scrollbar = Scrollbar::new("result-scrollbar", &grid.scrollbar)
            // Position is computed over the whole result (not the f32-bounded
            // window the list lays out), so the thumb is honest at 50M rows.
            .fraction(win.fraction)
            .thumb(win.thumb)
            .on_scrub(move |fraction, _, cx| {
                let target = (fraction as f64 * total.saturating_sub(1) as f64).round() as usize;
                // Re-center the window on the target, then place it at the top of
                // the viewport by setting the list's pixel offset directly — no
                // `scroll_to_item` into a multi-million-row (f32-degenerate)
                // canvas.
                let base = if total > WINDOW {
                    target.saturating_sub(WINDOW / 2).min(total - WINDOW)
                } else {
                    0
                };
                scrub_window.set(base);
                let local = target - base;
                let st = scrub_scroll.0.borrow();
                let x = st.base_handle.offset().x;
                st.base_handle
                    .set_offset(point(x, px(-(local as f32 * rh))));
                drop(st);
                scrub_view.update(cx, |_, cx| cx.notify()).ok();
            });

        container
            .child(toolbar)
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .bg(bg_app)
                    .relative()
                    .child(table)
                    .child(scrollbar),
            )
            .child(footer)
    }
}
