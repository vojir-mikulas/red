//! How dangerous a statement is, and why.
//!
//! The confirm gate's job is not to ask "are you sure" as often as possible: a
//! prompt that fires on ordinary work trains the reflex that dismisses it, and
//! pressures the user into switching the rail off entirely. So this module grades
//! rather than flags, and reports *reasons* rather than a verdict, so a caller can
//! tell the user what it noticed instead of showing an undifferentiated red button.
//!
//! Two rules shape every classification here.
//!
//! **Reason over stripped SQL.** Every scan runs on a [`super::strip_noise`] copy,
//! so a keyword inside a string literal, a quoted identifier, or a comment is
//! invisible. Without that, `UPDATE t SET note = 'see where'` reads as filtered
//! when it rewrites the whole table.
//!
//! **Escalate when unsure.** A shape this module cannot parse confidently is
//! reported at the higher level, never the lower one. Over-warning costs a
//! keystroke; under-warning costs a table. The same asymmetry is why detections
//! that would need a real parser (an always-true predicate hidden inside a longer
//! boolean expression) are deliberately left narrow here, and answered properly by
//! counting the affected rows against the engine instead.

use super::{Dialect, first_keyword, has_word, split_statements, strip_noise};

/// Whole-word tokens that make a statement a write even though it leads with a read
/// keyword: the data-modifying CTE verbs (Postgres executes these), `INTO` for
/// `SELECT … INTO` / `INTO OUTFILE`, and the sequence advancers. All are reserved
/// words, so a column legitimately named one of them would have to be quoted, and
/// [`strip_noise`] blanks it before any scan reaches here.
pub const WRITE_TOKENS: &[&str] = &[
    "insert", "update", "delete", "merge", "into", "nextval", "setval",
];

/// Server-side functions callable from inside a `SELECT` that write or read files,
/// manipulate large objects, execute remote SQL, or emit WAL: write channels that
/// read as reads (`SELECT lo_import('/etc/passwd')`, `SELECT dblink_exec(…)`).
///
/// A denylist, so it is a mitigation and not a guarantee; the complete answer is an
/// engine-level read-only connection. The names are underscore-qualified and could
/// not plausibly be bare column names, so blocking them does not trip real queries.
pub const DANGEROUS_FNS: &[&str] = &[
    // Postgres: file read/write, large objects, remote exec, WAL, admin file ops.
    "lo_import",
    "lo_export",
    "pg_read_file",
    "pg_read_binary_file",
    "pg_ls_dir",
    "pg_stat_file",
    "pg_logical_emit_message",
    // `dblink` / `dblink_send_query` run arbitrary SQL on a remote (often the same
    // loopback) server from inside a SELECT. `dblink_exec` is the obvious one; the
    // bare and async forms are the same hole under a different name.
    "dblink",
    "dblink_exec",
    "dblink_open",
    "dblink_send_query",
    "pg_file_write",
    "pg_file_unlink",
    "pg_file_rename",
    // MySQL: file read and UDF command execution.
    "load_file",
    "sys_exec",
    "sys_eval",
];

/// How dangerous a statement, or a whole batch, is. Ordered so that a batch's level
/// is the `max` of its statements': a leading `SELECT` must never mask a trailing
/// `DROP`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RiskLevel {
    /// Row-returning and side-effect free.
    Safe,
    /// Changes only what it names: `INSERT`, `CREATE`, a filtered `UPDATE`/`DELETE`.
    Write,
    /// Reaches further than it names, or does something this module cannot see into.
    Risky,
    /// Destroys a whole object.
    Critical,
}

/// The mutating verb of a statement whose row filter is missing or ineffective.
/// Only `UPDATE` and `DELETE` can carry these risks, so no other verb is
/// representable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutateVerb {
    Update,
    Delete,
}

/// What a `DROP` targets. The first three destroy user data outright; the rest
/// destroy a derived or structural object that can be rebuilt from a definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropKind {
    Table,
    Database,
    Schema,
    View,
    Index,
    /// A `DROP` of something else (a function, a trigger, a type, an extension).
    Other,
}

/// One concrete reason a statement was graded above [`RiskLevel::Safe`]. Callers
/// render these to explain the grade; the data is deliberately structural rather
/// than pre-worded, so the phrasing stays with the layer that displays it.
///
/// A `table` / `name` of `None` means the object could not be extracted, not that
/// there isn't one; see `target_object`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Risk {
    /// An `UPDATE`/`DELETE` with no `WHERE` at all: it rewrites or removes every
    /// row in the table.
    WholeTable {
        verb: MutateVerb,
        table: Option<String>,
    },
    /// An `UPDATE`/`DELETE` whose `WHERE` is one of the textbook always-true forms
    /// (`WHERE 1=1`, `WHERE true`), so the filter does not actually filter.
    AlwaysTrue {
        verb: MutateVerb,
        table: Option<String>,
    },
    /// A `DROP` of a whole object.
    Drops {
        object: DropKind,
        name: Option<String>,
    },
    /// `TRUNCATE`: empties a table, and on most engines is neither transactional
    /// nor undoable.
    Truncates { table: Option<String> },
    /// An `ALTER` that drops something (a column, a constraint), destroying the
    /// data or guarantee it held.
    DropsColumn { table: Option<String> },
    /// `GRANT` / `REVOKE` / `CREATE USER`: changes who can do what, which no
    /// row-level guard would catch.
    PrivilegeChange,
    /// `CALL` / `DO` / `EXEC`: runs procedural code whose effects this module
    /// cannot inspect, so it is graded on that opacity rather than on its body.
    OpaqueExecution,
    /// `MERGE`: reads as an upsert but can delete rows.
    Merge { table: Option<String> },
    /// A CTE that writes. Postgres executes `WITH x AS (DELETE … RETURNING …)
    /// SELECT * FROM x`, so a statement leading with `WITH` is not automatically a
    /// read; grading it by its leading keyword alone is exactly the hole this
    /// catches.
    DataModifyingCte,
    /// A `Risky`-or-worse statement sitting inside a multi-statement batch, where
    /// it is easy to miss. `index` is 0-based.
    HiddenInBatch { index: usize, total: usize },
}

/// The graded verdict for a statement or batch: the level a caller gates on, the
/// reasons it can show, and the object the statement targets when there is exactly
/// one runnable statement and its target could be extracted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assessment {
    pub level: RiskLevel,
    pub risks: Vec<Risk>,
    /// The target as *written* (`orders`, `public.orders`, `"my table"`), so it can
    /// be put back into a generated query as well as displayed. `None` for a batch,
    /// for a statement with no single target, and for any shape the extractor
    /// declines to guess at.
    pub table: Option<String>,
}

impl Assessment {
    /// The object name a typed confirmation should make the user write out,
    /// unqualified and unquoted (`orders`, not `` `public`.`orders` ``).
    ///
    /// Consults the risks before [`Self::table`], so that a `DROP` buried in a batch
    /// still yields a name: that is precisely the case where a typed confirmation
    /// earns its friction, and the batch itself has no single target.
    ///
    /// `None` when nothing here named an object at all (a `GRANT`, say). Callers
    /// must handle that by falling back to an ordinary confirmation, never by
    /// skipping one: not knowing what to make someone type is not a reason to stop
    /// asking.
    pub fn confirm_target(&self) -> Option<&str> {
        self.risks
            .iter()
            .find_map(|risk| match risk {
                Risk::Drops { name, .. } => name.as_deref(),
                Risk::Truncates { table } => table.as_deref(),
                _ => None,
            })
            .or(self.table.as_deref())
            .map(bare_name)
    }
}

/// The last segment of a possibly-qualified name, stripped of whichever quoting
/// style wrapped it: what a person would call the table.
fn bare_name(reference: &str) -> &str {
    let last = reference.rsplit('.').next().unwrap_or(reference);
    last.trim_matches(['"', '`', '[', ']'].as_slice())
}

/// Grade `sql`, which may be a whole `;`-separated script, by its most dangerous
/// statement. The `dialect` must be the engine that will run it: statement
/// boundaries and comment forms differ per engine, and grading against the wrong
/// lexing is exactly the drift this module exists to prevent.
///
/// Blank and comment-only input grades [`RiskLevel::Safe`] with no risks, matching
/// the "nothing runnable" reading the rest of the module uses.
pub fn assess(sql: &str, dialect: Dialect) -> Assessment {
    // Both the "is there anything runnable here" filter and the grading itself read
    // the stripped copy. Filtering the *raw* text instead would drop a MySQL
    // `/*!50000 DROP TABLE t */` as comment-only and grade the batch Safe, while the
    // server runs the body — [`strip_noise`] is what makes it visible.
    let statements: Vec<(&str, String)> = split_statements(sql, dialect)
        .into_iter()
        .map(|stmt| {
            let stripped = strip_noise(stmt, dialect);
            (stmt, stripped)
        })
        .filter(|(_, stripped)| !first_keyword(stripped).is_empty())
        .collect();
    let total = statements.len();
    let mut level = RiskLevel::Safe;
    let mut risks = Vec::new();
    for (index, (stmt, stripped)) in statements.iter().enumerate() {
        let (stmt_level, mut stmt_risks) = assess_one(stmt, stripped, dialect);
        level = level.max(stmt_level);
        // Flag *where* the danger is only when it could be overlooked. In a single
        // statement the user is already looking at it.
        if total > 1 && stmt_level >= RiskLevel::Risky {
            risks.push(Risk::HiddenInBatch { index, total });
        }
        risks.append(&mut stmt_risks);
    }
    // A batch has no single target, and callers that need one (a typed confirm, a
    // row-count preflight) must not be handed an arbitrary member of it.
    let table = match statements.as_slice() {
        [(only, _)] => target_object(only),
        _ => None,
    };
    Assessment {
        level,
        risks,
        table,
    }
}

/// Grade one statement, given its [`strip_noise`] copy. Split out so [`assess`]
/// owns only the batch reasoning.
fn assess_one(stmt: &str, stripped: &str, dialect: Dialect) -> (RiskLevel, Vec<Risk>) {
    let lower = stripped
        .trim()
        .trim_end_matches(';')
        .trim()
        .to_ascii_lowercase();
    let table = || target_object(stmt);

    match first_keyword(&lower) {
        // Deliberately *not* including `SHOW` / `DESCRIBE`, which are reads. Callers
        // route on `Safe` to mean "open this in the result grid", which wraps the
        // statement as `SELECT * FROM (<sql>)` for paging, and neither of those is
        // legal as a subquery. Grading them `Write` costs nothing (nothing confirms
        // at `Write` by default) and keeps the grading honest about being the
        // conservative direction.
        "select" | "values" | "pragma" => (RiskLevel::Safe, Vec::new()),
        // `EXPLAIN` plans without running — except `EXPLAIN ANALYZE`, which on
        // Postgres and MySQL 8.0.18+ *runs* the statement it is given. Grading the
        // whole form Safe hands the confirm gate and any read-only AI tier a write
        // channel, so the inner statement is graded instead.
        "explain" => explain_risk(stmt, stripped, &lower, dialect),
        // A CTE is a read *unless* its body writes, which Postgres permits.
        "with" => {
            if ["insert", "update", "delete", "merge"]
                .iter()
                .any(|verb| has_word(&lower, verb))
            {
                (RiskLevel::Risky, vec![Risk::DataModifyingCte])
            } else {
                (RiskLevel::Safe, Vec::new())
            }
        }
        // Keyed row writes: they touch what they name and nothing more. `REPLACE`
        // (MySQL) deletes-then-inserts, but only on the primary key, so it is the
        // same shape as an upsert.
        "insert" | "replace" => (RiskLevel::Write, Vec::new()),
        verb @ ("update" | "delete") => {
            let verb = if verb == "update" {
                MutateVerb::Update
            } else {
                MutateVerb::Delete
            };
            if !has_word(&lower, "where") {
                (
                    RiskLevel::Risky,
                    vec![Risk::WholeTable {
                        verb,
                        table: table(),
                    }],
                )
            } else if always_true_where(&lower) {
                (
                    RiskLevel::Risky,
                    vec![Risk::AlwaysTrue {
                        verb,
                        table: table(),
                    }],
                )
            } else {
                (RiskLevel::Write, Vec::new())
            }
        }
        "merge" => (RiskLevel::Risky, vec![Risk::Merge { table: table() }]),
        "truncate" => (
            RiskLevel::Critical,
            vec![Risk::Truncates { table: table() }],
        ),
        "drop" => {
            let object = match word_at(&lower, 1) {
                "table" => DropKind::Table,
                "database" => DropKind::Database,
                "schema" => DropKind::Schema,
                "view" => DropKind::View,
                "index" => DropKind::Index,
                _ => DropKind::Other,
            };
            // Dropping a view or an index destroys a definition, which is in source
            // control or trivially rebuilt; dropping a table or a namespace destroys
            // rows, which are not.
            let level = match object {
                DropKind::Table | DropKind::Database | DropKind::Schema => RiskLevel::Critical,
                DropKind::View | DropKind::Index | DropKind::Other => RiskLevel::Risky,
            };
            (
                level,
                vec![Risk::Drops {
                    object,
                    name: table(),
                }],
            )
        }
        // `ALTER … ADD` is additive; `ALTER … DROP COLUMN` / `DROP CONSTRAINT`
        // destroys data or a guarantee.
        "alter" => {
            if has_word(&lower, "drop") {
                (RiskLevel::Risky, vec![Risk::DropsColumn { table: table() }])
            } else {
                (RiskLevel::Write, Vec::new())
            }
        }
        "create" => match word_at(&lower, 1) {
            "user" | "role" => (RiskLevel::Risky, vec![Risk::PrivilegeChange]),
            _ => (RiskLevel::Write, Vec::new()),
        },
        "grant" | "revoke" => (RiskLevel::Risky, vec![Risk::PrivilegeChange]),
        "call" | "do" | "exec" | "execute" => (RiskLevel::Risky, vec![Risk::OpaqueExecution]),
        // Session and transaction control (`SET`, `BEGIN`, `USE`, `VACUUM`, …) and
        // anything unrecognised. `Write` is the honest floor: it is not a read, and
        // nothing here has established it is worse.
        _ => (RiskLevel::Write, Vec::new()),
    }
}

/// Grade an `EXPLAIN`. A plain `EXPLAIN` (and SQLite's `EXPLAIN QUERY PLAN`)
/// plans without running, so it is a read; `EXPLAIN ANALYZE` runs the statement
/// it is handed on Postgres and MySQL 8.0.18+, so the honest grade is whatever
/// that inner statement's own grade is.
///
/// `stripped` is the byte-offset-preserving [`strip_noise`] copy of `stmt`, which
/// is what lets the inner statement be sliced out of both at a single offset and
/// graded as itself — literals and quoting intact for `target_object`.
fn explain_risk(
    stmt: &str,
    stripped: &str,
    lower: &str,
    dialect: Dialect,
) -> (RiskLevel, Vec<Risk>) {
    if !has_word(lower, "analyze") {
        return (RiskLevel::Safe, Vec::new());
    }
    match inner_statement_at(stripped) {
        Some(at) => assess_one(&stmt[at..], &stripped[at..], dialect),
        // `ANALYZE` is present but nothing recognisable follows it. Escalate when
        // unsure: `Write` is the same floor the unrecognised arm below uses.
        None => (RiskLevel::Write, Vec::new()),
    }
}

/// The byte offset in `stripped` of the first word that begins a statement, used
/// to find what an `EXPLAIN` wraps. `explain` itself is absent from the list so
/// the scan always advances past it.
fn inner_statement_at(stripped: &str) -> Option<usize> {
    const VERBS: [&str; 13] = [
        "select", "insert", "update", "delete", "merge", "replace", "with", "create", "drop",
        "alter", "truncate", "call", "values",
    ];
    let b = stripped.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if !super::is_word_byte(b[i]) {
            i += 1;
            continue;
        }
        let end = super::word_end(b, i);
        if VERBS
            .iter()
            .any(|v| stripped[i..end].eq_ignore_ascii_case(v))
        {
            return Some(i);
        }
        i = end;
    }
    None
}

/// The `n`th whitespace-separated word of an already-lower-cased, noise-stripped
/// statement (`0` is the verb). Empty when there is no such word.
fn word_at(lower: &str, n: usize) -> &str {
    lower.split_whitespace().nth(n).unwrap_or("")
}

/// Whether the `WHERE` in an already-lower-cased, noise-stripped statement is one
/// of the textbook always-true forms.
///
/// Deliberately narrow: it matches only when the *entire* predicate is the tautology,
/// so `WHERE 1=1 AND status = 'x'` (a real filter) and `WHERE 1=1 OR x = 2` (also
/// always true, but indistinguishable from a real filter without evaluating it) both
/// read as filtered. Catching the general case needs the engine's own row count, not
/// a cleverer scanner, which is why this stays a cheap check for the copy-paste
/// template rather than a predicate evaluator.
fn always_true_where(lower: &str) -> bool {
    let tokens: Vec<&str> = lower.split_whitespace().collect();
    let Some(at) = tokens.iter().position(|t| *t == "where") else {
        return false;
    };
    // Rejoin without spaces so `1 = 1` and `1=1` are the same predicate.
    let predicate = tokens[at + 1..].concat();
    matches!(predicate.as_str(), "1=1" | "true" | "1<>0" | "1!=0")
}

/// The object a statement targets, as written, or `None` when the shape is one this
/// extractor will not guess at.
///
/// Narrow on purpose. `None` costs a caller a nicety (an unnamed confirm, a skipped
/// row-count preflight); a *wrong* name would put the wrong table in front of the
/// user at exactly the moment they are deciding whether to destroy it. So the
/// multi-table forms (`DELETE a FROM a JOIN b`, `UPDATE a, b SET …`) return `None`
/// rather than picking one.
pub(super) fn target_object(stmt: &str) -> Option<String> {
    let tokens = lex_refs(stmt);
    let word = |n: usize| match tokens.get(n) {
        Some(Ref::Ident(w)) => Some(w.to_ascii_lowercase()),
        _ => None,
    };
    let mut at = match word(0)?.as_str() {
        "update" | "truncate" => 1,
        "insert" | "delete" | "merge" => {
            // The object follows a preposition. Anything else is a multi-table form.
            if !matches!(word(1).as_deref(), Some("into" | "from")) {
                return None;
            }
            2
        }
        "drop" | "alter" => 2,
        _ => return None,
    };
    // Modifiers that can sit between the verb and the name.
    while matches!(
        word(at).as_deref(),
        Some("table" | "if" | "exists" | "only" | "concurrently")
    ) {
        at += 1;
    }
    read_ref(tokens.get(at..)?)
}

/// A token in the walk from a statement's verb to the object it names.
enum Ref<'a> {
    /// An identifier, kept exactly as written (quotes included) so a caller can put
    /// it back into a generated query.
    Ident(&'a str),
    /// The `.` joining the parts of a qualified name.
    Dot,
    /// Anything else, including a string literal: never part of an object name, and
    /// present only so the walk above can stop on it.
    Other,
}

/// Tokenize `stmt` just far enough to find the object its verb names. Whitespace
/// and comments are skipped; the three identifier quoting styles plus SQL Server's
/// `[brackets]` are recognised and kept whole.
fn lex_refs(stmt: &str) -> Vec<Ref<'_>> {
    let b = stmt.as_bytes();
    let n = b.len();
    let mut out = Vec::new();
    let mut i = 0;
    while i < n {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if c == b'-' && i + 1 < n && b[i + 1] == b'-' {
            while i < n && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'/' && i + 1 < n && b[i + 1] == b'*' {
            i += 2;
            while i + 1 < n && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(n);
            continue;
        }
        // A string literal is data, never a name; consume it as an opaque stop.
        if c == b'\'' {
            i += 1;
            while i < n && b[i] != b'\'' {
                i += 1;
            }
            i = (i + 1).min(n);
            out.push(Ref::Other);
            continue;
        }
        if matches!(c, b'"' | b'`' | b'[') {
            let close = if c == b'[' { b']' } else { c };
            let start = i;
            i += 1;
            while i < n && b[i] != close {
                i += 1;
            }
            i = (i + 1).min(n);
            out.push(Ref::Ident(&stmt[start..i]));
            continue;
        }
        if c.is_ascii_alphabetic() || c == b'_' {
            let start = i;
            while i < n && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            out.push(Ref::Ident(&stmt[start..i]));
            continue;
        }
        out.push(if c == b'.' { Ref::Dot } else { Ref::Other });
        i += 1;
    }
    out
}

/// Read a possibly-qualified name (`t`, `s.t`, `db.s.t`) from the front of `tokens`,
/// rebuilt as written. `None` when the tokens do not start with a name.
fn read_ref(tokens: &[Ref<'_>]) -> Option<String> {
    let mut out = String::new();
    let mut at = 0;
    loop {
        match tokens.get(at) {
            Some(Ref::Ident(w)) => out.push_str(w),
            _ => return None,
        }
        at += 1;
        if matches!(tokens.get(at), Some(Ref::Dot)) {
            out.push('.');
            at += 1;
        } else {
            return Some(out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Assessment, Dialect, DropKind, MutateVerb, Risk, RiskLevel, target_object};

    /// The gradings here probe shapes, not dialect lexing, so they all run under
    /// [`Dialect::Generic`]; the per-dialect lexing has its own tests in `sql`.
    fn assess(sql: &str) -> Assessment {
        super::assess(sql, Dialect::Generic)
    }

    /// MySQL executes the body of `/*!…*/`. Grading it as a comment made the whole
    /// statement read as blank, so `red exec` ran the `DROP` with no prompt at all.
    #[test]
    fn mysql_executable_comment_is_graded_by_its_body() {
        let hidden = super::assess("/*!50000 DROP TABLE users */", Dialect::MySql);
        assert_eq!(hidden.level, RiskLevel::Critical, "got {hidden:#?}");
        assert_eq!(
            hidden.level,
            super::assess("DROP TABLE users", Dialect::MySql).level
        );
        // Everywhere else it really is a comment, so it stays "nothing runnable".
        assert_eq!(
            super::assess("/*!50000 DROP TABLE users */", Dialect::Postgres).level,
            RiskLevel::Safe
        );
    }

    /// `EXPLAIN ANALYZE <DML>` runs the DML on Postgres and MySQL 8.0.18+, so it
    /// has to be graded as the statement it wraps rather than as a plan.
    #[test]
    fn explain_analyze_is_graded_as_the_statement_it_runs() {
        assert_eq!(
            assess("EXPLAIN ANALYZE DELETE FROM orders").level,
            assess("DELETE FROM orders").level
        );
        assert_eq!(
            assess("EXPLAIN ANALYZE DELETE FROM orders").level,
            RiskLevel::Risky
        );
        assert_eq!(
            assess("EXPLAIN (ANALYZE, BUFFERS) DROP TABLE orders").confirm_target(),
            Some("orders")
        );
        // A plan is still a plan: neither of these executes anything.
        assert_eq!(
            assess("EXPLAIN SELECT * FROM orders").level,
            RiskLevel::Safe
        );
        assert_eq!(
            assess("EXPLAIN QUERY PLAN SELECT * FROM orders").level,
            RiskLevel::Safe
        );
        assert_eq!(
            assess("EXPLAIN ANALYZE SELECT * FROM orders").level,
            RiskLevel::Safe
        );
    }

    #[test]
    fn confirm_target_names_what_the_user_must_type() {
        // The bare name, not the qualified reference: `orders`, not `public.orders`.
        assert_eq!(
            assess("DROP TABLE public.orders").confirm_target(),
            Some("orders")
        );
        assert_eq!(
            assess("TRUNCATE `my table`").confirm_target(),
            Some("my table")
        );
        assert_eq!(
            assess(r#"DROP TABLE IF EXISTS "Weird Name""#).confirm_target(),
            Some("Weird Name")
        );
        // A batch has no single target, but the drop inside it still names one, which
        // is exactly the case a typed confirmation is for.
        assert_eq!(
            assess("SELECT 1; DROP TABLE users").confirm_target(),
            Some("users")
        );
        // Nothing critical, or nothing named: the caller falls back to a plain confirm.
        assert_eq!(assess("SELECT 1").confirm_target(), None);
        assert_eq!(assess("GRANT ALL ON t TO bob").confirm_target(), None);
    }

    fn level(sql: &str) -> RiskLevel {
        assess(sql).level
    }

    #[test]
    fn reads_are_safe() {
        for sql in [
            "SELECT * FROM users",
            "  select 1 ",
            "EXPLAIN SELECT * FROM t",
            "VALUES (1), (2)",
            "PRAGMA table_info(t)",
            "WITH x AS (SELECT 1) SELECT * FROM x",
            // A `;`-terminated read, and a batch of reads, are still reads.
            "SELECT 1;",
            "SELECT 1; SELECT 2",
        ] {
            assert_eq!(level(sql), RiskLevel::Safe, "{sql}");
        }
        // Nothing runnable grades as a read rather than as an unknown.
        assert_eq!(level(""), RiskLevel::Safe);
        assert_eq!(level("  -- just a note"), RiskLevel::Safe);
        assert_eq!(level(";;"), RiskLevel::Safe);
    }

    #[test]
    fn keyed_writes_do_not_escalate() {
        for sql in [
            "INSERT INTO t VALUES (1)",
            "REPLACE INTO t VALUES (1)",
            "CREATE TABLE t (id int)",
            "CREATE INDEX i ON t(a)",
            "ALTER TABLE t ADD COLUMN x int",
            // Reads, but graded `Write` because callers treat `Safe` as "wrappable
            // as a subquery" and these are not. See the note in `assess_one`.
            "SHOW TABLES",
            "DESCRIBE t",
            "UPDATE t SET a = 1 WHERE id = 42",
            "DELETE FROM t WHERE id = 42",
            "DELETE FROM t WHERE id IN (SELECT id FROM u)",
            // A filter that merely *contains* a tautology is still a filter.
            "DELETE FROM t WHERE 1=1 AND id = 42",
        ] {
            assert_eq!(level(sql), RiskLevel::Write, "{sql}");
        }
    }

    #[test]
    fn unfiltered_mutations_are_risky() {
        let a = assess("DELETE FROM orders");
        assert_eq!(a.level, RiskLevel::Risky);
        assert_eq!(
            a.risks,
            vec![Risk::WholeTable {
                verb: MutateVerb::Delete,
                table: Some("orders".into())
            }]
        );
        let a = assess("UPDATE public.users SET banned = true");
        assert_eq!(a.level, RiskLevel::Risky);
        assert_eq!(
            a.risks,
            vec![Risk::WholeTable {
                verb: MutateVerb::Update,
                table: Some("public.users".into())
            }]
        );
    }

    #[test]
    fn a_where_inside_a_literal_does_not_count_as_a_filter() {
        // The whole point of scanning stripped SQL: this rewrites every row.
        let a = assess("UPDATE t SET note = 'see where it goes'");
        assert_eq!(a.level, RiskLevel::Risky);
        assert!(matches!(a.risks[..], [Risk::WholeTable { .. }]));
        // Likewise a quoted column named `where`.
        assert_eq!(
            level(r#"DELETE FROM t WHERE "where" = 1"#),
            RiskLevel::Write
        );
    }

    #[test]
    fn always_true_predicates_are_risky() {
        for sql in [
            "DELETE FROM t WHERE 1=1",
            "delete from t where 1 = 1",
            "UPDATE t SET a = 1 WHERE true",
            "DELETE FROM t WHERE 1<>0",
        ] {
            assert_eq!(level(sql), RiskLevel::Risky, "{sql}");
            assert!(
                matches!(assess(sql).risks[..], [Risk::AlwaysTrue { .. }]),
                "{sql}"
            );
        }
    }

    #[test]
    fn drops_and_truncates_are_critical() {
        for sql in [
            "DROP TABLE users",
            "drop table if exists users",
            "DROP DATABASE prod",
            "DROP SCHEMA public",
            "TRUNCATE orders",
            "TRUNCATE TABLE orders",
        ] {
            assert_eq!(level(sql), RiskLevel::Critical, "{sql}");
        }
        // Definitions are rebuildable, so dropping them stops short of critical.
        assert_eq!(level("DROP INDEX i"), RiskLevel::Risky);
        assert_eq!(level("DROP VIEW v"), RiskLevel::Risky);
        assert_eq!(level("DROP FUNCTION f()"), RiskLevel::Risky);

        let a = assess("DROP TABLE IF EXISTS public.users");
        assert_eq!(
            a.risks,
            vec![Risk::Drops {
                object: DropKind::Table,
                name: Some("public.users".into())
            }]
        );
    }

    #[test]
    fn statements_the_old_gate_let_through_are_graded() {
        // Every one of these classified as a plain `Write` under the leading-keyword
        // gate, so none of them ever prompted.
        for sql in [
            "GRANT ALL ON t TO bob",
            "REVOKE SELECT ON t FROM bob",
            "CREATE USER bob PASSWORD 'x'",
            "CREATE ROLE admin",
            "MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN DELETE",
            "CALL cleanup()",
            "DO $$ BEGIN DELETE FROM t; END $$",
            "ALTER TABLE t DROP COLUMN secret",
            // A data-modifying CTE leads with a read keyword but writes.
            "WITH x AS (DELETE FROM t RETURNING *) SELECT * FROM x",
        ] {
            assert_eq!(level(sql), RiskLevel::Risky, "{sql}");
        }
    }

    #[test]
    fn a_batch_takes_its_worst_statement_and_says_where() {
        let a = assess("SELECT 1; INSERT INTO t VALUES (1); DROP TABLE users");
        assert_eq!(a.level, RiskLevel::Critical);
        // The batch flags the position of the dangerous statement, which is the part
        // a user scrolling past a long script would miss.
        assert!(
            a.risks
                .contains(&Risk::HiddenInBatch { index: 2, total: 3 })
        );
        // No single target: a preflight or a typed confirm must not key off a batch.
        assert_eq!(a.table, None);

        // A `;` inside a literal or a comment does not make a batch, so neither of
        // these carries a batch risk (nor a drop).
        for sql in [
            "SELECT 'a; DROP TABLE t' AS x",
            "SELECT 1 -- DROP TABLE t\n; SELECT 2",
        ] {
            let a = assess(sql);
            assert_eq!(a.level, RiskLevel::Safe, "{sql}");
            assert!(a.risks.is_empty(), "{sql}");
        }

        // A batch of ordinary writes is not escalated just for being a batch.
        let a = assess("INSERT INTO t VALUES (1); INSERT INTO t VALUES (2)");
        assert_eq!(a.level, RiskLevel::Write);
        assert!(a.risks.is_empty());
    }

    #[test]
    fn target_object_reads_the_shapes_it_knows() {
        for (sql, want) in [
            ("DELETE FROM orders", Some("orders")),
            ("delete from public.orders where a=1", Some("public.orders")),
            ("UPDATE ONLY t SET a = 1", Some("t")),
            ("TRUNCATE TABLE db.s.t", Some("db.s.t")),
            ("DROP TABLE IF EXISTS t", Some("t")),
            ("ALTER TABLE t DROP COLUMN c", Some("t")),
            ("INSERT INTO t VALUES (1)", Some("t")),
            // Quoting is preserved so the name can go back into a query.
            (r#"DELETE FROM "my table""#, Some(r#""my table""#)),
            ("DELETE FROM `my table`", Some("`my table`")),
            // A comment between the verb and the name is skipped.
            ("DELETE /* hi */ FROM t", Some("t")),
        ] {
            assert_eq!(target_object(sql).as_deref(), want, "{sql}");
        }

        // Shapes it declines to guess at: naming the wrong table here would be worse
        // than naming none.
        for sql in [
            "DELETE a FROM a JOIN b ON a.id = b.id",
            "SELECT * FROM t",
            "WITH x AS (DELETE FROM t RETURNING *) SELECT * FROM x",
            "",
        ] {
            assert_eq!(target_object(sql), None, "{sql}");
        }
    }
}
