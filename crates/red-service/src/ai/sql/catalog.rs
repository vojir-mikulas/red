//! What the SQL agent is told it can do: the tier-filtered tool catalog and the
//! system prompt that introduces it.
//!
//! Filtering happens at *construction* ([`gate_catalog`]), so a tool above the
//! policy's tier is never offered rather than offered and refused. The prompt is
//! courtesy on top of that: the catalog is the real gate. Each description
//! states its **trigger** -- when to call the tool -- because a tool the model
//! never reaches for is a tool that does not exist.

use red_ai::ToolDef;
use red_core::{AiPolicy, AiTier};
use serde_json::json;

use super::super::export::export_tool_def;
use super::super::gate::gate_catalog;
use super::super::grounding::{
    history_tool_def, list_saved_queries_tool_def, read_saved_query_tool_def,
};
use super::super::knowledge::knowledge_tool_def;
use super::super::report::report_tool_def;
use super::super::turn::spawn_subagent_tool_def;
use super::super::util::finish_system_prompt;
use crate::protocol::AiContext;

/// The read-only tool catalog, filtered to the policy's access tier. Each
/// tool is backed by a `DatabaseDriver` method and auto-runs; none can mutate.
/// Filtering happens *here*, at construction, so a tool above the tier is never
/// offered; the model can't call what isn't in the catalog. Shared with the MCP
/// server, so the API-key and subscription/ACP paths expose the identical set.
pub(in crate::ai) fn tool_catalog(policy: &AiPolicy) -> Vec<ToolDef> {
    let max_rows = policy.limits.max_rows;
    let all = [
        ToolDef {
            name: "list_schema".into(),
            description:
                "List the database's schemas and their tables and views (names and kinds \
                only). Call this to discover what objects exist before describing or querying them."
                    .into(),
            input_schema: json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        },
        ToolDef {
            name: "describe_table".into(),
            description: "Get one table or view's columns (name, type, nullability, primary key), \
                foreign keys, and indexes. Use this before writing a query against a table."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "schema": { "type": "string", "description": "Schema/namespace name (e.g. \"main\" or \"public\")." },
                    "table": { "type": "string", "description": "The table or view name." },
                },
                "required": ["schema", "table"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "object_ddl".into(),
            description: "The object's REAL definition, as SQL. describe_table gives columns, keys \
                and indexes but silently drops check constraints, defaults, generated-column \
                expressions, view bodies and trigger source. Call this when the question is \"why \
                does this insert fail\", \"what does this view actually do\", or \"what does this \
                trigger/function contain\" — the DDL is the answer. Nothing is executed."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "schema": { "type": "string", "description": "Schema/namespace name, as reported by list_schema." },
                    "name": { "type": "string", "description": "The object's name." },
                    "kind": {
                        "type": "string",
                        "enum": ["table", "view", "matview", "function", "procedure", "trigger", "sequence", "type"],
                        "description": "The object kind (default \"table\").",
                    },
                },
                "required": ["schema", "name"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "relationship_map".into(),
            description: "The database's foreign-key graph in ONE call: every declared FK edge as \
                `child.column -> parent.column`, plus the tables nothing references and that \
                reference nothing. CALL THIS BEFORE WRITING ANY QUERY THAT JOINS MORE THAN ONE \
                TABLE — it is the verified join graph, so you never have to guess a join key from \
                a column name. Omit both arguments for the whole database."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "schema": { "type": "string", "description": "Restrict to one schema/namespace; omit for all." },
                    "tables": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Restrict to edges touching these tables (either side); omit for all.",
                    },
                },
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "profile_table".into(),
            description: "Profile one table's data: per-column null counts and ratios, distinct \
                counts (with unique-key and constant-column hints), and min/max (plus sum/avg for \
                numeric columns), followed by its foreign-key relationships (outgoing and \
                incoming). One pushed-down aggregate pass per column — it never returns raw rows — \
                so use it to understand a table's shape and data quality before querying, instead \
                of hand-writing count/distinct/min/max SELECTs."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "schema": { "type": "string", "description": "Schema/namespace name (e.g. \"main\" or \"public\"); as reported by list_schema." },
                    "table": { "type": "string", "description": "The table to profile." },
                },
                "required": ["schema", "table"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "run_select".into(),
            description: format!(
                "Run a read-only SELECT (or WITH ... SELECT) query and return up to {max_rows} \
                rows. Non-SELECT statements are rejected. Results are row- and cell-capped and \
                subject to a statement timeout; use LIMIT and targeted columns. This is the only \
                way to read actual data."
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "sql": { "type": "string", "description": "A single SELECT/WITH query." },
                    "limit": {
                        "type": "integer",
                        "description": format!("Max rows to return (1..{max_rows})."),
                    },
                },
                "required": ["sql"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "fetch_more".into(),
            description: "Read the NEXT window of a result you already started with run_select,                 using the cursor handle it gave you. The cursor keeps its place, so the windows                 tile the result exactly - no rows repeated, none skipped, and no re-running the                 query. Never page a large result by rewriting it with OFFSET: that re-executes                 the whole query every time and silently duplicates or drops rows whenever the                 ordering is not total. Reading a window pushes the previous one out of your                 context, so SUMMARIZE AS YOU GO - keep a running tally or a list of what you                 found, rather than trying to hold every window at once."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "cursor": { "type": "string", "description": "The handle run_select returned, e.g. \"c3\"." },
                    "limit": {
                        "type": "integer",
                        "description": format!("Max rows for this window (1..{max_rows}); fewer come back when the window fills its size budget first."),
                    },
                },
                "required": ["cursor"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "search_data".into(),
            description: format!(
                "Find rows anywhere in a table containing `term`, without writing a WHERE clause: \
                it builds a case-insensitive contains-match across every searchable column and \
                returns up to {max_rows} matching rows. Use it for \"where is this value\", \
                \"which row mentions X\", or when you know a value but not which column holds it. \
                Binary/blob columns are skipped."
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "schema": { "type": "string", "description": "Schema/namespace name, as reported by list_schema." },
                    "table": { "type": "string", "description": "The table to search." },
                    "term": { "type": "string", "description": "The text to look for (matched case-insensitively as a substring)." },
                    "limit": {
                        "type": "integer",
                        "description": format!("Max rows to return (1..{max_rows})."),
                    },
                },
                "required": ["schema", "table", "term"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "explain".into(),
            description: "Return the query planner's EXPLAIN output for a SQL statement. By \
                default it only PLANS (nothing executes). Pass `analyze: true` to run the \
                statement and get actual row counts and timings beside the estimates — that \
                comparison is what makes plan reasoning real, and it is the way to prove a bad \
                cardinality estimate. Because EXPLAIN ANALYZE executes, it is allowed for \
                read-only statements ONLY; anything that could write is refused outright."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "sql": { "type": "string", "description": "The SQL to explain." },
                    "analyze": {
                        "type": "boolean",
                        "description": "Run the statement to collect actuals (read-only statements only). Default false.",
                    },
                },
                "required": ["sql"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "health_report".into(),
            description: "A bounded health snapshot of the connection: total and per-table sizes \
                (largest first), plus the findings the engine's catalog supports — unused and \
                redundant indexes, foreign keys with no index on the child side, tables with no \
                primary key, dead tuples/bloat, sequential-scan-heavy tables. It also lists the \
                checks that could NOT run here, so \"no findings\" is never mistaken for a clean \
                bill of health. Every query inside is a bounded catalog read, not a scan. Pair \
                with server_sessions for \"why is this database slow\": this one answers the \
                structural half."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "schema": { "type": "string", "description": "Restrict the report to one schema/namespace; omit for the whole connection." },
                },
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "server_sessions".into(),
            description: "What the server is doing RIGHT NOW: the live sessions longest-running \
                first, with their user, database, state, wait, elapsed time, running statement, \
                and which sessions block which. This is the \"why is it slow right now\" half \
                that health_report cannot answer — a blocked-on-lock wait tree looks nothing \
                like a missing index."
                .into(),
            input_schema: json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        },
        ToolDef {
            name: "diff_schema".into(),
            description: "Compare the STRUCTURE of two schemas in this connection: which objects \
                exist on one side only, and per shared table which columns/indexes/foreign keys \
                were added, removed, or changed. Use it for \"what is different between staging \
                and production\" when both live here. Nothing is executed; the differences come \
                back as text."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "left": { "type": "string", "description": "The baseline schema/namespace." },
                    "right": { "type": "string", "description": "The schema/namespace to compare against it." },
                    "tables": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Restrict the comparison to these tables; omit for all.",
                    },
                },
                "required": ["left", "right"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "diff_data".into(),
            description: "Compare the ROWS of two tables in this connection, aligned on a key \
                column: which keys are only on one side, and which shared keys have differing \
                values (and in which columns). Both tables are read key-ordered and merge-walked, \
                so nothing is materialized. Use it for \"did the copy land\", \"what drifted\", \
                \"which rows differ\"."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "left_schema": { "type": "string", "description": "Schema of the baseline table." },
                    "left_table": { "type": "string", "description": "The baseline table." },
                    "right_schema": { "type": "string", "description": "Schema of the table to compare against it." },
                    "right_table": { "type": "string", "description": "The table to compare against it." },
                    "key": { "type": "string", "description": "The column to align on; omit to use the baseline's single-column primary key." },
                },
                "required": ["left_table", "right_table"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "suggest_index".into(),
            description: "Given a query, decide whether an index would help and emit the CREATE \
                INDEX statement to consider — as TEXT, for the user to read. It explains the \
                query, and if the plan scans, reads the table's existing indexes and columns so \
                the suggestion does not duplicate one that already exists. It does NOT create \
                anything; create_index does that, behind approval."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "sql": { "type": "string", "description": "The slow SELECT to advise on." },
                    "schema": { "type": "string", "description": "Schema of the table the query filters (for the existing-index check)." },
                    "table": { "type": "string", "description": "The table the query filters." },
                },
                "required": ["sql", "table"],
                "additionalProperties": false,
            }),
        },
        export_tool_def(
            "Stream a read-only query's WHOLE result to a file for the user (CSV, JSON, SQL \
             INSERTs, or a standalone HTML table) and hand it over as a card in the chat they can \
             open. Unlike run_select this is not row-capped — the rows go to a file, not to you — \
             so use it when the user asks for an export/download/dump rather than an answer. Only \
             SELECT/WITH queries are accepted.",
            json!({
                "sql": { "type": "string", "description": "A single SELECT/WITH query whose full result is written." },
                "format": {
                    "type": "string",
                    "enum": ["csv", "json", "sql", "html"],
                    "description": "Output format (default \"csv\").",
                },
                "name": { "type": "string", "description": "A short name for the file, e.g. \"monthly-revenue\"." },
            }),
            &["sql"],
        ),
        report_tool_def(),
        knowledge_tool_def(),
        history_tool_def("SQL statements"),
        list_saved_queries_tool_def(),
        read_saved_query_tool_def(),
        ToolDef {
            name: "open_query".into(),
            description: "Open a SQL query in a new editor tab in the user's workspace so they have \
                it in the grid. A read-only SELECT runs automatically; anything else is just loaded \
                for the user to run themselves. Use this to hand the user a query to explore or \
                build on; it does NOT return rows to you (use run_select for that)."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "sql": { "type": "string", "description": "The SQL to open in a new query tab." },
                },
                "required": ["sql"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "save_query".into(),
            description: "Save a REUSABLE SQL query to the user's saved-queries library under a \
                short name, so they can reopen and rerun it later (⇧⌘O). Use this when the user \
                asks for a report/query they'll want again — e.g. \"monthly revenue\" — rather \
                than open_query (which is a one-off tab). For a parametrized query, leave named \
                `:placeholders` in the SQL (e.g. `WHERE month = :month`) and explain them in the \
                description; the user fills them in when they run it. Nothing executes."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "A short, human-readable name (e.g. \"Monthly revenue\")." },
                    "sql": { "type": "string", "description": "The SQL to save, runnable as-is (named :placeholders allowed for parameters)." },
                    "description": { "type": "string", "description": "One line on what it does and any placeholders to fill in; shown in the picker." },
                },
                "required": ["name", "sql"],
                "additionalProperties": false,
            }),
        },
        spawn_subagent_tool_def(),
        ToolDef {
            name: "create_index".into(),
            description: "Create an index, behind the user's explicit approval. This is the one \
                DDL the agent may run: an index is ADDITIVE and reversible, unlike \
                DROP/TRUNCATE/ALTER, which stay blocked. Read suggest_index and describe_table \
                first — building an index on a large table locks and loads the server, so say how \
                big the table is when you propose it."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "schema": { "type": "string", "description": "Schema/namespace of the table." },
                    "table": { "type": "string", "description": "The table to index." },
                    "name": { "type": "string", "description": "The index name." },
                    "columns": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "The columns to index, in order.",
                        "minItems": 1,
                    },
                    "unique": { "type": "boolean", "description": "Create a UNIQUE index. Default false." },
                },
                "required": ["table", "name", "columns"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "kill_session".into(),
            description: "Stop a running server session: `cancel` stops its current statement and \
                keeps the session, `terminate` drops the whole session and ROLLS BACK its open \
                transaction. Call server_sessions first to get the `key`, and copy that session's \
                `user` and `statement` into this call so the user can see what they are stopping — \
                the target is re-checked against the live server before anything happens, and the \
                kill is refused if the session has been recycled meanwhile. Requires the user's \
                explicit approval."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "key": { "type": "string", "description": "The session key from server_sessions." },
                    "mode": {
                        "type": "string",
                        "enum": ["cancel", "terminate"],
                        "description": "\"cancel\" stops the statement; \"terminate\" drops the session (rolls back its transaction). Default \"cancel\".",
                    },
                    "user": { "type": "string", "description": "The session's user, copied from server_sessions; verified before the kill." },
                    "statement": { "type": "string", "description": "The session's running statement, copied from server_sessions, so the approval shows what is being stopped." },
                },
                "required": ["key"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "propose_write".into(),
            description: "Execute a SINGLE data-modifying statement: INSERT, UPDATE, or DELETE. \
                EVERY call requires explicit per-statement approval: the user sees the exact SQL \
                and must Allow it before it runs; assume it may be denied. UPDATE and DELETE MUST \
                include a WHERE clause. DDL (DROP/TRUNCATE/ALTER/CREATE) and any multi-statement \
                input are rejected; tell the user to run those by hand. Use this only when the \
                user has asked you to change data; otherwise read with run_select."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "sql": { "type": "string", "description": "A single INSERT/UPDATE/DELETE statement (UPDATE/DELETE need a WHERE)." },
                },
                "required": ["sql"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "propose_changeset".into(),
            description: "Execute SEVERAL data-modifying statements as ONE approved unit, in \
                order. On an engine with multi-statement transactions they commit together, or if \
                any fails the whole set is rolled back (nothing changes); ClickHouse has no such \
                transaction, so there a failure leaves the statements before it applied. Use this \
                for a related multi-step change — e.g. insert a parent row then \
                its children, or update several rows in lockstep — where a half-applied result \
                would be wrong. EVERY call requires explicit approval: the user sees the full list \
                of statements and must Allow it before anything runs; assume it may be denied. Each \
                statement must be a single INSERT/UPDATE/DELETE (UPDATE/DELETE need a WHERE); DDL \
                and chained statements are rejected — tell the user to run those by hand. For a \
                single change use propose_write instead."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "statements": {
                        "type": "array",
                        "description": "The INSERT/UPDATE/DELETE statements to run in order, as one unit. Each is a single statement (UPDATE/DELETE need a WHERE).",
                        "items": { "type": "string" },
                        "minItems": 1,
                    },
                    "description": { "type": "string", "description": "One line on what this changeset does, shown to the user with the approval prompt." },
                },
                "required": ["statements"],
                "additionalProperties": false,
            }),
        },
    ];
    gate_catalog(all, policy)
}
/// The stable grounding instruction, tailored to the access tier. Shared
/// with the ACP path, which folds it into the agent's first prompt (ACP
/// `session/prompt` has no system role). The tier line keeps the model's
/// expectations in step with the catalog it actually receives, but the *catalog*
/// is the real gate; the prompt is just courtesy.
pub(crate) fn system_prompt(ctx: &AiContext, policy: &AiPolicy) -> String {
    let tools_line = match policy.tier {
        AiTier::Off => {
            "You have NO database tools available; answer from the schema overview and the \
             conversation alone, and tell the user you cannot read the live database."
        }
        AiTier::Schema => {
            "You have schema-only tools: list_schema, describe_table, relationship_map, and \
             object_ddl, plus search_query_history, list_saved_queries and read_saved_query (what \
             this user has already written against this database). You can inspect structure \
             (tables, columns, types, keys, definitions) but you CANNOT read row data; there is no \
             query tool, so do not promise to run one."
        }
        AiTier::Read => {
            "You have read-only tools: list_schema, describe_table, relationship_map (the \
             foreign-key graph), object_ddl (an object's real definition), run_select (capped \
             SELECTs), search_data (find a term across a table's columns), explain (optionally \
             with actuals), health_report and server_sessions (what is wrong / what is running \
             now), export_result (write a result to a file for the user), open_query (open a SQL \
             query in a new editor tab in the user's workspace; a read-only SELECT runs \
             automatically), and generate_report (you author an HTML report from data you've read, \
             with optional interactive Chart.js charts; it appears as a card in the chat the user \
             can open; use it when the user asks for a report), and save_knowledge (draft this \
             connection's knowledge file for the user to review). Use them to ground every answer in \
             the live database rather than guessing: discover objects with list_schema, inspect \
             structure with describe_table, and read data with run_select. Use open_query to hand \
             the user a query to explore in the grid. Prefer small, targeted queries with explicit \
             columns and LIMIT. You also have search_query_history, list_saved_queries and \
             read_saved_query: what this user has already written against this database."
        }
        AiTier::Write => {
            "You have the read tools (list_schema, describe_table, relationship_map, object_ddl, \
             run_select, search_data, explain, health_report, server_sessions, diff_schema, \
             diff_data, suggest_index, export_result, open_query, generate_report, \
             save_knowledge, search_query_history, list_saved_queries, read_saved_query) AND \
             gated write \
             tools: propose_write for a SINGLE INSERT/UPDATE/DELETE, propose_changeset for several \
             as one unit, create_index, and kill_session. Every one requires the user's explicit \
             Allow on the exact operation; assume it may be denied, and never batch or chain \
             statements inside one propose_write. UPDATE/DELETE must have a WHERE clause; \
             destructive DDL (DROP/TRUNCATE/ALTER) is not available; tell the user to run those by \
             hand. Only write when the user has asked you to change data; read first to get it \
             right, and verify after."
        }
    };
    let mut s = finish_system_prompt(
        format!(
            "You are RED's database agent, embedded in a native SQL explorer. You help the user \
             explore and understand the database they are connected to.\n\n\
             {tools_line}\n\n\
             Before any query that joins more than one table, call relationship_map; do not infer \
             join keys from column names. Before explaining a constraint failure or what a view \
             actually does, call object_ddl.\n\n\
             Before writing a non-trivial query, check list_saved_queries: a saved query is this \
             user's own blessed definition of a metric, and matching it matters more than writing \
             something cleverer. Use search_query_history to see how they actually write against \
             these tables - join paths, date columns, status values - instead of inferring from \
             column names. Where the two disagree with what the user has written down about this \
             database above, the written notes win on what something MEANS and the history wins on \
             how it is WRITTEN here.\n\n\
             Each tool result is labelled `[source N]`. When you state a figure or a fact you \
             read from a tool, append that marker to the claim - \"revenue was $4.2M [3]\". One \
             marker per claim, never the same source twice in a sentence, and never a marker on \
             something you reasoned out rather than read: a citation says where a number came \
             from, not that it is right.\n\n\
             When you write SQL for the user, put it in a fenced ```sql block so they can run it. \
             Be concise: lead with the answer, then the supporting query or detail.\n",
        ),
        ctx,
        "This connection is READ-ONLY: do not propose INSERT/UPDATE/DELETE/DDL.",
    );
    if !ctx.schema_summary.is_empty() {
        s.push_str("\n\nSchema overview (use describe_table for full detail):\n");
        s.push_str(&ctx.schema_summary);
    }
    s
}
/// Fold the volatile, per-turn context (editor SQL, last error, selection) into
/// the user's message so the stable system prompt stays prompt-cacheable. Shared
/// with the ACP path for the same per-turn grounding.
pub(crate) fn user_turn(message: &str, ctx: &AiContext) -> String {
    let mut s = String::new();
    // Cursors legitimately outlive the turn that opened them ("keep going" is a
    // real follow-up), but never *silently*: told nothing, a model either forgets
    // it can continue or invents a handle. Volatile per-turn state, so it rides
    // here rather than in the cached system prompt.
    if let Some(line) = ctx.open_cursors.as_deref().filter(|l| !l.trim().is_empty()) {
        s.push_str(line);
        s.push_str("\n\n");
    }
    // A reopened conversation seeds the prior exchange once, so the model
    // picks up where the saved chat left off even though its session is fresh.
    if let Some(prior) = ctx
        .prior_transcript
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        s.push_str("Earlier in this conversation (for context):\n");
        s.push_str(prior.trim());
        s.push_str("\n\n---\n\n");
    }
    if let Some(tab) = ctx.current_tab.as_deref().filter(|s| !s.trim().is_empty()) {
        s.push_str("The user is currently viewing tab ");
        s.push_str(tab.trim());
        s.push_str(
            ". When they say \"this\"/\"the current tab/query/result\", they mean this.\n\n",
        );
    }
    if let Some(sql) = ctx.editor_sql.as_deref().filter(|s| !s.trim().is_empty()) {
        s.push_str("Current editor SQL:\n```sql\n");
        s.push_str(sql.trim());
        s.push_str("\n```\n\n");
    }
    if let Some(err) = ctx.last_error.as_deref().filter(|s| !s.trim().is_empty()) {
        s.push_str("Last error shown:\n");
        s.push_str(err.trim());
        s.push_str("\n\n");
    }
    if let Some(sel) = ctx.selection.as_deref().filter(|s| !s.trim().is_empty()) {
        s.push_str("Selected rows:\n");
        s.push_str(sel.trim());
        s.push_str("\n\n");
    }
    s.push_str(message);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::protocol::AiContext;

    #[test]
    fn catalog_filters_by_tier() {
        use red_core::{AiPolicy, AiTier};
        let names = |tier| -> Vec<String> {
            tool_catalog(&AiPolicy {
                tier,
                ..AiPolicy::default()
            })
            .into_iter()
            .map(|t| t.name)
            .collect()
        };
        assert!(names(AiTier::Off).is_empty());
        // Schema tier: structure only. `object_ddl` and `relationship_map` belong
        // here because a definition and a declared constraint are catalog facts,
        // not rows.
        assert_eq!(
            names(AiTier::Schema),
            [
                "list_schema",
                "describe_table",
                "object_ddl",
                "relationship_map",
                "search_query_history",
                "list_saved_queries",
                "read_saved_query"
            ]
        );
        assert_eq!(
            names(AiTier::Read),
            [
                "list_schema",
                "describe_table",
                "object_ddl",
                "relationship_map",
                "profile_table",
                "run_select",
                "fetch_more",
                "search_data",
                "explain",
                "health_report",
                "server_sessions",
                "diff_schema",
                "diff_data",
                "suggest_index",
                "export_result",
                "generate_report",
                "save_knowledge",
                "search_query_history",
                "list_saved_queries",
                "read_saved_query",
                "open_query",
                "save_query",
                "spawn_subagent"
            ]
        );
    }

    #[test]
    fn catalog_has_the_readonly_tools_at_read_tier() {
        let catalog = tool_catalog(&AiPolicy::default());
        let names: Vec<&str> = catalog.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "list_schema",
                "describe_table",
                "object_ddl",
                "relationship_map",
                "profile_table",
                "run_select",
                "fetch_more",
                "search_data",
                "explain",
                "health_report",
                "server_sessions",
                "diff_schema",
                "diff_data",
                "suggest_index",
                "export_result",
                "generate_report",
                "save_knowledge",
                "search_query_history",
                "list_saved_queries",
                "read_saved_query",
                "open_query",
                "save_query",
                "spawn_subagent"
            ]
        );
    }

    #[test]
    fn catalog_offers_write_tool_only_at_write_tier_and_not_read_only() {
        let names = |p: AiPolicy| {
            tool_catalog(&p)
                .into_iter()
                .map(|t| t.name)
                .collect::<Vec<_>>()
        };
        // Read tier never offers the write tool.
        assert!(
            names(AiPolicy::default())
                .iter()
                .all(|n| n != "propose_write")
        );
        // Write tier offers it…
        let write = AiPolicy {
            tier: AiTier::Write,
            ..AiPolicy::default()
        };
        assert!(names(write).iter().any(|n| n == "propose_write"));
        // …but withholds it on a read-only connection.
        let write_ro = AiPolicy {
            tier: AiTier::Write,
            read_only: true,
            ..AiPolicy::default()
        };
        assert!(names(write_ro).iter().all(|n| n != "propose_write"));
    }

    /// The ordering *is* the feature, so it is asserted rather than assumed: the
    /// knowledge file overrides inference, so the model must read it before the
    /// schema it overrides, and both must stay in the (prompt-cached) system
    /// prompt rather than drifting into the per-turn message.
    #[test]
    fn system_prompt_puts_knowledge_after_the_tools_line_and_before_the_schema() {
        let ctx = AiContext {
            schema_summary: "orders(id int, status text)".into(),
            knowledge: Some("`void` orders never happened; exclude them.".into()),
            ..Default::default()
        };
        let prompt = system_prompt(&ctx, &AiPolicy::default());
        let tools = prompt.find("You have read-only tools").expect("tools line");
        let knowledge = prompt
            .find("`void` orders never happened")
            .expect("knowledge");
        let schema = prompt.find("Schema overview").expect("schema overview");
        assert!(tools < knowledge, "knowledge must follow the tools line");
        assert!(
            knowledge < schema,
            "knowledge must precede the schema overview"
        );
        // And it never leaks into the volatile per-turn message, which sits after
        // the last cache breakpoint and would be re-read on every single turn.
        assert!(!user_turn("how many orders?", &ctx).contains("`void` orders"));
    }

    #[test]
    fn user_turn_folds_prior_transcript_once() {
        let ctx = AiContext {
            prior_transcript: Some("You: hi\n\nAssistant: hello".into()),
            ..Default::default()
        };
        let turn = user_turn("and now?", &ctx);
        assert!(turn.contains("Earlier in this conversation"));
        assert!(turn.contains("Assistant: hello"));
        // The actual message still comes last.
        assert!(turn.trim_end().ends_with("and now?"));
        // No prior transcript → no preamble.
        let plain = user_turn("hi", &AiContext::default());
        assert!(!plain.contains("Earlier in this conversation"));
        assert_eq!(plain, "hi");
    }
}
