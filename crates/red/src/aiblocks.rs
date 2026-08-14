//! Rich blocks in assistant markdown: a fenced ```` ```datatable ````, ```` ```barchart ````
//! or ```` ```stats ```` renders as a component instead of as a wall of JSON.
//!
//! The assistant already returns tables, distributions and headline numbers; until
//! now they arrived as prose or as a code block the reader had to parse by eye.
//! This is the presentation layer for them, and it is deliberately small: the model
//! emits a fenced block of plain JSON, and if that JSON does not parse the block
//! falls back to the ordinary code rendering. **Nothing the model wrote is ever
//! dropped** -- a rich block is an upgrade, never a filter.
//!
//! Not Mongo-specific: every markdown surface (the chat panel, the ACP transcript,
//! the knowledge editor) renders through `crate::markdown`, so all of them get this.

use flint::prelude::*;
use gpui::{AnyElement, div, prelude::*, px};
use serde_json::Value as Json;

/// The fence languages that render as components.
const DATATABLE: &str = "datatable";
const BARCHART: &str = "barchart";
const STATS: &str = "stats";

/// Rows a rendered datatable shows. A block is a summary; past this it is a result
/// grid, and the agent has tools that open one.
const MAX_ROWS: usize = 50;
/// Bars a chart shows, for the same reason.
const MAX_BARS: usize = 30;

/// Render a fenced block whose info string is `lang`, or `None` when that language
/// is not a rich block (or its body will not parse, which is the caller's cue to
/// show the code as written).
pub(crate) fn render(lang: &str, body: &str, theme: &Theme) -> Option<AnyElement> {
    let lang = lang.trim().to_ascii_lowercase();
    if !matches!(lang.as_str(), DATATABLE | BARCHART | STATS) {
        return None;
    }
    let value: Json = serde_json::from_str(body.trim()).ok()?;
    match lang.as_str() {
        DATATABLE => datatable(&value, theme),
        BARCHART => barchart(&value, theme),
        STATS => stats(&value, theme),
        _ => None,
    }
}

/// `{ "title"?, "columns": ["a"], "rows": [["1"]] }`
fn datatable(value: &Json, theme: &Theme) -> Option<AnyElement> {
    let columns: Vec<String> = value
        .get("columns")?
        .as_array()?
        .iter()
        .map(cell_text)
        .collect();
    let rows: Vec<Vec<String>> = value
        .get("rows")?
        .as_array()?
        .iter()
        .filter_map(|r| Some(r.as_array()?.iter().map(cell_text).collect()))
        .collect();
    if columns.is_empty() {
        return None;
    }

    let header = columns.iter().fold(
        div()
            .flex()
            .gap_2()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(theme.border)
            .text_color(theme.text_muted),
        |row, name| row.child(div().flex_1().min_w(px(0.)).truncate().child(name.clone())),
    );
    let shown = rows.len().min(MAX_ROWS);
    let body = rows.iter().take(shown).fold(
        div().flex().flex_col(),
        |list: gpui::Div, cells: &Vec<String>| {
            list.child(
                cells
                    .iter()
                    .fold(div().flex().gap_2().px_2().py(px(2.)), |row, cell| {
                        row.child(div().flex_1().min_w(px(0.)).truncate().child(cell.clone()))
                    }),
            )
        },
    );

    Some(
        block_frame(value, theme)
            .text_size(theme.scale(11.5))
            .child(header)
            .child(body)
            .when(rows.len() > shown, |d| {
                d.child(
                    div()
                        .px_2()
                        .py_1()
                        .text_color(theme.text_faint)
                        .child(format!("… {} more row(s)", rows.len() - shown)),
                )
            })
            .into_any_element(),
    )
}

/// `{ "title"?, "data": [{ "label": "a", "value": 3 }] }`
///
/// Bars are drawn as filled rows rather than through a charting library: the shape
/// a block like this carries is a comparison of a handful of magnitudes, which a
/// proportional bar says exactly and a plotting dependency would not say better.
fn barchart(value: &Json, theme: &Theme) -> Option<AnyElement> {
    let data: Vec<(String, f64)> = value
        .get("data")?
        .as_array()?
        .iter()
        .filter_map(|item| {
            let label = item.get("label").map(cell_text)?;
            let v = item.get("value")?;
            let n = v.as_f64().or_else(|| v.as_str()?.parse().ok())?;
            Some((label, n))
        })
        .collect();
    if data.is_empty() {
        return None;
    }
    // Scaled against the largest magnitude, so a set with negatives still reads.
    let peak = data
        .iter()
        .map(|(_, v)| v.abs())
        .fold(0.0_f64, f64::max)
        .max(f64::MIN_POSITIVE);

    let bars = data.iter().take(MAX_BARS).fold(
        div().flex().flex_col().gap_1().p_2(),
        |list, (label, v)| {
            let fraction = ((v.abs() / peak) as f32).clamp(0.0, 1.0);
            list.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .w(px(120.))
                            .flex_shrink_0()
                            .truncate()
                            .text_color(theme.text_muted)
                            .child(label.clone()),
                    )
                    .child(
                        div().flex_1().min_w(px(0.)).child(
                            div()
                                .h(px(10.))
                                .w(gpui::relative(fraction))
                                .rounded(px(2.))
                                .bg(if *v < 0.0 { theme.red } else { theme.accent }),
                        ),
                    )
                    .child(
                        div()
                            .w(px(90.))
                            .flex_shrink_0()
                            .text_color(theme.text)
                            .child(trim_number(*v)),
                    ),
            )
        },
    );
    Some(
        block_frame(value, theme)
            .text_size(theme.scale(11.5))
            .child(bars)
            .into_any_element(),
    )
}

/// `{ "title"?, "items": [{ "label": "rows", "value": 1200, "hint"? }] }`
fn stats(value: &Json, theme: &Theme) -> Option<AnyElement> {
    let items = value.get("items")?.as_array()?;
    if items.is_empty() {
        return None;
    }
    let tiles = items.iter().fold(
        div().flex().flex_wrap().gap_2().p_2(),
        |row: gpui::Div, item: &Json| {
            let label = item.get("label").map(cell_text).unwrap_or_default();
            let value = item.get("value").map(cell_text).unwrap_or_default();
            let hint = item.get("hint").map(cell_text).filter(|h| !h.is_empty());
            row.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(1.))
                    .px_3()
                    .py_2()
                    .min_w(px(120.))
                    .rounded(px(4.))
                    .bg(theme.bg_panel)
                    .border_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .text_size(theme.scale(10.5))
                            .text_color(theme.text_muted)
                            .child(label),
                    )
                    .child(
                        div()
                            .text_size(theme.scale(16.))
                            .text_color(theme.text)
                            .child(value),
                    )
                    .children(hint.map(|h| {
                        div()
                            .text_size(theme.scale(10.5))
                            .text_color(theme.text_faint)
                            .child(h)
                    })),
            )
        },
    );
    Some(block_frame(value, theme).child(tiles).into_any_element())
}

/// The bordered card every rich block sits in, with its optional title.
fn block_frame(value: &Json, theme: &Theme) -> gpui::Div {
    let title = value
        .get("title")
        .map(cell_text)
        .filter(|t| !t.trim().is_empty());
    div()
        .flex()
        .flex_col()
        .rounded(px(5.))
        .border_1()
        .border_color(theme.border)
        .bg(theme.bg_elevated)
        .children(title.map(|t| {
            div()
                .px_2()
                .py_1()
                .border_b_1()
                .border_color(theme.border)
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_size(theme.scale(12.))
                .text_color(theme.text)
                .child(t)
        }))
}

/// One JSON value as a cell: a string keeps its own text (no quotes), everything
/// else renders compactly.
fn cell_text(value: &Json) -> String {
    match value {
        Json::String(s) => s.clone(),
        Json::Null => String::new(),
        Json::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

/// A number without a trailing `.0`, so integer-valued data reads as integers.
fn trim_number(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v:.2}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether a body would render as a rich block, without needing a theme.
    fn recognized(lang: &str, body: &str) -> bool {
        let lang = lang.trim().to_ascii_lowercase();
        if !matches!(lang.as_str(), DATATABLE | BARCHART | STATS) {
            return false;
        }
        let Ok(value) = serde_json::from_str::<Json>(body.trim()) else {
            return false;
        };
        match lang.as_str() {
            DATATABLE => value.get("columns").and_then(Json::as_array).is_some(),
            BARCHART => value
                .get("data")
                .and_then(Json::as_array)
                .is_some_and(|d| !d.is_empty()),
            STATS => value
                .get("items")
                .and_then(Json::as_array)
                .is_some_and(|i| !i.is_empty()),
            _ => false,
        }
    }

    #[test]
    fn only_the_named_languages_are_rich() {
        assert!(recognized(
            "datatable",
            r#"{"columns":["a"],"rows":[["1"]]}"#
        ));
        assert!(recognized(
            "STATS",
            r#"{"items":[{"label":"n","value":1}]}"#
        ));
        assert!(recognized(
            "barchart",
            r#"{"data":[{"label":"a","value":1}]}"#
        ));
        // An ordinary code fence stays ordinary.
        assert!(!recognized("json", r#"{"columns":["a"],"rows":[]}"#));
        assert!(!recognized("", "select 1"));
    }

    #[test]
    fn a_block_that_will_not_parse_falls_back_rather_than_vanishing() {
        assert!(!recognized("datatable", "not json at all"));
        // Right language, wrong shape: still a fallback, never a blank card.
        assert!(!recognized("datatable", r#"{"rows":[["1"]]}"#));
        assert!(!recognized("barchart", r#"{"data":[]}"#));
    }

    #[test]
    fn numbers_read_as_written() {
        assert_eq!(trim_number(12.0), "12");
        assert_eq!(trim_number(-3.5), "-3.50");
        assert_eq!(cell_text(&Json::String("a b".into())), "a b");
        assert_eq!(cell_text(&Json::Null), "");
    }
}
