//! Dependency-free parsers from each engine's native `EXPLAIN` output into the
//! normalized [`red_core::QueryPlan`] tree (Track B4). No JSON: SQLite returns
//! tabular `(id, parent, detail)` rows, Postgres an indented text plan, MySQL a
//! `FORMAT=TREE` text tree, and older MySQL / MariaDB a tabular `EXPLAIN`. Every
//! parser also keeps the engine's verbatim text in [`QueryPlan::raw`], the UI's
//! "Copy plan" payload and its fallback when the structural parse yields nothing.
//!
//! Pure functions (text in, tree out), so the shapes are unit-tested without a
//! live server.

use red_core::{PlanNode, QueryPlan};

/// Build a plan from SQLite `EXPLAIN QUERY PLAN` rows `(id, parent, detail)`.
/// Parent `0` is a root; children link to their parent's id (SQLite assigns a
/// child a higher id than its parent, but we link by id rather than rely on it).
/// SQLite reports no cost numbers, so nodes carry only a label.
pub(crate) fn from_sqlite_rows(rows: Vec<(i64, i64, String)>) -> QueryPlan {
    use std::collections::HashMap;
    let mut detail: HashMap<i64, String> = HashMap::new();
    let mut children: HashMap<i64, Vec<i64>> = HashMap::new();
    let mut roots: Vec<i64> = Vec::new();
    for (id, parent, det) in &rows {
        detail.insert(*id, det.clone());
        if *parent == 0 {
            roots.push(*id);
        } else {
            children.entry(*parent).or_default().push(*id);
        }
    }
    fn build(
        id: i64,
        detail: &HashMap<i64, String>,
        children: &HashMap<i64, Vec<i64>>,
    ) -> PlanNode {
        let mut node = PlanNode::leaf(detail.get(&id).cloned().unwrap_or_default());
        if let Some(kids) = children.get(&id) {
            node.children = kids.iter().map(|k| build(*k, detail, children)).collect();
        }
        node
    }
    let nodes: Vec<PlanNode> = roots
        .iter()
        .map(|r| build(*r, &detail, &children))
        .collect();
    let raw = render_outline(&nodes);
    QueryPlan {
        nodes,
        raw,
        analyzed: false,
    }
}

/// Parse an indented text plan into a tree. Handles both Postgres's `FORMAT TEXT`
/// (root line has no marker, child nodes are prefixed `->`) and MySQL's
/// `FORMAT=TREE` (every node prefixed `->`): a line whose trimmed content starts
/// with `->` is a node at its indentation; the first non-marker line with an empty
/// stack is the root; any other non-marker line is detail for the current node.
/// `analyzed` flags an `EXPLAIN ANALYZE` plan (actual-time metrics present).
pub(crate) fn from_text_tree(raw: &str, analyzed: bool) -> QueryPlan {
    let mut stack: Vec<(usize, PlanNode)> = Vec::new();
    let mut roots: Vec<PlanNode> = Vec::new();

    for line in raw.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("->") {
            let (label, metrics) = split_paren_metrics(rest.trim_start());
            push_node(
                &mut stack,
                &mut roots,
                indent,
                PlanNode {
                    label,
                    detail: None,
                    metrics,
                    children: Vec::new(),
                },
            );
        } else if stack.is_empty() {
            let (label, metrics) = split_paren_metrics(trimmed);
            push_node(
                &mut stack,
                &mut roots,
                indent,
                PlanNode {
                    label,
                    detail: None,
                    metrics,
                    children: Vec::new(),
                },
            );
        } else if let Some((_, top)) = stack.last_mut() {
            append_detail(top, trimmed);
        }
    }
    while let Some((_, finished)) = stack.pop() {
        attach(&mut stack, &mut roots, finished);
    }
    QueryPlan {
        nodes: roots,
        raw: raw.trim_end().to_string(),
        analyzed,
    }
}

/// Parse an indentation-nested text plan with **no node markers**: ClickHouse's
/// `EXPLAIN`, where each step is a line whose nesting is purely its leading-space
/// indent (no `->` prefix like Postgres/MySQL). Every non-empty line is a node at
/// its indentation; a deeper-indented line nests under the previous shallower one.
/// Trailing `(key=value …)` groups fold into metrics, like the marker parser.
/// ClickHouse has no `EXPLAIN ANALYZE` actuals, so `analyzed` is always false.
pub(crate) fn from_indent_tree(raw: &str) -> QueryPlan {
    let mut stack: Vec<(usize, PlanNode)> = Vec::new();
    let mut roots: Vec<PlanNode> = Vec::new();
    for line in raw.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let (label, metrics) = split_paren_metrics(line.trim_start());
        push_node(
            &mut stack,
            &mut roots,
            indent,
            PlanNode {
                label,
                detail: None,
                metrics,
                children: Vec::new(),
            },
        );
    }
    while let Some((_, finished)) = stack.pop() {
        attach(&mut stack, &mut roots, finished);
    }
    QueryPlan {
        nodes: roots,
        raw: raw.trim_end().to_string(),
        analyzed: false,
    }
}

/// Build a flat plan from a tabular `EXPLAIN` (older MySQL / MariaDB, which lack
/// `FORMAT=TREE`): one node per row, its non-empty columns folded into metrics,
/// labelled by the `table` column when present. Not nested (honestly flat), but
/// readable, and `raw` carries the rendered table.
pub(crate) fn from_table(columns: Vec<String>, rows: Vec<Vec<String>>) -> QueryPlan {
    let table_col = columns.iter().position(|c| c.eq_ignore_ascii_case("table"));
    let nodes = rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let label = table_col
                .and_then(|c| row.get(c))
                .filter(|v| !v.is_empty())
                .cloned()
                .unwrap_or_else(|| format!("step {}", i + 1));
            let metrics = columns
                .iter()
                .zip(row.iter())
                .filter(|(_, v)| !v.is_empty())
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            PlanNode {
                label,
                detail: None,
                metrics,
                children: Vec::new(),
            }
        })
        .collect();
    QueryPlan {
        nodes,
        raw: render_table(&columns, &rows),
        analyzed: false,
    }
}

/// Attach a finished node as a child of the current stack top, or as a root.
fn attach(stack: &mut [(usize, PlanNode)], roots: &mut Vec<PlanNode>, node: PlanNode) {
    match stack.last_mut() {
        Some((_, parent)) => parent.children.push(node),
        None => roots.push(node),
    }
}

/// Push a node at `indent`, first closing every open node at an equal-or-deeper
/// indent (they're complete; a shallower or equal sibling has arrived).
fn push_node(
    stack: &mut Vec<(usize, PlanNode)>,
    roots: &mut Vec<PlanNode>,
    indent: usize,
    node: PlanNode,
) {
    while matches!(stack.last(), Some(&(top, _)) if top >= indent) {
        let (_, finished) = stack.pop().unwrap();
        attach(stack, roots, finished);
    }
    stack.push((indent, node));
}

/// Append a non-marker line to a node's detail (joined with ` · `).
fn append_detail(node: &mut PlanNode, line: &str) {
    let line = line.trim();
    if line.is_empty() {
        return;
    }
    match &mut node.detail {
        Some(d) => {
            d.push_str(" · ");
            d.push_str(line);
        }
        None => node.detail = Some(line.to_string()),
    }
}

/// Split an operation line into its label and the metrics from any trailing
/// `(key=value …)` group(s). A group is a metric group only if it contains `=`
/// (so a parenthesised filter like `(age > 30)` stays part of the label); the
/// label is the text before the first metric group. Keys may carry a space prefix
/// (Postgres's `(actual time=… rows=… loops=…)` becomes `actual time`, `rows`,
/// `loops`.
fn split_paren_metrics(content: &str) -> (String, Vec<(String, String)>) {
    let bytes = content.as_bytes();
    let mut groups: Vec<(usize, usize)> = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'(' {
            if depth == 0 {
                start = i;
            }
            depth += 1;
        } else if b == b')' && depth > 0 {
            depth -= 1;
            if depth == 0 {
                groups.push((start, i));
            }
        }
    }

    let mut metrics: Vec<(String, String)> = Vec::new();
    let mut label_end = content.len();
    let mut found = false;
    for (s, e) in &groups {
        let inner = &content[s + 1..*e];
        if inner.contains('=') {
            if !found {
                label_end = *s;
                found = true;
            }
            parse_metric_tokens(inner, &mut metrics);
        }
    }
    let label = content[..label_end].trim();
    let label = if label.is_empty() {
        content.trim()
    } else {
        label
    };
    (label.to_string(), metrics)
}

/// Parse `key=value` tokens, joining a leading word without `=` onto the next
/// key (so `actual time=0.1` → key `actual time`, value `0.1`).
fn parse_metric_tokens(inner: &str, out: &mut Vec<(String, String)>) {
    let mut prefix = String::new();
    for tok in inner.split_whitespace() {
        if let Some((k, v)) = tok.split_once('=') {
            let key = if prefix.is_empty() {
                k.to_string()
            } else {
                format!("{prefix} {k}")
            };
            out.push((key, v.to_string()));
            prefix.clear();
        } else {
            if !prefix.is_empty() {
                prefix.push(' ');
            }
            prefix.push_str(tok);
        }
    }
}

/// Render a node tree as an indented outline: SQLite's `raw` and the copy text.
fn render_outline(nodes: &[PlanNode]) -> String {
    fn walk(out: &mut String, node: &PlanNode, depth: usize) {
        for _ in 0..depth {
            out.push_str("  ");
        }
        out.push_str(&node.label);
        out.push('\n');
        for c in &node.children {
            walk(out, c, depth + 1);
        }
    }
    let mut out = String::new();
    for n in nodes {
        walk(&mut out, n, 0);
    }
    out.trim_end().to_string()
}

/// Render a tabular plan back to a header + ` | `-joined rows for `raw`.
fn render_table(columns: &[String], rows: &[Vec<String>]) -> String {
    let mut out = String::new();
    out.push_str(&columns.join(" | "));
    for row in rows {
        out.push('\n');
        out.push_str(&row.join(" | "));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_rows_build_a_tree_by_parent() {
        // A `Nested loop`-style plan: root id 1, two children 2 & 3 under it.
        let rows = vec![
            (1, 0, "SCAN authors".to_string()),
            (2, 1, "SEARCH books USING INDEX ix".to_string()),
            (3, 1, "USE TEMP B-TREE".to_string()),
        ];
        let plan = from_sqlite_rows(rows);
        assert_eq!(plan.nodes.len(), 1);
        assert_eq!(plan.nodes[0].label, "SCAN authors");
        assert_eq!(plan.nodes[0].children.len(), 2);
        assert_eq!(
            plan.nodes[0].children[0].label,
            "SEARCH books USING INDEX ix"
        );
        assert!(plan.raw.contains("SCAN authors"));
        assert!(!plan.analyzed);
    }

    #[test]
    fn pg_text_nests_by_indent_and_marker() {
        let raw = "Hash Join  (cost=1.09..2.21 rows=5 width=8)\n  \
                   Hash Cond: (a.id = b.a_id)\n  \
                   ->  Seq Scan on a  (cost=0.00..1.05 rows=5 width=4)\n  \
                   ->  Hash  (cost=1.04..1.04 rows=4 width=8)\n        \
                   ->  Seq Scan on b  (cost=0.00..1.04 rows=4 width=8)";
        let plan = from_text_tree(raw, false);
        assert_eq!(plan.nodes.len(), 1, "single root");
        let root = &plan.nodes[0];
        assert_eq!(root.label, "Hash Join");
        // cost/rows/width parsed off the root line.
        assert!(root.metrics.iter().any(|(k, v)| k == "rows" && v == "5"));
        assert_eq!(root.detail.as_deref(), Some("Hash Cond: (a.id = b.a_id)"));
        assert_eq!(root.children.len(), 2, "Seq Scan on a + Hash");
        let hash = &root.children[1];
        assert_eq!(hash.label, "Hash");
        assert_eq!(hash.children.len(), 1, "Seq Scan on b nests under Hash");
        assert_eq!(hash.children[0].label, "Seq Scan on b");
    }

    #[test]
    fn pg_analyze_parses_actual_time() {
        let raw = "Seq Scan on users  (cost=0.00..1.05 rows=5 width=4) \
                   (actual time=0.011..0.013 rows=5 loops=1)";
        let plan = from_text_tree(raw, true);
        assert!(plan.analyzed);
        let m = &plan.nodes[0].metrics;
        assert!(
            m.iter()
                .any(|(k, v)| k == "actual time" && v == "0.011..0.013"),
            "metrics: {m:?}"
        );
        assert!(m.iter().any(|(k, v)| k == "loops" && v == "1"));
    }

    #[test]
    fn mysql_tree_parses_marker_root() {
        let raw = "-> Nested loop inner join  (cost=1.40 rows=2)\n    \
                   -> Table scan on a  (cost=0.45 rows=2)\n    \
                   -> Single-row index lookup on b using PRIMARY  (cost=0.35 rows=1)";
        let plan = from_text_tree(raw, false);
        assert_eq!(plan.nodes.len(), 1);
        assert_eq!(plan.nodes[0].label, "Nested loop inner join");
        assert_eq!(plan.nodes[0].children.len(), 2);
    }

    #[test]
    fn clickhouse_indent_nests_without_markers() {
        // ClickHouse `EXPLAIN` indents by spaces with no `->` markers: the
        // `ReadFromMergeTree` step nests under `Expression`.
        let raw = "Expression ((Project names + Projection))\n  \
                   Expression\n    \
                   ReadFromMergeTree (default.events)";
        let plan = from_indent_tree(raw);
        assert_eq!(plan.nodes.len(), 1, "single root");
        let root = &plan.nodes[0];
        assert_eq!(root.label, "Expression ((Project names + Projection))");
        assert_eq!(root.children.len(), 1);
        assert_eq!(root.children[0].children.len(), 1);
        assert_eq!(
            root.children[0].children[0].label,
            "ReadFromMergeTree (default.events)"
        );
        assert!(!plan.analyzed);
        assert!(plan.raw.contains("default.events"));
    }

    #[test]
    fn tabular_fallback_is_flat_with_metrics() {
        let columns = vec!["id".into(), "table".into(), "type".into(), "rows".into()];
        let rows = vec![
            vec!["1".into(), "users".into(), "ALL".into(), "42".into()],
            vec!["1".into(), "orders".into(), "ref".into(), "".into()],
        ];
        let plan = from_table(columns, rows);
        assert_eq!(plan.nodes.len(), 2);
        assert_eq!(plan.nodes[0].label, "users");
        assert!(plan.nodes[0]
            .metrics
            .iter()
            .any(|(k, v)| k == "rows" && v == "42"));
        // Empty cells are dropped from metrics.
        assert!(!plan.nodes[1].metrics.iter().any(|(k, _)| k == "rows"));
    }

    #[test]
    fn label_keeps_parenthesised_filter_without_equals_metrics() {
        // A trailing `(x > 30)` has no `=`, so it stays in the label, not metrics.
        let (label, metrics) = split_paren_metrics("Filter on t (x > 30)");
        assert_eq!(label, "Filter on t (x > 30)");
        assert!(metrics.is_empty());
    }
}
