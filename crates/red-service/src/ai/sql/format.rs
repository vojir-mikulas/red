//! Rendering `red-core` SQL types as the text the model reads.
//!
//! Pure functions over already-fetched values: nothing here touches a driver, so
//! every one of them is directly testable against a hand-built fixture. The
//! recurring rule is that an *absence* must be stated rather than left to
//! inference -- a health check that could not run says so, and a statement the
//! role may not read is reported as hidden, not as missing.

use red_core::Value;

use super::super::util::{fmt_bytes, truncate_summary};

/// `schema.table`, or the bare table on an engine with no schemas.
pub(in crate::ai) fn qualified(schema: Option<&str>, table: &str) -> String {
    match schema.filter(|s| !s.is_empty()) {
        Some(s) => format!("{s}.{table}"),
        None => table.to_string(),
    }
}
/// One end of an FK edge as `schema.table.column`, or `schema.table.(a, b)` for a
/// composite key. `side` picks the column of each `(from, to)` pair.
pub(super) fn fk_side(
    schema: Option<&str>,
    table: &str,
    side: usize,
    columns: &[(String, String)],
) -> String {
    let cols: Vec<&str> = columns
        .iter()
        .map(|(from, to)| {
            if side == 0 {
                from.as_str()
            } else {
                to.as_str()
            }
        })
        .collect();
    let table = qualified(schema, table);
    match cols.as_slice() {
        [one] => format!("{table}.{one}"),
        many => format!("{table}.({})", many.join(", ")),
    }
}
pub(super) fn format_schema(schemas: &[red_core::SchemaMeta]) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    for sch in schemas {
        let _ = writeln!(out, "schema {} ({} objects):", sch.name, sch.objects.len());
        for obj in &sch.objects {
            let _ = writeln!(out, "  {} {}", obj.kind.as_str(), obj.name);
        }
    }
    if out.is_empty() {
        out.push_str("(no objects)");
    }
    out
}
pub(super) fn format_table_detail(schema: &str, table: &str, d: &red_core::TableDetail) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "{schema}.{table}");
    let _ = writeln!(out, "columns:");
    for c in &d.columns {
        let ty = c.type_name.as_deref().unwrap_or("?");
        let mut flags = Vec::new();
        if c.primary_key {
            flags.push("PK");
        }
        if c.not_null {
            flags.push("NOT NULL");
        }
        let flags = if flags.is_empty() {
            String::new()
        } else {
            format!(" [{}]", flags.join(", "))
        };
        let _ = writeln!(out, "  {} {ty}{flags}", c.name);
    }
    if !d.foreign_keys.is_empty() {
        let _ = writeln!(out, "foreign keys:");
        for fk in &d.foreign_keys {
            let _ = writeln!(out, "  {} -> {}.{}", fk.column, fk.ref_table, fk.ref_column);
        }
    }
    if !d.indexes.is_empty() {
        let _ = writeln!(out, "indexes:");
        for ix in &d.indexes {
            let uniq = if ix.unique { "unique " } else { "" };
            let _ = writeln!(out, "  {uniq}{} ({})", ix.name, ix.columns.join(", "));
        }
    }
    out
}
pub(in crate::ai) fn format_page(page: &red_core::ResultPage) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let header: Vec<&str> = page.columns.iter().map(|c| c.name.as_str()).collect();
    let _ = writeln!(out, "{}", header.join(" | "));
    for row in &page.rows {
        let cells: Vec<String> = row.iter().map(render_cell).collect();
        let _ = writeln!(out, "{}", cells.join(" | "));
    }
    let _ = write!(out, "({} rows)", page.rows.len());
    out
}
pub(in crate::ai) fn render_cell(v: &Value) -> String {
    // `Value`'s Display already renders NULL, capped text (`head…`), and blobs
    // (`<N bytes>`), exactly the compact form we want for the model.
    v.to_string()
}
pub(super) fn format_plan(plan: &red_core::QueryPlan) -> String {
    if plan.nodes.is_empty() {
        return plan.raw.clone();
    }
    let mut out = String::new();
    for node in &plan.nodes {
        write_plan_node(&mut out, node, 0);
    }
    out
}
fn write_plan_node(out: &mut String, node: &red_core::PlanNode, depth: usize) {
    use std::fmt::Write;
    let indent = "  ".repeat(depth);
    let _ = write!(out, "{indent}{}", node.label);
    if let Some(d) = &node.detail {
        let _ = write!(out, " — {d}");
    }
    if !node.metrics.is_empty() {
        let m: Vec<String> = node
            .metrics
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        let _ = write!(out, " [{}]", m.join(", "));
    }
    out.push('\n');
    for child in &node.children {
        write_plan_node(out, child, depth + 1);
    }
}
/// How many of the largest tables `health_report` lists. Enough to answer "where
/// did the disk go" without turning the report into a catalog dump.
const HEALTH_TOP_TABLES: usize = 20;
/// A [`HealthReport`](red_core::health::HealthReport) as text for the agent.
///
/// The `unavailable` list is not decoration: a report that silently drops the
/// unused-index check reads as a clean bill of health, so what could *not* be
/// checked is stated as plainly as what was.
pub(super) fn format_health(r: &red_core::health::HealthReport) -> String {
    use std::fmt::Write;

    let scope = match &r.namespace {
        Some(ns) => format!(" (schema {ns})"),
        None => String::new(),
    };
    let mut s = format!(
        "Health of this {:?} connection{scope}\n{} across {} table(s), of which {} is index.\n",
        r.engine,
        fmt_bytes(r.totals.bytes),
        r.totals.table_count,
        fmt_bytes(r.totals.index_bytes),
    );
    if !r.tables.is_empty() {
        s.push_str("\nLargest tables:\n");
        for t in r.tables.iter().take(HEALTH_TOP_TABLES) {
            let _ = writeln!(
                s,
                "  {}  {} ({} index, ~{} rows est)",
                qualified(t.table.schema.as_deref(), &t.table.name),
                fmt_bytes(t.bytes),
                fmt_bytes(t.index_bytes),
                t.estimated_rows,
            );
        }
        if r.tables.len() > HEALTH_TOP_TABLES {
            let _ = writeln!(s, "  …({} more)", r.tables.len() - HEALTH_TOP_TABLES);
        }
    }
    let findings = r.sorted_findings();
    if findings.is_empty() {
        s.push_str("\nNo findings from the checks that ran.\n");
    } else {
        let _ = write!(s, "\n{} finding(s), worst first:\n", findings.len());
        for f in findings {
            let object = f
                .object
                .as_ref()
                .map(|t| format!(" {}", qualified(t.schema.as_deref(), &t.name)))
                .unwrap_or_default();
            let _ = writeln!(s, "  [{:?}] {:?}{object}: {}", f.severity, f.kind, f.title);
            let _ = writeln!(s, "    {}", f.detail);
            if let Some(sql) = &f.suggested_sql {
                // Text to read and paste. RED never runs a remediation itself, and
                // saying so keeps the model from treating it as something to apply.
                let _ = writeln!(s, "    suggested (NOT run; hand this to the user): {sql}");
            }
        }
    }
    if !r.unavailable.is_empty() {
        s.push_str("\nChecks that could NOT run here (so their absence proves nothing):\n");
        for u in &r.unavailable {
            let _ = writeln!(s, "  {:?}: {}", u.kind, u.reason);
        }
    }
    s
}
/// Live server sessions as text, longest-running first (the driver's own order).
pub(super) fn format_sessions(sessions: &[red_core::ServerSession], restricted: bool) -> String {
    use std::fmt::Write;

    if sessions.is_empty() {
        return "No client sessions are running.".to_string();
    }
    let mut s = format!("{} session(s), longest-running first:\n", sessions.len());
    for x in sessions {
        let field = |label: &str, v: &Option<String>| match v {
            Some(v) if !v.is_empty() => format!(" {label}={v}"),
            _ => String::new(),
        };
        let _ = write!(
            s,
            "  [{}]{}{}{}{} {} for {:.1}s",
            x.key,
            field("user", &x.user),
            field("db", &x.database),
            field("app", &x.application),
            field("from", &x.client_addr),
            x.state,
            x.elapsed_secs,
        );
        if x.is_self {
            s.push_str(" (RED's own connection)");
        }
        s.push('\n');
        if let Some(w) = &x.wait {
            let _ = writeln!(s, "    waiting on {w}");
        }
        if !x.blocked_by.is_empty() {
            let by: Vec<String> = x.blocked_by.iter().map(ToString::to_string).collect();
            let _ = writeln!(s, "    blocked by {}", by.join(", "));
        }
        match &x.query {
            Some(q) => {
                let _ = writeln!(s, "    {}", truncate_summary(q.trim(), 300));
            }
            None => s.push_str("    (statement not visible to this role)\n"),
        }
    }
    if restricted {
        s.push_str(
            "(the connected role may not read other sessions' statements, so some are hidden \
             rather than absent)\n",
        );
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use red_core::TableRef;

    /// A report that drops a check it could not run reads as a clean bill of
    /// health, so the unavailable list is part of the output contract.
    #[test]
    fn health_report_states_what_it_could_not_check() {
        use red_core::health::{
            Finding, FindingKind, HealthReport, Severity, SizeTotals, TableSize, UnavailableCheck,
        };

        let mut report = HealthReport::new(red_core::DbKind::Postgres, Some("public".into()), 0);
        report.totals = SizeTotals {
            bytes: 2 * 1024 * 1024,
            index_bytes: 1024 * 1024,
            table_count: 3,
        };
        report.tables = vec![TableSize {
            table: TableRef {
                schema: Some("public".into()),
                name: "events".into(),
            },
            bytes: 1024 * 1024,
            index_bytes: 512 * 1024,
            estimated_rows: 90_000,
        }];
        report.findings = vec![Finding {
            severity: Severity::Bad,
            kind: FindingKind::MissingFkIndex,
            object: Some(TableRef {
                schema: Some("public".into()),
                name: "order_items".into(),
            }),
            title: "foreign key with no index".into(),
            detail: "every parent delete scans".into(),
            suggested_sql: Some("CREATE INDEX ...".into()),
        }];
        report.unavailable = vec![UnavailableCheck {
            kind: FindingKind::UnusedIndex,
            reason: "needs pg_stat_user_indexes".into(),
        }];

        let out = format_health(&report);
        assert!(out.contains("public.events"), "{out}");
        assert!(out.contains("public.order_items"), "{out}");
        assert!(out.contains("Bad"), "{out}");
        // The remediation is text, and says so, so nothing reads it as applied.
        assert!(out.contains("NOT run"), "{out}");
        assert!(out.contains("needs pg_stat_user_indexes"), "{out}");
        assert!(out.contains("absence proves nothing"), "{out}");
    }

    #[test]
    fn session_list_reports_hidden_statements_as_hidden() {
        use red_core::{ServerSession, SessionKey};

        let sessions = vec![
            ServerSession {
                key: SessionKey("101".into()),
                user: Some("reporting".into()),
                application: Some("psql".into()),
                client_addr: Some("10.0.0.9".into()),
                database: Some("shop".into()),
                state: "active".into(),
                wait: None,
                blocked_by: vec![SessionKey("77".into())],
                query: Some("SELECT * FROM orders".into()),
                elapsed_secs: 12.5,
                is_self: false,
            },
            ServerSession {
                key: SessionKey("102".into()),
                user: None,
                application: None,
                client_addr: None,
                database: None,
                state: "idle".into(),
                wait: Some("Lock:transactionid".into()),
                blocked_by: Vec::new(),
                query: None,
                elapsed_secs: 0.2,
                is_self: true,
            },
        ];
        let out = format_sessions(&sessions, true);
        assert!(
            out.contains("[101]") && out.contains("user=reporting"),
            "{out}"
        );
        assert!(out.contains("blocked by 77"), "{out}");
        assert!(out.contains("waiting on Lock:transactionid"), "{out}");
        assert!(out.contains("RED's own connection"), "{out}");
        // A statement the role may not read is reported as hidden, not as absent.
        assert!(out.contains("not visible to this role"), "{out}");
        assert!(out.contains("hidden rather than absent"), "{out}");
    }
}
