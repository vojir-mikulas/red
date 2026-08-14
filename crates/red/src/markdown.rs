//! A small Markdown renderer for assistant chat bubbles. The model answers in
//! Markdown; rendering it (instead of showing the raw `**`/`#`/fences) makes the
//! transcript readable. This is a pragmatic subset (paragraphs, ATX headings,
//! fenced code blocks, bullet/numbered lists, thematic breaks, and inline
//! `**bold**` / `*italic*` / `` `code` ``) rendered with `StyledText` runs so
//! prose still wraps naturally. It is intentionally not a full CommonMark engine.

use flint::Theme;
use gpui::{AnyElement, SharedString, StyledText, TextRun, div, font, prelude::*, px};

/// Render Markdown `src` as a column of block elements.
pub(crate) fn render(src: &str, theme: &Theme) -> AnyElement {
    render_blocks(&parse(src), theme)
}

/// Parse Markdown `src` into its blocks. Exposed (with [`render_blocks`]) so a
/// caller can cache the parse for a *settled* message and rebuild only the elements
/// each frame, instead of re-parsing the whole transcript on every repaint.
pub(crate) fn parse(src: &str) -> Vec<Block> {
    parse_blocks(src)
}

/// Render already-parsed `blocks` as a column of block elements.
pub(crate) fn render_blocks(blocks: &[Block], theme: &Theme) -> AnyElement {
    render_blocks_with(blocks, theme, &mut |text, runs| styled(text, runs))
}

/// A factory for a text leaf: given the plain text and its styled runs, produce the
/// element that renders it. The default ([`render_blocks`]) makes a non-interactive
/// `StyledText`; the assistant panel passes one that pulls a pooled, *selectable*
/// `SelectableLabel` so settled chat prose can be highlighted and copied. Called once
/// per text leaf (paragraphs, headings, list items, table cells) in document order,
/// so a caller can index a prebuilt pool by call count. Code blocks and rules carry
/// no leaf (they keep their own layout / affordances).
pub(crate) type TextLeaf<'a> = dyn FnMut(String, Vec<TextRun>) -> AnyElement + 'a;

/// Render `blocks` routing every text leaf through `leaf` (see [`TextLeaf`]).
pub(crate) fn render_blocks_with(
    blocks: &[Block],
    theme: &Theme,
    leaf: &mut TextLeaf,
) -> AnyElement {
    let mut col = div().flex().flex_col().gap_1p5();
    for block in blocks {
        col = col.child(render_block(block, theme, leaf));
    }
    col.into_any_element()
}

/// A parsed top-level block.
pub(crate) enum Block {
    Paragraph(String),
    Heading(u8, String),
    /// A fenced block, with the info string that followed the opening fence when
    /// there was one. The language is kept rather than dropped because it decides
    /// whether the block renders as code or as a component (see
    /// [`crate::aiblocks`]).
    Code {
        lang: Option<String>,
        text: String,
    },
    Bullets(Vec<String>),
    Numbers(Vec<String>),
    Table {
        headers: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    Rule,
}

/// Split the source into blocks line-by-line (no nesting beyond one list level).
fn parse_blocks(src: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut para: Vec<&str> = Vec::new();
    let mut bullets: Vec<String> = Vec::new();
    let mut numbers: Vec<String> = Vec::new();

    // Flush whatever run of lines is currently open before starting a new kind.
    fn flush_para(blocks: &mut Vec<Block>, para: &mut Vec<&str>) {
        if !para.is_empty() {
            blocks.push(Block::Paragraph(para.join(" ")));
            para.clear();
        }
    }
    fn flush_bullets(blocks: &mut Vec<Block>, bullets: &mut Vec<String>) {
        if !bullets.is_empty() {
            blocks.push(Block::Bullets(std::mem::take(bullets)));
        }
    }
    fn flush_numbers(blocks: &mut Vec<Block>, numbers: &mut Vec<String>) {
        if !numbers.is_empty() {
            blocks.push(Block::Numbers(std::mem::take(numbers)));
        }
    }

    let mut lines = src.lines().peekable();
    while let Some(line) = lines.next() {
        let trimmed = line.trim_start();

        // Fenced code block: collect verbatim until the closing fence.
        if let Some(info) = trimmed.strip_prefix("```") {
            flush_para(&mut blocks, &mut para);
            flush_bullets(&mut blocks, &mut bullets);
            flush_numbers(&mut blocks, &mut numbers);
            let lang = Some(info.trim().to_string()).filter(|l| !l.is_empty());
            let mut code = Vec::new();
            for l in lines.by_ref() {
                if l.trim_start().starts_with("```") {
                    break;
                }
                code.push(l);
            }
            blocks.push(Block::Code {
                lang,
                text: code.join("\n"),
            });
            continue;
        }

        // Blank line ends any open run.
        if trimmed.is_empty() {
            flush_para(&mut blocks, &mut para);
            flush_bullets(&mut blocks, &mut bullets);
            flush_numbers(&mut blocks, &mut numbers);
            continue;
        }

        // Thematic break.
        if matches!(trimmed, "---" | "***" | "___") {
            flush_para(&mut blocks, &mut para);
            flush_bullets(&mut blocks, &mut bullets);
            flush_numbers(&mut blocks, &mut numbers);
            blocks.push(Block::Rule);
            continue;
        }

        // ATX heading (`#`..`######`).
        if let Some((level, text)) = heading(trimmed) {
            flush_para(&mut blocks, &mut para);
            flush_bullets(&mut blocks, &mut bullets);
            flush_numbers(&mut blocks, &mut numbers);
            blocks.push(Block::Heading(level, text));
            continue;
        }

        // GFM table: a row of `|`-separated cells immediately followed by a
        // delimiter row (`| --- | --- |`). Collect the contiguous body rows.
        if trimmed.contains('|') && lines.peek().is_some_and(|n| is_delimiter_row(n.trim())) {
            flush_para(&mut blocks, &mut para);
            flush_bullets(&mut blocks, &mut bullets);
            flush_numbers(&mut blocks, &mut numbers);
            let headers = table_cells(trimmed);
            lines.next(); // consume the delimiter row
            let mut rows = Vec::new();
            while let Some(peeked) = lines.peek() {
                let lt = peeked.trim();
                if lt.is_empty() || !lt.contains('|') {
                    break;
                }
                rows.push(table_cells(lt));
                lines.next();
            }
            blocks.push(Block::Table { headers, rows });
            continue;
        }

        // Bullet list item.
        if let Some(rest) = bullet_item(trimmed) {
            flush_para(&mut blocks, &mut para);
            flush_numbers(&mut blocks, &mut numbers);
            bullets.push(rest.to_string());
            continue;
        }

        // Numbered list item.
        if let Some(rest) = numbered_item(trimmed) {
            flush_para(&mut blocks, &mut para);
            flush_bullets(&mut blocks, &mut bullets);
            numbers.push(rest.to_string());
            continue;
        }

        // Otherwise it's prose; lists/paragraphs don't interleave.
        flush_bullets(&mut blocks, &mut bullets);
        flush_numbers(&mut blocks, &mut numbers);
        para.push(line.trim());
    }

    flush_para(&mut blocks, &mut para);
    flush_bullets(&mut blocks, &mut bullets);
    flush_numbers(&mut blocks, &mut numbers);
    blocks
}

fn heading(line: &str) -> Option<(u8, String)> {
    let hashes = line.chars().take_while(|&c| c == '#').count();
    if (1..=6).contains(&hashes) && line[hashes..].starts_with(' ') {
        Some((hashes as u8, line[hashes..].trim().to_string()))
    } else {
        None
    }
}

/// A table delimiter row: every `|`-separated cell is dashes (with optional
/// `:` alignment markers), e.g. `| :--- | ---: |`.
fn is_delimiter_row(line: &str) -> bool {
    if !line.contains('-') {
        return false;
    }
    let cells = split_cells(line);
    !cells.is_empty()
        && cells.iter().all(|c| {
            let c = c.trim();
            !c.is_empty() && c.contains('-') && c.chars().all(|ch| ch == '-' || ch == ':')
        })
}

/// Split one table row into trimmed cell strings (outer pipes stripped).
fn table_cells(line: &str) -> Vec<String> {
    split_cells(line)
        .into_iter()
        .map(|c| c.trim().to_string())
        .collect()
}

fn split_cells(line: &str) -> Vec<String> {
    let t = line.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    t.split('|').map(str::to_string).collect()
}

fn bullet_item(line: &str) -> Option<&str> {
    for marker in ["- ", "* ", "+ "] {
        if let Some(rest) = line.strip_prefix(marker) {
            return Some(rest.trim());
        }
    }
    None
}

fn numbered_item(line: &str) -> Option<&str> {
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let after = &line[digits..];
    after
        .strip_prefix(". ")
        .or_else(|| after.strip_prefix(") "))
        .map(str::trim)
}

/// Max source lines rendered for one code block. One element is built per line, so
/// an unbounded block (a model dumping a huge file) would build thousands of nodes
/// every repaint. Past this a summary row stands in.
const MAX_CODE_LINES: usize = 400;

/// Max body rows rendered for one Markdown table. The result grid is the place for
/// large result sets; a chat table is a summary, so cap it and note the remainder.
const MAX_TABLE_ROWS: usize = 200;

fn render_block(block: &Block, theme: &Theme, leaf: &mut TextLeaf) -> AnyElement {
    match block {
        Block::Paragraph(text) => div()
            .text_size(theme.scale(12.5))
            .child(inline(text, theme, leaf))
            .into_any_element(),
        Block::Heading(level, text) => {
            let size = match level {
                1 => 16.0,
                2 => 14.5,
                _ => 13.0,
            };
            div()
                .text_size(theme.scale(size))
                .child(inline_bold(text, theme, leaf))
                .into_any_element()
        }
        Block::Code { lang, text: code } => {
            // A fenced block the assistant meant as data renders as data. A body
            // that will not parse falls through to the code rendering below, so
            // nothing the model wrote is ever swallowed.
            if let Some(lang) = lang
                && let Some(rich) = crate::aiblocks::render(lang, code, theme)
            {
                return rich;
            }
            let mut block = div()
                .flex()
                .flex_col()
                .p_2()
                .rounded(px(5.))
                .bg(theme.bg_elevated)
                .font_family(theme.mono_family.clone())
                .text_size(theme.scale(11.5))
                .text_color(theme.text);
            for line in code.lines().take(MAX_CODE_LINES) {
                // A non-breaking-ish line: render each source line as its own row.
                block = block.child(div().child(line.to_string()));
            }
            let total = code.lines().count();
            if total > MAX_CODE_LINES {
                block = block.child(
                    div()
                        .text_color(theme.text_muted)
                        .child(format!("… {} more lines", total - MAX_CODE_LINES)),
                );
            }
            block.into_any_element()
        }
        Block::Bullets(items) => {
            let mut list = div().flex().flex_col().gap_1();
            for item in items {
                list = list.child(
                    div()
                        .flex()
                        .gap_1p5()
                        .text_size(theme.scale(12.5))
                        .child(div().flex_none().text_color(theme.text_muted).child("•"))
                        .child(list_body(inline(item, theme, leaf))),
                );
            }
            list.into_any_element()
        }
        Block::Numbers(items) => {
            let mut list = div().flex().flex_col().gap_1();
            for (i, item) in items.iter().enumerate() {
                list = list.child(
                    div()
                        .flex()
                        .gap_1p5()
                        .text_size(theme.scale(12.5))
                        .child(
                            div()
                                .flex_none()
                                .text_color(theme.text_muted)
                                .child(format!("{}.", i + 1)),
                        )
                        .child(list_body(inline(item, theme, leaf))),
                );
            }
            list.into_any_element()
        }
        Block::Table { headers, rows } => {
            let mut table = div()
                .flex()
                .flex_col()
                .rounded(px(5.))
                .border_1()
                .border_color(theme.border)
                .overflow_hidden()
                .text_size(theme.scale(11.5));
            table = table.child(table_row(headers, theme, true, leaf));
            // Normalize every body row to the header's column count so a ragged row
            // (more/fewer cells than the header) can't skew the grid, and cap the
            // number of rows rendered.
            let cols = headers.len();
            for row in rows.iter().take(MAX_TABLE_ROWS) {
                let cells: Vec<String> = (0..cols)
                    .map(|i| row.get(i).cloned().unwrap_or_default())
                    .collect();
                table = table.child(table_row(&cells, theme, false, leaf));
            }
            if rows.len() > MAX_TABLE_ROWS {
                table = table.child(
                    div()
                        .px_2()
                        .py_1()
                        .text_color(theme.text_muted)
                        .child(format!("… {} more rows", rows.len() - MAX_TABLE_ROWS)),
                );
            }
            table.into_any_element()
        }
        Block::Rule => div().h(px(1.)).my_1().bg(theme.border).into_any_element(),
    }
}

/// A list item's text cell.
///
/// `min_w(0)` is load-bearing, not cosmetic. GPUI measures text at its *unwrapped*
/// width under `AvailableSpace::MinContent`, so a text element's min-content size
/// equals its max-content size. A flex item's automatic minimum size is that
/// min-content size, so without this the cell refuses to shrink to the row and a
/// long item runs off the right edge instead of wrapping. Every text cell in a
/// *row*-direction flex needs it (see [`table_row`]).
fn list_body(text: AnyElement) -> gpui::Div {
    div().flex_1().min_w(px(0.)).child(text)
}

/// One table row: equal-width cells, a bottom rule, and a subtle header tint.
fn table_row(cells: &[String], theme: &Theme, header: bool, leaf: &mut TextLeaf) -> AnyElement {
    let mut row = div()
        .flex()
        .border_b_1()
        .border_color(theme.border)
        .when(header, |r| r.bg(theme.bg_elevated));
    for cell in cells {
        let body = if header {
            inline_bold(cell, theme, leaf)
        } else {
            inline(cell, theme, leaf)
        };
        row = row.child(div().flex_1().min_w(px(0.)).px_2().py_1().child(body));
    }
    row.into_any_element()
}

/// Inline span styles we recognise.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Span {
    Plain,
    Bold,
    Italic,
    Code,
    /// A `[N]` source citation the agent attached to a claim, resolving to the
    /// turn's N-th data-returning tool call. Rendered small and tinted so a cited
    /// figure reads differently from an uncited one at a glance -- which is the
    /// point, since "this paragraph cites nothing" is a claim about the answer.
    Citation,
}

/// Render inline Markdown (`**bold**`, `*italic*`, `` `code` ``) as wrapping
/// styled text via `leaf`. The wrapping div must set the text size (runs carry only
/// family / weight / style / color).
fn inline(text: &str, theme: &Theme, leaf: &mut TextLeaf) -> AnyElement {
    let segments = parse_inline(text);
    let mut s = String::new();
    let mut runs = Vec::new();
    for (seg, span) in segments {
        // A citation keeps its brackets on screen: the marker survives being
        // copied out of the chat, and `[3]` is what people already read as a
        // citation.
        let seg = if span == Span::Citation {
            format!("[{seg}]")
        } else {
            seg
        };
        let f = match span {
            Span::Plain => font(theme.font_family.clone()),
            Span::Bold => font(theme.font_family.clone()).bold(),
            Span::Italic => font(theme.font_family.clone()).italic(),
            Span::Code => font(theme.mono_family.clone()),
            Span::Citation => font(theme.mono_family.clone()),
        };
        let color = match span {
            Span::Code | Span::Citation => theme.accent,
            _ => theme.text,
        };
        runs.push(TextRun {
            len: seg.len(),
            font: f,
            color,
            // A faint tint of the accent, not a solid surface; reads as a subtle
            // chip in every theme rather than a stark white/black box (the old
            // `bg_elevated` was pure white in Ayu Light).
            background_color: (span == Span::Code).then(|| theme.accent.opacity(0.12)),
            underline: None,
            strikethrough: None,
        });
        s.push_str(&seg);
    }
    leaf(s, runs)
}

/// A whole-string bold variant for headings.
fn inline_bold(text: &str, theme: &Theme, leaf: &mut TextLeaf) -> AnyElement {
    let run = TextRun {
        len: text.len(),
        font: font(theme.font_family.clone()).bold(),
        color: theme.text,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    leaf(text.to_string(), vec![run])
}

fn styled(text: String, runs: Vec<TextRun>) -> AnyElement {
    if text.is_empty() {
        return div().into_any_element();
    }
    StyledText::new(SharedString::from(text))
        .with_runs(runs)
        .into_any_element()
}

/// Split a line into styled segments. Backtick code spans win over emphasis;
/// unmatched markers fall back to plain text.
fn parse_inline(text: &str) -> Vec<(String, Span)> {
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<(String, Span)> = Vec::new();
    let mut plain = String::new();
    let mut i = 0;

    let push_plain = |plain: &mut String, out: &mut Vec<(String, Span)>| {
        if !plain.is_empty() {
            out.push((std::mem::take(plain), Span::Plain));
        }
    };

    while i < chars.len() {
        let c = chars[i];
        // Inline code: verbatim until the next backtick.
        if c == '`'
            && let Some(end) = find(&chars, i + 1, '`')
        {
            push_plain(&mut plain, &mut out);
            out.push((chars[i + 1..end].iter().collect(), Span::Code));
            i = end + 1;
            continue;
        }
        // Bold, `**…**` (checked before single-`*` italic).
        if c == '*'
            && i + 1 < chars.len()
            && chars[i + 1] == '*'
            && let Some(end) = find_seq(&chars, i + 2, '*', '*')
        {
            push_plain(&mut plain, &mut out);
            out.push((chars[i + 2..end].iter().collect(), Span::Bold));
            i = end + 2;
            continue;
        }
        // A `[N]` citation. Bounded to digits, so ordinary bracketed prose and a
        // Markdown link (`[text](url)`, which has the paren) are left alone.
        if c == '['
            && let Some(end) = find(&chars, i + 1, ']')
            && end > i + 1
            && chars[i + 1..end].iter().all(|c| c.is_ascii_digit())
            && chars.get(end + 1) != Some(&'(')
        {
            push_plain(&mut plain, &mut out);
            out.push((chars[i + 1..end].iter().collect(), Span::Citation));
            i = end + 1;
            continue;
        }
        // Italic: `*…*` or `_…_`.
        if (c == '*' || c == '_')
            && let Some(end) = find(&chars, i + 1, c)
        {
            push_plain(&mut plain, &mut out);
            out.push((chars[i + 1..end].iter().collect(), Span::Italic));
            i = end + 1;
            continue;
        }
        plain.push(c);
        i += 1;
    }
    push_plain(&mut plain, &mut out);
    if out.is_empty() {
        out.push((String::new(), Span::Plain));
    }
    out
}

fn find(chars: &[char], from: usize, target: char) -> Option<usize> {
    (from..chars.len()).find(|&j| chars[j] == target)
}

fn find_seq(chars: &[char], from: usize, a: char, b: char) -> Option<usize> {
    (from..chars.len().saturating_sub(1)).find(|&j| chars[j] == a && chars[j + 1] == b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans(text: &str) -> Vec<(String, Span)> {
        parse_inline(text)
    }

    /// A `[N]` marker becomes a citation; ordinary bracketed prose does not.
    /// The negative cases are the ones that matter: a false citation is a link to
    /// a source that does not exist.
    #[test]
    fn only_a_numeric_bracket_is_a_citation() {
        let segs = spans("revenue was $4.2M [3] last month");
        assert_eq!(
            segs.iter().map(|(_, s)| *s).collect::<Vec<_>>(),
            vec![Span::Plain, Span::Citation, Span::Plain]
        );
        assert_eq!(segs[1].0, "3");

        // Not citations: words, empty brackets, and a Markdown link.
        for text in ["see [foo] here", "empty [] here", "a [text](url) link"] {
            assert!(
                spans(text).iter().all(|(_, s)| *s != Span::Citation),
                "{text} must not parse as a citation"
            );
        }
    }

    /// SQL is full of brackets, and a fenced block is verbatim by construction:
    /// `parse_inline` never sees a code block's contents, so `[3]` inside one
    /// stays literal.
    #[test]
    fn a_citation_inside_a_code_block_stays_literal() {
        let blocks = parse("```sql\nSELECT a[3] FROM t\n```");
        match &blocks[..] {
            [Block::Code { text, .. }] => assert!(text.contains("a[3]"), "{text}"),
            other => panic!("expected one code block, got {} blocks", other.len()),
        }
        // An inline code span is verbatim too.
        let segs = spans("run `SELECT a[3]` first");
        assert!(segs.iter().all(|(_, s)| *s != Span::Citation));
    }

    /// An out-of-range marker is still a citation *token* -- resolving it is the
    /// panel's job, and the panel simply finds no chip with that number. What
    /// must not happen is a panic or a link to nothing.
    #[test]
    fn an_out_of_range_marker_is_harmless() {
        let segs = spans("total was 9 [99]");
        assert_eq!(
            segs.last().map(|(t, s)| (t.as_str(), *s)),
            Some(("99", Span::Citation))
        );
    }

    #[test]
    fn splits_inline_styles() {
        let segs = parse_inline("a **b** c `d` *e*");
        let kinds: Vec<Span> = segs.iter().map(|(_, s)| *s).collect();
        assert_eq!(
            kinds,
            vec![
                Span::Plain,
                Span::Bold,
                Span::Plain,
                Span::Code,
                Span::Plain,
                Span::Italic
            ]
        );
        // Byte lengths must sum to the marker-stripped text (StyledText invariant).
        let joined: String = segs.iter().map(|(t, _)| t.as_str()).collect();
        let total: usize = segs.iter().map(|(t, _)| t.len()).sum();
        assert_eq!(total, joined.len());
    }

    #[test]
    fn parses_block_kinds() {
        let md = "# Title\n\npara line\n\n- one\n- two\n\n```\ncode\n```";
        let blocks = parse_blocks(md);
        assert!(matches!(blocks[0], Block::Heading(1, _)));
        assert!(matches!(blocks[1], Block::Paragraph(_)));
        assert!(matches!(&blocks[2], Block::Bullets(v) if v.len() == 2));
        assert!(matches!(blocks[3], Block::Code { .. }));
    }

    #[test]
    fn parses_a_gfm_table() {
        let md = "| Name | Rows |\n| --- | ---: |\n| widgets | 3 |\n| gadgets | 7 |";
        let blocks = parse_blocks(md);
        assert_eq!(blocks.len(), 1);
        let Block::Table { headers, rows } = &blocks[0] else {
            panic!("expected a table, got something else");
        };
        assert_eq!(headers, &["Name", "Rows"]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], vec!["widgets".to_string(), "3".to_string()]);
    }
}
