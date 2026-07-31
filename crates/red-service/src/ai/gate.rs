//! The write gate: the single source of truth for which tools may mutate, and
//! what the user is shown before one runs.
//!
//! Two rules hold the whole thing up. First, [`READ_ONLY_TOOLS`] is an
//! **allowlist** and [`is_write_tool`] is its complement, so a tool nobody
//! remembers to classify fails *closed* -- gated, and withheld from the
//! MCP/ACP paths -- rather than slipping through a denylist someone forgot to
//! extend. Second, [`assess_write`] is called from both the turn loop (to decide
//! reject vs. prompt) and each seam's executor (to re-validate before running),
//! so the gate the user sees and the gate the write rides cannot drift apart.

use red_ai::ToolDef;
use red_core::sql::{DANGEROUS_FNS, Dialect, WRITE_TOKENS, has_word, strip_noise};
use red_core::{AiPolicy, AiTier};
use serde_json::Value as Json;

use super::doc::write::{assess_doc_write, is_doc_write_tool};
use super::kv::write::{assess_kv_write, is_kv_write_tool};
use super::sql::format::qualified;
use super::sql::tools::index_args;
use super::util::truncate_summary;

/// Apply the two membership gates every seam's catalog shares: the tier decides
/// which tools exist at all, and any write tool is additionally withheld on a
/// read-only connection so it's never even offered there. One helper
/// so the SQL, KV, and doc catalogs gate identically.
pub(in crate::ai) fn gate_catalog(
    all: impl IntoIterator<Item = ToolDef>,
    policy: &AiPolicy,
) -> Vec<ToolDef> {
    all.into_iter()
        .filter(|t| {
            policy.tier.allows_tool(&t.name) && !(policy.read_only && is_write_tool(&t.name))
        })
        .collect()
}
/// Narrow a parent catalog to what a delegated subagent may use: minus every
/// write tool and minus `spawn_subagent` itself, so a child can neither mutate
/// data nor recurse. Narrows (never widens) the parent's tier — even a Write-tier
/// parent yields a read-only child. The security-critical "read-only,
/// non-recursive child" rule, in one place for all three seams.
pub(super) fn narrow_to_subagent(catalog: Vec<ToolDef>) -> Vec<ToolDef> {
    catalog
        .into_iter()
        .filter(|t| t.name != "spawn_subagent" && !is_write_tool(&t.name))
        .collect()
}
/// A conservative read-only gate: the statement must be a single SELECT or a CTE
/// that resolves to a SELECT, with no statement separator and no embedded write.
///
/// `run_select` runs on the *user's* connection, which is writable unless the
/// connection itself was opened read-only, so this gate, not the engine, is what
/// keeps a read-tier agent from mutating data. A naive "starts with SELECT/WITH"
/// check is not enough: Postgres executes **data-modifying CTEs**
/// (`WITH x AS (DELETE … RETURNING …) SELECT * FROM x`), and `SELECT … INTO` /
/// `INTO OUTFILE` and sequence-advancing functions also write while leading with
/// SELECT. So, like [`write_shape`], we reason about a **noise-stripped** copy
/// (literals/quoted-identifiers/comments blanked) and reject any surviving write
/// keyword. The stripping, the whole-word test, and both token lists live in
/// `red_core::sql`, so this gate, the UI's `is_read_only`, and [`write_shape`] cannot
/// drift apart. False positives (a rejected legitimate read) are acceptable: the user can
/// always run such a query by hand in a query tab. (Defense in depth: opening the
/// AI's reads on an engine-level read-only connection would make this belt-and-
/// suspenders: a worthwhile follow-up, but it needs a per-call driver seam.)
pub(in crate::ai) fn is_read_only_select(sql: &str, dialect: Dialect) -> bool {
    let stripped = strip_noise(sql, dialect);
    let trimmed = stripped.trim().trim_end_matches(';').trim();
    if trimmed.is_empty() {
        return false;
    }
    // No embedded statement terminator (a `;` could chain a write past the prefix).
    if trimmed.contains(';') {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    if !(lower.starts_with("select") || lower.starts_with("with")) {
        return false;
    }
    // A statement that *starts* SELECT/WITH can still write. Reject if any write
    // keyword survives noise-stripping as a whole-word token: the data-modifying
    // CTE verbs (Postgres runs these), `INTO` (`SELECT … INTO new_table` /
    // `INTO OUTFILE`/`DUMPFILE`), and the sequence-advancing functions. These verbs
    // are reserved words, so they can't be bare column names in a real read; a
    // column legitimately named one of them would be quoted, and quoting blanks it
    // out before this check. (`FOR UPDATE` locking reads trip `update` and are
    // rejected too; fine, the assistant browses, it doesn't lock.)
    !WRITE_TOKENS
        .iter()
        .chain(DANGEROUS_FNS)
        .any(|w| has_word(&lower, w))
}
/// The tools that never mutate data and so may run on any backend without the
/// per-call write gate. This is an allowlist on purpose: anything *not* named here
/// is treated as a write, so a future tool fails *closed* (gated, withheld from the
/// MCP/ACP path) until it's explicitly vetted and added, rather than slipping
/// through a denylist someone forgot to extend.
pub(in crate::ai) const READ_ONLY_TOOLS: &[&str] = &[
    "list_schema",
    "describe_table",
    "relationship_map",
    "object_ddl",
    "profile_table",
    "run_select",
    // Reads the next window of a `run_select` already in flight.
    "fetch_more",
    "search_data",
    // Plans; with `analyze` it also *runs* the statement, which is why that
    // branch refuses anything `risk::assess` grades above Safe.
    "explain",
    "health_report",
    "server_sessions",
    "diff_schema",
    "diff_data",
    "suggest_index",
    "export_result",
    "generate_report",
    // Hands the user a SQL query to open in a tab; no DB mutation of its own.
    "open_query",
    // Writes a `.sql` file to the user's saved-queries library; no DB mutation.
    "save_query",
    // Hands the UI a draft knowledge file to open for review; writes nothing at
    // all (not even a file) and never touches the database.
    "save_knowledge",
    // Grounding: these read the app's own stores (query history, saved queries,
    // recently-viewed keys). No driver call at all, so nothing to mutate.
    "search_query_history",
    "list_saved_queries",
    "read_saved_query",
    "kv_recent_keys",
    // Redis (KV) read tools: pure reads through the `KvDriver` seam.
    "kv_server_info",
    "kv_scan_keys",
    "kv_key_info",
    "kv_key_schema",
    "kv_get_value",
    "kv_read_collection",
    "kv_stream_groups",
    "kv_biggest_keys",
    "kv_analyze",
    "kv_slowlog",
    "kv_client_list",
    "kv_config_get",
    "kv_keyspace_notifications",
    // MongoDB (doc) read tools: pure reads through the `DocDriver` seam. The
    // signature tools (`profile_collection`/`audit_collection`/`index_advice`)
    // are host-side compositions over the read methods, so they're reads too.
    "doc_server_info",
    "list_collections",
    "describe_collection",
    "doc_reference_map",
    "profile_collection",
    "sample_documents",
    "get_document",
    "find",
    "aggregate",
    "count",
    "distinct",
    "explain_query",
    "index_advice",
    "audit_collection",
    "doc_current_op",
];
/// Whether `name` is a mutating tool: it never auto-runs and never auto-allows;
/// it rides the per-call approval gate on both backends. Defined as the
/// complement of [`READ_ONLY_TOOLS`] so a new, unlisted tool is treated as a write.
pub(crate) fn is_write_tool(name: &str) -> bool {
    !READ_ONLY_TOOLS.contains(&name)
}
/// Tools that don't mutate the database but assume a running GUI: they emit UI
/// events (`open_query` opens a tab) or write into the app's on-disk libraries
/// (`save_query`, `generate_report`) for the app to surface. They're meaningless
/// over the headless `red mcp` stdio transport, so that path drops them from the
/// advertised catalog and refuses a call to them.
const UI_ONLY_TOOLS: &[&str] = &[
    "open_query",
    "save_query",
    "generate_report",
    // The draft is only useful if there's an editor to review it in; a headless
    // caller has nowhere to put it and no user to check it.
    "save_knowledge",
    // Writes a file into the app's output folder and announces it as a card for
    // the user to open. Over a headless transport there is nobody to hand it to,
    // and the folder is the app's, not the caller's.
    "export_result",
];
/// Whether a call to `name` produces data an answer could cite, and therefore
/// earns a source number.
///
/// A read that returns facts qualifies; chrome does not. `open_query` hands the
/// user a tab, `save_query` and `save_knowledge` write a file, `generate_report`
/// and `export_result` produce a document -- none of them is evidence for a
/// sentence, and listing them under "Sources" would pad the count with things
/// nobody can check a number against. Writes are excluded for the same reason:
/// they change the world rather than describe it.
///
/// Defined over [`READ_ONLY_TOOLS`] minus that chrome, so a new read is a source
/// by default and a new piece of chrome has to be named here. That direction is
/// deliberate: an uncited source understates provenance, while a source that
/// isn't evidence overstates it.
pub(crate) fn is_source_tool(name: &str) -> bool {
    const NOT_EVIDENCE: &[&str] = &[
        "open_query",
        "save_query",
        "save_knowledge",
        "generate_report",
        "export_result",
        // A subagent's individual reads never cross back to the parent; only its
        // report does, so the delegation is not a citable source of its own.
        "spawn_subagent",
    ];
    !is_write_tool(name) && !NOT_EVIDENCE.contains(&name)
}
/// Whether `name` may run over the headless `red mcp` transport: a read-only tool
/// that isn't one of the GUI-only [`UI_ONLY_TOOLS`]. Writes are already excluded
/// by [`is_write_tool`]; this additionally drops the UI-bound reads.
pub(crate) fn is_headless_tool(name: &str) -> bool {
    !is_write_tool(name) && !UI_ONLY_TOOLS.contains(&name)
}
/// The outcome of vetting a `propose_write` call before it runs. The
/// single source of truth, called by `run_turn` (to decide reject vs. prompt) and
/// by `run_tool` (to re-validate before executing). Keeping it in one place means
/// the gate the user sees and the gate the write rides can't drift apart.
pub(in crate::ai) enum WriteAssessment {
    /// Not a write tool; run it normally (no approval).
    NotWrite,
    /// Blocked outright (wrong tier, read-only connection, or a destructive shape):
    /// report this to the model without prompting the user.
    Reject(String),
    /// An allowed single INSERT/UPDATE/DELETE: prompt the user with this exact SQL,
    /// and only run it on Allow.
    NeedsApproval { sql: String },
}
/// Vet a tool call for the write gate. A `propose_write` is allowed only at the
/// `Write` tier, on a writable connection, and for a safe statement shape; anything
/// else is rejected (never silently run, never even prompted).
pub(in crate::ai) fn assess_write(
    name: &str,
    input: &Json,
    policy: &AiPolicy,
    dialect: Dialect,
) -> WriteAssessment {
    if !is_write_tool(name) {
        return WriteAssessment::NotWrite;
    }
    if policy.tier != AiTier::Write {
        return WriteAssessment::Reject(
            "the write tool is not available at this access tier".into(),
        );
    }
    if policy.read_only {
        return WriteAssessment::Reject(
            "this connection is read-only: writes are disabled. Tell the user; do not retry."
                .into(),
        );
    }
    if is_kv_write_tool(name) {
        return assess_kv_write(name, input);
    }
    if is_doc_write_tool(name) {
        return assess_doc_write(name, input);
    }
    if name == "propose_changeset" {
        return assess_changeset(input, dialect);
    }
    // Not SQL, so `write_shape` has nothing to lex: a kill is graded by what it
    // stops, and the prompt has to say what that is.
    if name == "kill_session" {
        return assess_kill_session(input);
    }
    // The one DDL the agent may run. It is deliberately carved out of
    // `write_shape`'s blanket DDL block rather than loosening it: an index is
    // additive and reversible, a DROP/TRUNCATE/ALTER is not, and widening the
    // block would let all three through.
    if name == "create_index" {
        return match index_args(input) {
            Ok((table, index, columns, unique)) => WriteAssessment::NeedsApproval {
                sql: format!(
                    "CREATE{} INDEX {index} ON {} ({})\nBuilding an index locks and loads the \
                     server for the duration; it is reversible with a DROP INDEX afterwards.",
                    if unique { " UNIQUE" } else { "" },
                    qualified(table.schema.as_deref(), &table.name),
                    columns.join(", "),
                ),
            },
            Err(why) => WriteAssessment::Reject(why),
        };
    }
    let sql = input.get("sql").and_then(Json::as_str).unwrap_or("").trim();
    match write_shape(sql, dialect) {
        WriteShape::Ok => WriteAssessment::NeedsApproval {
            sql: sql.to_string(),
        },
        WriteShape::NotWrite => WriteAssessment::Reject(
            "propose_write is only for INSERT/UPDATE/DELETE; use run_select to read".into(),
        ),
        WriteShape::Blocked(why) => WriteAssessment::Reject(why.into()),
    }
}
/// Vet a `kill_session` for the approval gate. Not a statement, so there is no
/// shape to lex; what matters is that the prompt names the *target* — the
/// session, whose it is, and what it is running — because "terminate session
/// 4711" alone is not something anyone can meaningfully approve.
fn assess_kill_session(input: &Json) -> WriteAssessment {
    let Some(key) = input
        .get("key")
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|k| !k.is_empty())
    else {
        return WriteAssessment::Reject(
            "kill_session needs the `key` of a session from server_sessions".into(),
        );
    };
    let mode = match kill_mode(input) {
        Ok(m) => m,
        Err(why) => return WriteAssessment::Reject(why),
    };
    let who = input
        .get("user")
        .and_then(Json::as_str)
        .filter(|u| !u.is_empty())
        .map(|u| format!(" (user {u})"))
        .unwrap_or_default();
    let mut op = format!("{} `{key}`{who}", mode.verb());
    if mode == red_core::KillMode::Terminate {
        op.push_str("\n\u{26a0} Terminating rolls back this session's open transaction.");
    }
    match input
        .get("statement")
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(sql) => op.push_str(&format!("\nRunning: {}", truncate_summary(sql, 300))),
        None => op.push_str(
            "\nThe agent did not say what this session is running; read server_sessions before \
             allowing.",
        ),
    }
    WriteAssessment::NeedsApproval { sql: op }
}
/// The [`KillMode`](red_core::KillMode) a `kill_session`/`doc_kill_op` input
/// names, defaulting to the reversible one. An unrecognized spelling is an error
/// rather than a guess: guessing wrong here means terminating a session the user
/// only meant to interrupt.
pub(in crate::ai) fn kill_mode(input: &Json) -> Result<red_core::KillMode, String> {
    match input.get("mode").and_then(Json::as_str).unwrap_or("cancel") {
        "cancel" => Ok(red_core::KillMode::Cancel),
        "terminate" => Ok(red_core::KillMode::Terminate),
        other => Err(format!(
            "kill mode must be \"cancel\" or \"terminate\", not `{other}`"
        )),
    }
}
/// The statements of a `propose_changeset` call: the non-empty, trimmed entries of
/// its `statements` array, in order.
pub(in crate::ai) fn changeset_statements(input: &Json) -> Vec<String> {
    input
        .get("statements")
        .and_then(Json::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}
/// Vet a `propose_changeset`: every statement must pass the same shape gate as a
/// single write (DML only, WHERE required, no DDL, no chaining). Any failure rejects
/// the *whole* changeset — it's atomic, so a bad statement means nothing runs. On
/// success the approval prompt shows the numbered statements as one reviewable unit.
fn assess_changeset(input: &Json, dialect: Dialect) -> WriteAssessment {
    let statements = changeset_statements(input);
    if statements.is_empty() {
        return WriteAssessment::Reject(
            "propose_changeset needs a non-empty `statements` array of INSERT/UPDATE/DELETE \
             statements"
                .into(),
        );
    }
    for (i, stmt) in statements.iter().enumerate() {
        match write_shape(stmt, dialect) {
            WriteShape::Ok => {}
            WriteShape::NotWrite => {
                return WriteAssessment::Reject(format!(
                    "statement {} is not an INSERT/UPDATE/DELETE; a changeset only modifies data",
                    i + 1
                ));
            }
            WriteShape::Blocked(why) => {
                return WriteAssessment::Reject(format!("statement {}: {why}", i + 1));
            }
        }
    }
    // Numbered, one per line: the exact set the user approves as a unit.
    let body = statements
        .iter()
        .enumerate()
        .map(|(i, s)| format!("{}. {s}", i + 1))
        .collect::<Vec<_>>()
        .join("\n");
    WriteAssessment::NeedsApproval { sql: body }
}
/// The shape verdict for a candidate write statement.
enum WriteShape {
    /// A single, qualified INSERT/UPDATE/DELETE: eligible (still needs approval).
    Ok,
    /// Not a write at all (SELECT/WITH/empty).
    NotWrite,
    /// A shape blocked even with approval, with the reason to report.
    Blocked(&'static str),
}
/// Classify a candidate write conservatively. The hard blocks (DDL and
/// privilege statements, an unqualified UPDATE/DELETE with no WHERE, and any chained
/// statement) are the cases per-call approval alone shouldn't be trusted to catch
/// (a rubber-stamped `DELETE` with no WHERE is catastrophic). False negatives are
/// fine: the user can always run those by hand in a query tab.
///
/// Classification runs on a **noise-stripped** copy (string literals, quoted
/// identifiers, and comments blanked) so a keyword or `;` *inside a literal* can't
/// fool the gate; e.g. `UPDATE t SET note = 'see where'` (no real WHERE) is still
/// blocked, and a `;` inside a string isn't read as statement chaining.
fn write_shape(sql: &str, dialect: Dialect) -> WriteShape {
    let stripped = strip_noise(sql, dialect);
    let trimmed = stripped.trim().trim_end_matches(';').trim();
    if trimmed.is_empty() {
        return WriteShape::Blocked("the statement is empty");
    }
    // No embedded terminator: a real `;` chains a second statement past the keyword
    // check (and past the user's eyes).
    if trimmed.contains(';') {
        return WriteShape::Blocked("multiple statements are not allowed; submit one at a time");
    }
    let lower = trimmed.to_ascii_lowercase();
    let first = lower.split_whitespace().next().unwrap_or("");
    match first {
        "select" | "with" => WriteShape::NotWrite,
        "insert" => WriteShape::Ok,
        "update" | "delete" => {
            // Require a real WHERE keyword (a word token, not a substring) so a
            // whole-table mutation can't slip through.
            if has_word(&lower, "where") {
                WriteShape::Ok
            } else {
                WriteShape::Blocked(
                    "an UPDATE/DELETE without a WHERE clause is blocked; add a WHERE, or run a \
                     full-table change yourself in a query tab",
                )
            }
        }
        // DROP / TRUNCATE / ALTER / CREATE / RENAME / GRANT / REVOKE / …: DDL and
        // privilege changes are never run through the assistant.
        _ => WriteShape::Blocked(
            "only INSERT/UPDATE/DELETE are allowed here; DDL (DROP/TRUNCATE/ALTER/…) must be run \
             manually in a query tab",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::ai::doc::catalog::doc_tool_catalog;
    use crate::ai::kv::catalog::kv_tool_catalog;
    use crate::ai::sql::catalog::tool_catalog;

    use crate::ai::testutil::assess_write;

    use serde_json::json;

    /// The gate tests probe statement shapes, not dialect lexing, so they run
    /// under [`Dialect::Generic`]; the dialect-sensitive cases have their own
    /// tests below.
    fn is_read_only_select(sql: &str) -> bool {
        super::is_read_only_select(sql, Dialect::Generic)
    }

    /// Postgres does not backslash-escape in a plain literal, so `'a\'` is a
    /// complete string and the `DELETE` after it is live SQL: the gate must see
    /// it. Under the old unconditional-backslash lexing the whole payload
    /// blanked as one string and *passed* the read gate.
    #[test]
    fn read_gate_lexes_strings_per_dialect() {
        let payload = "SELECT 'a\\'; DELETE FROM t; --'";
        assert!(!super::is_read_only_select(payload, Dialect::Postgres));
        // Under MySQL the backslash escapes the quote, so it really is one
        // SELECT with a string argument — allowed.
        assert!(super::is_read_only_select(payload, Dialect::MySql));
    }

    #[test]
    fn read_only_gate_rejects_writes_and_chains() {
        assert!(is_read_only_select("SELECT 1"));
        assert!(is_read_only_select(
            "  with x as (select 1) select * from x  "
        ));
        assert!(is_read_only_select("select 1;"));
        assert!(!is_read_only_select("UPDATE t SET x=1"));
        assert!(!is_read_only_select("DELETE FROM t"));
        assert!(!is_read_only_select("select 1; drop table t"));
        assert!(!is_read_only_select(""));
    }

    #[test]
    fn read_only_gate_rejects_data_modifying_ctes_and_select_into() {
        // A data-modifying CTE leads with WITH but Postgres executes the DELETE.
        assert!(!is_read_only_select(
            "WITH x AS (DELETE FROM t RETURNING *) SELECT * FROM x"
        ));
        assert!(!is_read_only_select(
            "with g as (update t set a=1 returning id) select * from g"
        ));
        assert!(!is_read_only_select(
            "WITH n AS (INSERT INTO t VALUES (1) RETURNING *) SELECT * FROM n"
        ));
        // SELECT … INTO (Postgres creates a table) / INTO OUTFILE (MySQL writes a file).
        assert!(!is_read_only_select("SELECT * INTO new_t FROM t"));
        assert!(!is_read_only_select(
            "SELECT * FROM t INTO OUTFILE '/tmp/x'"
        ));
        // Sequence-advancing functions write.
        assert!(!is_read_only_select("SELECT nextval('s')"));
        assert!(!is_read_only_select("select setval('s', 1)"));
        // Server-side functions that read/write files or run remote SQL are refused.
        assert!(!is_read_only_select("SELECT lo_import('/etc/passwd')"));
        assert!(!is_read_only_select("SELECT pg_read_file('/etc/passwd')"));
        assert!(!is_read_only_select(
            "SELECT dblink_exec('dbname=x', 'DELETE FROM t')"
        ));
        // Bare and async `dblink` run arbitrary remote SQL just like `dblink_exec`.
        assert!(!is_read_only_select(
            "SELECT * FROM dblink('dbname=x', 'DELETE FROM t RETURNING id') AS r(id int)"
        ));
        assert!(!is_read_only_select(
            "SELECT dblink_send_query('c', 'DELETE FROM t')"
        ));
        assert!(!is_read_only_select("select load_file('/etc/passwd')"));
        // A write keyword merely *inside a literal or quoted identifier* is harmless
        // and must NOT block a real read (noise is stripped before the check).
        assert!(is_read_only_select("SELECT 'delete me' AS note FROM t"));
        assert!(is_read_only_select(r#"SELECT "update" FROM t"#));
        assert!(is_read_only_select("SELECT id FROM t WHERE c = 'a;b'"));
    }

    #[test]
    fn write_gate_blocks_dangerous_shapes_and_allows_qualified() {
        let write = AiPolicy {
            tier: AiTier::Write,
            ..AiPolicy::default()
        };
        let assess = |sql: &str| assess_write("propose_write", &json!({ "sql": sql }), &write);
        let allowed = |sql: &str| matches!(assess(sql), WriteAssessment::NeedsApproval { .. });
        let rejected = |sql: &str| matches!(assess(sql), WriteAssessment::Reject(_));

        // Qualified writes are eligible (they still need approval).
        assert!(allowed("INSERT INTO t (a) VALUES (1)"));
        assert!(allowed("UPDATE t SET a = 1 WHERE id = 5"));
        assert!(allowed("DELETE FROM t WHERE id = 5"));
        // Unqualified mass mutations are hard-blocked.
        assert!(rejected("UPDATE t SET a = 1"));
        assert!(rejected("DELETE FROM t"));
        // DDL / privilege statements are never run via the tool.
        assert!(rejected("DROP TABLE t"));
        assert!(rejected("TRUNCATE t"));
        assert!(rejected("ALTER TABLE t ADD c int"));
        // No chaining a second statement past the gate.
        assert!(rejected("UPDATE t SET a=1 WHERE id=1; DROP TABLE t"));
        // A read query isn't a write.
        assert!(rejected("SELECT * FROM t"));
        // A `where` inside a string literal or comment is NOT a real WHERE; the
        // statement is still an unqualified mutation and must be blocked.
        assert!(rejected("UPDATE t SET note = 'see where you go'"));
        assert!(rejected("DELETE FROM t -- delete where id = 1"));
        // Conversely, a real WHERE with a `;` inside a string literal is a single,
        // qualified statement: allowed (the `;` isn't statement chaining).
        assert!(allowed("UPDATE t SET note = 'a;b' WHERE id = 1"));
    }

    #[test]
    fn write_gate_respects_tier_and_read_only() {
        let qualified = json!({ "sql": "DELETE FROM t WHERE id = 1" });
        // Below the Write tier the write tool is rejected outright.
        let read = AiPolicy::default();
        assert!(matches!(
            assess_write("propose_write", &qualified, &read),
            WriteAssessment::Reject(_)
        ));
        // A read-only connection rejects it even at the Write tier.
        let read_only = AiPolicy {
            tier: AiTier::Write,
            read_only: true,
            ..AiPolicy::default()
        };
        assert!(matches!(
            assess_write("propose_write", &qualified, &read_only),
            WriteAssessment::Reject(_)
        ));
        // A read tool is never gated as a write.
        assert!(matches!(
            assess_write("run_select", &json!({ "sql": "SELECT 1" }), &read),
            WriteAssessment::NotWrite
        ));
    }

    #[test]
    fn changeset_assessment_gates_shape_tier_and_read_only() {
        let write = AiPolicy {
            tier: AiTier::Write,
            ..AiPolicy::default()
        };
        let ok = json!({ "statements": [
            "INSERT INTO t VALUES (1)",
            "UPDATE t SET a = 1 WHERE id = 1",
        ] });

        // A valid set at the Write tier needs approval; the prompt body numbers each.
        match assess_write("propose_changeset", &ok, &write) {
            WriteAssessment::NeedsApproval { sql } => {
                assert!(sql.contains("1. INSERT"), "got: {sql}");
                assert!(sql.contains("2. UPDATE"), "got: {sql}");
            }
            _ => panic!("expected NeedsApproval for a valid changeset"),
        }

        // Below the Write tier the whole tool is refused.
        assert!(matches!(
            assess_write("propose_changeset", &ok, &AiPolicy::default()),
            WriteAssessment::Reject(_)
        ));
        // A read-only connection refuses even at the Write tier.
        let read_only = AiPolicy {
            tier: AiTier::Write,
            read_only: true,
            ..AiPolicy::default()
        };
        assert!(matches!(
            assess_write("propose_changeset", &ok, &read_only),
            WriteAssessment::Reject(_)
        ));
        // One bad statement (DDL) rejects the whole set — it's atomic.
        let ddl = json!({ "statements": ["INSERT INTO t VALUES (1)", "DROP TABLE t"] });
        assert!(matches!(
            assess_write("propose_changeset", &ddl, &write),
            WriteAssessment::Reject(_)
        ));
        // An unqualified UPDATE/DELETE is blocked.
        let nowhere = json!({ "statements": ["DELETE FROM t"] });
        assert!(matches!(
            assess_write("propose_changeset", &nowhere, &write),
            WriteAssessment::Reject(_)
        ));
        // An empty set is refused.
        let empty = json!({ "statements": [] });
        assert!(matches!(
            assess_write("propose_changeset", &empty, &write),
            WriteAssessment::Reject(_)
        ));
    }

    #[test]
    fn headless_transport_keeps_reads_drops_writes_and_gui_tools() {
        // The `red mcp` stdio transport advertises/runs only DB reads that work
        // without the GUI: writes and the UI-bound reads are withheld.
        assert!(is_headless_tool("run_select"));
        assert!(is_headless_tool("list_schema"));
        assert!(is_headless_tool("kv_get_value"));
        // Writes stay out (they're not in READ_ONLY_TOOLS).
        assert!(!is_headless_tool("propose_write"));
        assert!(!is_headless_tool("kv_delete"));
        // GUI-only reads are withheld even though they don't mutate the DB.
        for t in UI_ONLY_TOOLS {
            assert!(
                !is_headless_tool(t),
                "{t} needs the GUI; withhold it headless"
            );
        }
    }

    /// `is_write_tool` is the complement of an allowlist, so a read nobody
    /// remembered to list is silently gated as a write (and withheld from MCP).
    /// Assert the membership *and* the fail-closed property it rests on.
    #[test]
    fn new_reads_are_listed_and_an_unlisted_name_fails_closed() {
        for t in [
            "relationship_map",
            "object_ddl",
            "search_data",
            "health_report",
            "server_sessions",
            "diff_schema",
            "diff_data",
            "suggest_index",
            "export_result",
            "kv_key_schema",
            "kv_read_collection",
            "kv_stream_groups",
            "kv_client_list",
            "kv_keyspace_notifications",
            "doc_reference_map",
            "get_document",
            "doc_current_op",
        ] {
            assert!(!is_write_tool(t), "{t} must be in READ_ONLY_TOOLS");
        }
        // The property itself: an unlisted name is a write, so a future tool is
        // gated until someone vets it rather than slipping through.
        assert!(is_write_tool("some_tool_nobody_listed"));
        // And the new writes are writes, so none is auto-allowed over ACP/MCP.
        for t in [
            "kill_session",
            "create_index",
            "kv_set",
            "kv_copy_key",
            "kv_client_kill",
            "kv_command",
            "doc_kill_op",
        ] {
            assert!(is_write_tool(t), "{t} must be gated as a write");
            assert!(!is_headless_tool(t), "{t} must not be offered headlessly");
            assert!(!AiTier::Read.allows_tool(t), "{t} must not exist at Read");
            assert!(AiTier::Write.allows_tool(t), "{t} must exist at Write");
        }
        // The UI-bound reads are reads, but still withheld from the headless
        // transport: there is no app there to hand a tab or a file to.
        for t in [
            "export_result",
            "open_query",
            "save_query",
            "generate_report",
            "save_knowledge",
        ] {
            assert!(!is_write_tool(t));
            assert!(!is_headless_tool(t), "{t} is GUI-bound");
        }
        // `save_knowledge` writes nothing (not even a file): it hands the UI a
        // draft to open for review, so it must never be gated as a write and must
        // never exist below Read, where the agent can't sample a value to learn
        // anything from.
        assert!(!AiTier::Schema.allows_tool("save_knowledge"));
        assert!(AiTier::Read.allows_tool("save_knowledge"));
        assert!(AiTier::Write.allows_tool("save_knowledge"));
    }

    /// A source number says "you can check this number against that call", so it
    /// belongs to reads that return facts and to nothing else. Padding the list
    /// with chrome would inflate a count the reader is meant to trust.
    #[test]
    fn only_data_returning_reads_earn_a_source_number() {
        for tool in [
            "run_select",
            "fetch_more",
            "profile_table",
            "search_data",
            "explain",
            "list_schema",
            "describe_table",
            "relationship_map",
            "search_query_history",
            "kv_get_value",
            "find",
            "aggregate",
        ] {
            assert!(is_source_tool(tool), "{tool} produces citable data");
        }
        // Chrome: hands the user a tab, a file or a document, none of which is
        // evidence for a sentence.
        for tool in [
            "open_query",
            "save_query",
            "save_knowledge",
            "generate_report",
            "export_result",
            "spawn_subagent",
        ] {
            assert!(!is_source_tool(tool), "{tool} is not evidence");
        }
        // Writes change the world rather than describing it.
        for tool in [
            "propose_write",
            "propose_changeset",
            "kv_set",
            "create_index",
        ] {
            assert!(!is_source_tool(tool), "{tool} is a write");
        }
        // Fails closed the useful way round: an unknown name is a write, and a
        // write is never a source.
        assert!(!is_source_tool("some_tool_nobody_listed"));
    }

    /// A kill prompt has to say *what* is being stopped. "Terminate session 4711"
    /// is not something anyone can meaningfully approve.
    #[test]
    fn kill_prompts_name_their_target() {
        let write = AiPolicy {
            tier: AiTier::Write,
            ..AiPolicy::default()
        };
        let detail = |name: &str, input: Json| match assess_write(name, &input, &write) {
            WriteAssessment::NeedsApproval { sql } => sql,
            _ => panic!("{name} must prompt"),
        };
        let sql = detail(
            "kill_session",
            json!({
                "key": "4711", "mode": "terminate",
                "user": "reporting", "statement": "SELECT * FROM huge",
            }),
        );
        assert!(sql.contains("Terminate session"), "{sql}");
        assert!(sql.contains("4711") && sql.contains("reporting"), "{sql}");
        assert!(sql.contains("SELECT * FROM huge"), "{sql}");
        // Terminate says what it costs; cancel does not claim to.
        assert!(sql.contains("rolls back"), "{sql}");
        assert!(
            !detail("kill_session", json!({ "key": "4711" })).contains("rolls back"),
            "a cancel must not claim to roll anything back"
        );
        // Missing context is stated rather than papered over.
        assert!(
            detail("kill_session", json!({ "key": "4711" })).contains("did not say"),
            "an unexplained kill must say so"
        );

        let kv = detail(
            "kv_client_kill",
            json!({ "id": 12, "addr": "10.0.0.2:6379", "cmd": "keys" }),
        );
        assert!(kv.contains("CLIENT KILL ID 12"), "{kv}");
        assert!(kv.contains("10.0.0.2:6379") && kv.contains("keys"), "{kv}");

        let doc = detail(
            "doc_kill_op",
            json!({ "opid": 88, "namespace": "app.orders", "command": "{\"find\":\"orders\"}" }),
        );
        assert!(doc.contains("KILL operation 88"), "{doc}");
        assert!(doc.contains("app.orders"), "{doc}");
        assert!(doc.contains("NOT rolled back"), "{doc}");

        // A kill with no target is refused outright, never prompted.
        for (name, input) in [
            ("kill_session", json!({})),
            ("kv_client_kill", json!({})),
            ("doc_kill_op", json!({})),
            ("kill_session", json!({ "key": "1", "mode": "obliterate" })),
        ] {
            assert!(
                matches!(
                    assess_write(name, &input, &write),
                    WriteAssessment::Reject(_)
                ),
                "{name} with {input} must be refused"
            );
        }
    }

    /// `create_index` deliberately widens the blanket DDL block. Assert the
    /// widening is exactly one statement kind wide: the DDL that destroys is
    /// still refused through `propose_write`.
    #[test]
    fn create_index_is_the_only_ddl_the_agent_may_run() {
        let write = AiPolicy {
            tier: AiTier::Write,
            ..AiPolicy::default()
        };
        match assess_write(
            "create_index",
            &json!({ "schema": "public", "table": "orders", "name": "idx_orders_created",
                     "columns": ["created_at"], "unique": true }),
            &write,
        ) {
            WriteAssessment::NeedsApproval { sql } => {
                assert!(
                    sql.contains("CREATE UNIQUE INDEX idx_orders_created"),
                    "{sql}"
                );
                assert!(sql.contains("public.orders"), "{sql}");
                assert!(sql.contains("created_at"), "{sql}");
                // The cost is named: an index build is not free on a live server.
                assert!(sql.contains("locks and loads"), "{sql}");
            }
            _ => panic!("create_index must prompt"),
        }
        // Destructive DDL is still blocked at the shape gate.
        for sql in [
            "DROP TABLE orders",
            "TRUNCATE orders",
            "ALTER TABLE orders DROP COLUMN x",
            "DROP INDEX idx_orders_created",
        ] {
            assert!(
                matches!(
                    assess_write("propose_write", &json!({ "sql": sql }), &write),
                    WriteAssessment::Reject(_)
                ),
                "`{sql}` must stay blocked"
            );
        }
        // And a create_index with no columns is refused rather than prompted.
        assert!(matches!(
            assess_write(
                "create_index",
                &json!({ "table": "orders", "name": "idx", "columns": [] }),
                &write
            ),
            WriteAssessment::Reject(_)
        ));
    }

    #[test]
    fn kv_read_tools_are_not_gated_as_writes() {
        // Regression guard: the KV read tools must be in READ_ONLY_TOOLS, else the
        // write gate would reject every one of them at Read tier.
        let read = AiPolicy::default();
        for t in [
            "kv_server_info",
            "kv_scan_keys",
            "kv_key_info",
            "kv_get_value",
            "kv_biggest_keys",
            "kv_analyze",
            "kv_slowlog",
            "kv_config_get",
        ] {
            assert!(!is_write_tool(t), "{t} must be read-only");
            assert!(
                matches!(
                    assess_write(t, &json!({}), &read),
                    WriteAssessment::NotWrite
                ),
                "{t} must not be gated as a write"
            );
        }
    }

    #[test]
    fn doc_read_tools_are_not_gated_as_writes() {
        // Every doc read tool must be in READ_ONLY_TOOLS, else the write gate
        // would reject it at Read tier.
        let read = AiPolicy::default();
        for t in [
            "doc_server_info",
            "list_collections",
            "describe_collection",
            "profile_collection",
            "sample_documents",
            "find",
            "aggregate",
            "count",
            "distinct",
            "explain_query",
            "index_advice",
            "audit_collection",
        ] {
            assert!(!is_write_tool(t), "{t} must be read-only");
            assert!(
                matches!(
                    assess_write(t, &json!({}), &read),
                    WriteAssessment::NotWrite
                ),
                "{t} must not be gated as a write"
            );
        }
    }

    #[tokio::test]
    async fn structure_maps_are_reads_at_their_stated_tiers() {
        // Two are structure-only (Schema tier and up); the Mongo one samples
        // values, so it starts at Read.
        for t in ["relationship_map", "kv_key_schema"] {
            assert!(!is_write_tool(t), "{t} must be read-only");
            assert!(AiTier::Schema.allows_tool(t), "{t} must exist at Schema");
            assert!(is_headless_tool(t), "{t} must be offered over MCP");
        }
        assert!(!is_write_tool("doc_reference_map"));
        assert!(!AiTier::Schema.allows_tool("doc_reference_map"));
        assert!(AiTier::Read.allows_tool("doc_reference_map"));
        // Each seam's catalog actually offers its own map at Read tier.
        let read = AiPolicy::default();
        assert!(
            tool_catalog(&read)
                .iter()
                .any(|t| t.name == "relationship_map")
        );
        assert!(
            kv_tool_catalog(&read)
                .iter()
                .any(|t| t.name == "kv_key_schema")
        );
        assert!(
            doc_tool_catalog(&read)
                .iter()
                .any(|t| t.name == "doc_reference_map")
        );
    }
}
