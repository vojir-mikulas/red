//! The ClickHouse write half: what a table will let the grid change, and how.
//!
//! Split out of the driver proper because none of it is shaped like the rest of the
//! engine surface. Reading ClickHouse is uniform; writing to it is a pile of
//! engine-specific facts that have to be *derived* rather than assumed:
//!
//! * Only the `MergeTree` family mutates at all. `Memory`, `Log`, `File`, `Kafka`,
//!   views and `Distributed` accept some inserts but no `ALTER … UPDATE`.
//! * Key columns (sorting / primary / partition / sampling) are rejected by the
//!   engine on update, and `MATERIALIZED` / `ALIAS` columns cannot be written at all.
//! * There is no row identity. The sorting key is not unique and duplicates are
//!   normal, so a row is addressed by a *snapshot of its values* and every write
//!   preflights `count() == 1` before it runs.
//! * Cost is per part, not per row: a predicate that leads with the sorting-key
//!   columns prunes parts, one that doesn't rewrites the whole table. That is why
//!   [`ChTableFacts::edit_caps`] puts the sorting key at the front of the identity.
//!
//! See `docs/plans/todo/clickhouse-writes.md` for the reasoning behind each rule.

use red_core::{ColumnValue, EditMode, EditOp, RedError, Result, RowEditCaps, Value};

use super::ch_quote;

/// One column of a table as `system.columns` describes it, plus the key-membership
/// flags that decide whether it can be written.
pub(super) struct ChColumn {
    pub(super) name: String,
    /// The declared type, verbatim (`Nullable(LowCardinality(String))`, `Int32`, …).
    pub(super) type_name: String,
    /// `DEFAULT` / `MATERIALIZED` / `ALIAS` / empty. The last two are computed by the
    /// engine and cannot be written.
    pub(super) default_kind: String,
    pub(super) in_sorting_key: bool,
    pub(super) in_primary_key: bool,
    pub(super) in_partition_key: bool,
    pub(super) in_sampling_key: bool,
}

impl ChColumn {
    /// Whether the engine computes this column, so it can be neither inserted nor
    /// updated (`MATERIALIZED` and `ALIAS`; a plain `DEFAULT` is writable).
    fn computed(&self) -> bool {
        matches!(self.default_kind.as_str(), "MATERIALIZED" | "ALIAS")
    }

    /// Whether the engine refuses to update this column: any key membership. A key
    /// column defines where the row physically lives, so changing it is an insert
    /// plus a delete, not an update, and ClickHouse says so.
    fn in_any_key(&self) -> bool {
        self.in_sorting_key || self.in_primary_key || self.in_partition_key || self.in_sampling_key
    }
}

/// What one ClickHouse table is, for write purposes: its engine and its columns.
pub(super) struct ChTableFacts {
    /// `system.tables.engine`, e.g. `MergeTree`, `ReplicatedReplacingMergeTree`,
    /// `Memory`, `View`, `Distributed`.
    pub(super) engine: String,
    pub(super) columns: Vec<ChColumn>,
}

impl ChTableFacts {
    /// Derive what the grid may do with this table's rows.
    ///
    /// The identity leads with the sorting-key columns and only then falls back to
    /// the other comparable ones: order is irrelevant to *what* the conjunction
    /// matches, but decisive for what it *costs*, since a leading sorting-key
    /// predicate is what lets ClickHouse skip parts instead of rewriting the table.
    pub(super) fn edit_caps(&self) -> RowEditCaps {
        if let Some(reason) = self.mutation_blocker() {
            return RowEditCaps {
                note: Some(reason),
                no_insert: self.computed_columns(),
                ..RowEditCaps::default()
            };
        }
        // Sorting-key columns first (part pruning), then everything else that can be
        // compared in a WHERE at all.
        let (mut identity, rest): (Vec<&ChColumn>, Vec<&ChColumn>) = self
            .columns
            .iter()
            .filter(|c| !c.computed() && is_comparable(&c.type_name))
            .partition(|c| c.in_sorting_key);
        identity.extend(rest);
        let identity: Vec<String> = identity.into_iter().map(|c| c.name.clone()).collect();
        if identity.is_empty() {
            return RowEditCaps {
                note: Some(
                    "no comparable column: this table's rows can't be addressed by value"
                        .to_string(),
                ),
                no_insert: self.computed_columns(),
                ..RowEditCaps::default()
            };
        }
        RowEditCaps {
            mode: EditMode::BestEffort,
            identity,
            no_update: self
                .columns
                .iter()
                .filter(|c| c.in_any_key() || c.computed())
                .map(|c| c.name.clone())
                .collect(),
            no_insert: self.computed_columns(),
            note: Some(format!(
                "{}: updates and deletes are asynchronous, non-transactional mutations",
                self.engine
            )),
        }
    }

    /// Why this table can't be mutated, or `None` when it can. Named engines rather
    /// than a blanket allow-list so a new `*MergeTree` variant works without a code
    /// change while a `Distributed` table -- which mutates only via its local tables
    /// or `ON CLUSTER` -- keeps saying why it doesn't.
    fn mutation_blocker(&self) -> Option<String> {
        if self.engine.ends_with("MergeTree") {
            return None;
        }
        Some(match self.engine.as_str() {
            "View" | "MaterializedView" | "LiveView" | "WindowView" => {
                "a view is not an edit target; edit the underlying table".to_string()
            }
            "Distributed" => {
                "a Distributed table can't be mutated directly; edit the local tables \
                 or use ALTER … ON CLUSTER"
                    .to_string()
            }
            other => format!("the {other} engine has no UPDATE/DELETE; only MergeTree mutates"),
        })
    }

    /// The engine-computed columns (`MATERIALIZED` / `ALIAS`), which are never
    /// writable even on a table that is otherwise fully editable.
    fn computed_columns(&self) -> Vec<String> {
        self.columns
            .iter()
            .filter(|c| c.computed())
            .map(|c| c.name.clone())
            .collect()
    }
}

/// Strip the wrappers that don't change how a value compares: `Nullable(T)` and
/// `LowCardinality(T)`, in either nesting order. What's left is the type whose
/// spelling decides comparability and the bind placeholder.
pub(super) fn peel_type(type_name: &str) -> &str {
    let t = type_name.trim();
    for wrapper in ["Nullable(", "LowCardinality("] {
        if let Some(inner) = t.strip_prefix(wrapper)
            && let Some(inner) = inner.strip_suffix(')')
        {
            return peel_type(inner);
        }
    }
    t
}

/// Whether a value of this declared type can carry a row's identity through a
/// `WHERE`. The excluded set is not about the engine's expressiveness but about
/// **what the grid holds**: a composite or semi-structured cell arrives as *rendered
/// text*, not as a comparable value, so a predicate built from it would compare a
/// display form against a real column. Floats are excluded for the same reason one
/// step subtler -- the JSON read path doesn't guarantee a round-trip, and an
/// identity that is silently one ULP off matches nothing (or, worse, the wrong row).
pub(super) fn is_comparable(type_name: &str) -> bool {
    let t = peel_type(type_name);
    // Parameterised scalars keep their base name before the `(`: `Decimal(9, 2)`,
    // `DateTime64(3)`, `FixedString(16)`, `Enum8('a' = 1)`.
    let base = t.split('(').next().unwrap_or(t).trim();
    if base.starts_with("Int") || base.starts_with("UInt") {
        return true;
    }
    matches!(
        base,
        "String"
            | "FixedString"
            | "UUID"
            | "Date"
            | "Date32"
            | "DateTime"
            | "DateTime64"
            | "Decimal"
            | "Decimal32"
            | "Decimal64"
            | "Decimal128"
            | "Decimal256"
            | "Enum"
            | "Enum8"
            | "Enum16"
            | "Bool"
            | "IPv4"
            | "IPv6"
    )
}

/// Which statement form a mutation takes. Both are correct; they differ by an order
/// of magnitude in cost, which is why the driver probes rather than assumes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum Form {
    /// `ALTER TABLE … UPDATE` / `ALTER TABLE … DELETE`: enqueues a rewrite of every
    /// part the predicate can touch. Always available, potentially very expensive.
    #[default]
    Mutation,
    /// `UPDATE … SET` / `DELETE FROM …`: the lightweight forms, which write a mask or
    /// a patch part instead of rewriting. Only used when the server says it has them.
    Lightweight,
}

/// Which of the two mutating verbs an op is. Keeps the renderer's match exhaustive
/// over *statements* rather than over [`EditOp`], so the non-mutating `Insert` arm is
/// rejected once, up front, instead of needing an unreachable branch further down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MutKind {
    Update,
    Delete,
}

/// One op rendered for ClickHouse: the statement to POST with its binds, the
/// preflight count that shares those binds, and the same statement with values inline
/// for the confirm dialog.
pub(super) struct OpSql {
    pub(super) sql: String,
    /// `param_*` URL params. Identity values use the `i` prefix, assigned values `v`,
    /// so the two sets can't collide.
    pub(super) params: Vec<(String, String)>,
    /// `SELECT count() FROM <table> WHERE <identity>`: run before the statement, and
    /// bound with the same `param_i*` values.
    pub(super) count_sql: String,
    /// Display only: what the user is asked to approve. Never executed.
    pub(super) display: String,
}

/// Render `op` into everything the driver needs to preflight and run it against
/// `qualified` (an already-quoted `db`.`table`).
///
/// Values are **bound**, never interpolated: each becomes a typed placeholder
/// (`{i0:Int64}`) with its value in a `param_i0` URL param, the same mechanism the
/// seek path uses. The identity conjunction renders `IS NULL` for a null member
/// (a `= NULL` matches nothing, so it would silently address no row).
pub(super) fn render_op(qualified: &str, op: &EditOp, form: Form) -> Result<OpSql> {
    let (kind, keys, set) = match op {
        EditOp::Update { keys, set, .. } => (MutKind::Update, keys, set.as_slice()),
        EditOp::Delete { keys, .. } => (MutKind::Delete, keys, &[][..]),
        EditOp::Insert { .. } => {
            return Err(RedError::Driver(
                "an insert is not a mutation; it rides the bulk insert path".to_string(),
            ));
        }
    };
    // An identity-less op would address every row in the table. There is no reading
    // of the user's intent under which that is what they clicked.
    if keys.is_empty() {
        return Err(RedError::Driver(
            "refusing an edit with no row identity: it would match the whole table".to_string(),
        ));
    }
    if kind == MutKind::Update && set.is_empty() {
        return Err(RedError::Driver("nothing to update".to_string()));
    }

    let mut params: Vec<(String, String)> = Vec::with_capacity(keys.len() + set.len());
    let mut bind = |prefix: &str, i: usize, cv: &ColumnValue| -> Result<String> {
        let name = format!("{prefix}{i}");
        params.push((format!("param_{name}"), param_text(&cv.value)?));
        Ok(format!(
            "{{{name}:{}}}",
            bind_type(cv.decl_type.as_deref(), &cv.value)?
        ))
    };
    let assigns = render_assignments(set, &mut bind)?;
    let where_sql = render_identity(keys, &mut bind)?;

    // The display pass re-renders the same shapes with literals; it shares the
    // builders so the two can't drift apart into "what we showed" vs "what we ran".
    let mut show =
        |_: &str, _: usize, cv: &ColumnValue| -> Result<String> { Ok(display_literal(&cv.value)) };
    let display_assigns = render_assignments(set, &mut show)?;
    let display_where = render_identity(keys, &mut show)?;

    Ok(OpSql {
        sql: statement(qualified, kind, form, &assigns, &where_sql),
        count_sql: format!("SELECT count() FROM {qualified} WHERE {where_sql}"),
        display: statement(qualified, kind, form, &display_assigns, &display_where),
        params,
    })
}

/// The `SET`/`UPDATE` assignment list. A null assignment is the literal `NULL`
/// keyword, so the binder never has to carry a typeless null.
fn render_assignments(
    set: &[ColumnValue],
    slot: &mut impl FnMut(&str, usize, &ColumnValue) -> Result<String>,
) -> Result<String> {
    let mut parts = Vec::with_capacity(set.len());
    for (i, cv) in set.iter().enumerate() {
        let value = match cv.value {
            Value::Null => "NULL".to_string(),
            _ => slot("v", i, cv)?,
        };
        parts.push(format!("{} = {value}", ch_quote(&cv.column)));
    }
    Ok(parts.join(", "))
}

/// The identity conjunction that addresses the row.
fn render_identity(
    keys: &[ColumnValue],
    slot: &mut impl FnMut(&str, usize, &ColumnValue) -> Result<String>,
) -> Result<String> {
    let mut parts = Vec::with_capacity(keys.len());
    for (i, cv) in keys.iter().enumerate() {
        parts.push(match cv.value {
            Value::Null => format!("{} IS NULL", ch_quote(&cv.column)),
            _ => format!("{} = {}", ch_quote(&cv.column), slot("i", i, cv)?),
        });
    }
    Ok(parts.join(" AND "))
}

/// Assemble the statement text for one `(verb, form)` pair.
fn statement(qualified: &str, kind: MutKind, form: Form, assigns: &str, where_sql: &str) -> String {
    match (kind, form) {
        (MutKind::Update, Form::Mutation) => {
            format!("ALTER TABLE {qualified} UPDATE {assigns} WHERE {where_sql}")
        }
        (MutKind::Update, Form::Lightweight) => {
            format!("UPDATE {qualified} SET {assigns} WHERE {where_sql}")
        }
        (MutKind::Delete, Form::Mutation) => {
            format!("ALTER TABLE {qualified} DELETE WHERE {where_sql}")
        }
        (MutKind::Delete, Form::Lightweight) => {
            format!("DELETE FROM {qualified} WHERE {where_sql}")
        }
    }
}

/// The placeholder type to bind a value under.
///
/// Prefer the column's **declared** type, peeled of the wrappers that don't change
/// comparison: a `Decimal(9, 2)` or `DateTime64(3)` compared against a bare `String`
/// param is an "illegal types of arguments" error, and a value that reached the grid
/// as text (which is how ClickHouse's JSON read path returns those) would otherwise
/// bind as exactly that. Two spellings are deliberately *not* used verbatim:
/// `FixedString(n)` (a shorter identity value would be space-padded into a
/// non-match) and `Enum…` (whose type text carries quotes and commas, and which
/// compares against its label as a `String` anyway). With no declared type, fall back
/// to the value's own shape, as the seek path does.
fn bind_type(decl_type: Option<&str>, value: &Value) -> Result<String> {
    let Some(decl) = decl_type else {
        return value_type(value).map(str::to_string);
    };
    let peeled = peel_type(decl);
    let base = peeled.split('(').next().unwrap_or(peeled).trim();
    if base.starts_with("Int") || base.starts_with("UInt") {
        return Ok(base.to_string());
    }
    Ok(match base {
        "FixedString" | "Enum" | "Enum8" | "Enum16" => "String".to_string(),
        "String" | "UUID" | "Date" | "Date32" | "DateTime" | "Bool" | "IPv4" | "IPv6" => {
            base.to_string()
        }
        // Parameterised, and the parameter matters: `DateTime64(3)`, `Decimal(9, 2)`.
        "DateTime64" | "Decimal" | "Decimal32" | "Decimal64" | "Decimal128" | "Decimal256" => {
            peeled.to_string()
        }
        _ => value_type(value)?.to_string(),
    })
}

/// The placeholder type implied by a value's own shape, for a column whose declared
/// type is unknown or unusable as a bind spelling.
fn value_type(value: &Value) -> Result<&'static str> {
    match value {
        Value::Integer(_) => Ok("Int64"),
        Value::Text(_) => Ok("String"),
        Value::Real(_) | Value::Blob(_) | Value::Capped(_) | Value::Null => Err(unbindable(value)),
    }
}

/// The text form of a bound value. ClickHouse substitutes it per the placeholder's
/// declared type, so no quoting is involved and nothing can break out of a literal.
fn param_text(value: &Value) -> Result<String> {
    match value {
        Value::Integer(n) => Ok(n.to_string()),
        Value::Text(s) => Ok(s.to_string()),
        other => Err(unbindable(other)),
    }
}

/// Why a value can't be part of a ClickHouse write. Each of these is a *silent
/// wrong-row* risk rather than an inconvenience: a display-clipped cell is a prefix,
/// a blob has no comparable text form, and a float that round-tripped through JSON
/// may no longer be the number in the column.
fn unbindable(value: &Value) -> RedError {
    RedError::Query(
        match value {
            Value::Real(_) => {
                "a floating-point value can't identify a row: it may not have survived \
                 the read exactly"
            }
            Value::Capped(_) => {
                "this cell is display-clipped, so only its beginning is known; open it \
                 in the inspector to load the full value first"
            }
            Value::Blob(_) => "a binary value can't be compared as a row identity",
            // Reached only if a caller binds a null instead of rendering `IS NULL`.
            Value::Null | Value::Integer(_) | Value::Text(_) => {
                "this value can't be bound into a ClickHouse statement"
            }
        }
        .to_string(),
    )
}

/// A value as a SQL literal, for the **confirm dialog only**. Never used to build an
/// executed statement (those bind), so this is a readability helper, not an
/// injection surface.
fn display_literal(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Integer(n) => n.to_string(),
        Value::Real(x) => x.to_string(),
        Value::Text(s) => format!("'{}'", s.replace('\'', "''")),
        Value::Blob(b) => format!("<{} bytes>", b.len()),
        Value::Capped(c) if c.blob => format!("<{} bytes>", c.len),
        Value::Capped(c) => format!("'{}…'", c.head.replace('\'', "''")),
    }
}

/// What this server will accept, probed once per connection on the first write.
///
/// Every field is measured, never assumed: the lightweight DML spellings and their
/// sync settings arrived in different releases, and sending a setting a server
/// doesn't know is an outright error, not a no-op.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct ChFeatures {
    /// `DELETE FROM … WHERE` (a `_row_exists` mask rather than a part rewrite);
    /// generally available from 23.3.
    pub(super) lightweight_delete: bool,
    /// `UPDATE … SET … WHERE` (patch parts, 25.7+). True only when the server has the
    /// feature **and already has it switched on**: it is experimental, and enabling
    /// an experimental engine feature is the operator's call, not a database
    /// explorer's.
    pub(super) lightweight_update: bool,
    pub(super) has_mutations_sync: bool,
    pub(super) has_lightweight_deletes_sync: bool,
}

/// The `system.settings` names the probe reads. Kept next to [`features_from`], which
/// is the only thing that interprets them.
pub(super) const PROBE_SETTINGS: [&str; 4] = [
    "mutations_sync",
    "lightweight_deletes_sync",
    "allow_experimental_lightweight_update",
    "allow_lightweight_update",
];

impl ChFeatures {
    /// The URL settings to attach to a mutation so a submit that reports success has
    /// really landed. `replicated` picks `mutations_sync = 2` (wait for every replica)
    /// over `1` (this server only): on a `Replicated*` engine, waiting only locally
    /// would report visible while the other replicas are still catching up.
    ///
    /// Only settings the server was observed to have are sent.
    pub(super) fn sync_settings(&self, form: Form, replicated: bool) -> Vec<(String, String)> {
        let mut out = Vec::with_capacity(2);
        if self.has_mutations_sync {
            let level = if replicated { "2" } else { "1" };
            out.push(("mutations_sync".to_string(), level.to_string()));
        }
        if form == Form::Lightweight && self.has_lightweight_deletes_sync {
            out.push(("lightweight_deletes_sync".to_string(), "1".to_string()));
        }
        out
    }

    /// The statement form to use for `kind`.
    pub(super) fn form(&self, verb: &str) -> Form {
        let lightweight = match verb {
            "Delete" => self.lightweight_delete,
            _ => self.lightweight_update,
        };
        if lightweight {
            Form::Lightweight
        } else {
            Form::Mutation
        }
    }
}

/// Interpret a `version()` string plus the `(name, value)` rows of the
/// [`PROBE_SETTINGS`] lookup. Pure, so the version gates are testable without a
/// server -- which matters, because they are the part most likely to drift.
pub(super) fn features_from(version: &str, settings: &[(String, String)]) -> ChFeatures {
    let has = |name: &str| settings.iter().any(|(n, _)| n == name);
    let enabled = |name: &str| {
        settings
            .iter()
            .any(|(n, v)| n == name && !matches!(v.as_str(), "" | "0" | "false"))
    };
    ChFeatures {
        lightweight_delete: at_least(version, 23, 3),
        lightweight_update: enabled("allow_experimental_lightweight_update")
            || enabled("allow_lightweight_update"),
        has_mutations_sync: has("mutations_sync"),
        has_lightweight_deletes_sync: has("lightweight_deletes_sync"),
    }
}

/// Whether `version` (`"23.8.1.2"`, `"24.3.1"`) is at least `major.minor`. An
/// unparseable version reads as "older", so an unknown server gets the always-
/// available `ALTER …` path rather than a spelling it might not have.
pub(super) fn at_least(version: &str, major: u32, minor: u32) -> bool {
    let mut parts = version.trim().split('.').map(str::parse::<u32>);
    let (Some(Ok(hi)), Some(Ok(lo))) = (parts.next(), parts.next()) else {
        return false;
    };
    (hi, lo) >= (major, minor)
}

/// Whether an error body is ClickHouse reporting that the *wait* timed out, not that
/// the statement failed. The mutation was accepted and is still running, so this is
/// reported as "submitted", never as an error the user is tempted to retry -- a
/// retried mutation is a second full part rewrite.
pub(super) fn is_timeout_error(text: &str) -> bool {
    text.contains("TIMEOUT_EXCEEDED")
        || text.contains("Code: 159")
        || text.contains("Timeout exceeded")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, type_name: &str) -> ChColumn {
        ChColumn {
            name: name.into(),
            type_name: type_name.into(),
            default_kind: String::new(),
            in_sorting_key: false,
            in_primary_key: false,
            in_partition_key: false,
            in_sampling_key: false,
        }
    }

    #[test]
    fn peels_nullable_and_lowcardinality_wrappers() {
        assert_eq!(peel_type("Int32"), "Int32");
        assert_eq!(peel_type("Nullable(String)"), "String");
        assert_eq!(peel_type("LowCardinality(Nullable(String))"), "String");
        assert_eq!(peel_type("Nullable(LowCardinality(String))"), "String");
        // A wrapper that isn't one leaves the type alone. `SimpleAggregateFunction`
        // in particular is deliberately *not* peeled: its cell is a partial aggregate
        // that merges on read, so the underlying type's comparability doesn't carry.
        assert_eq!(peel_type("Array(String)"), "Array(String)");
        assert_eq!(
            peel_type("SimpleAggregateFunction(sum, UInt64)"),
            "SimpleAggregateFunction(sum, UInt64)"
        );
    }

    #[test]
    fn comparable_types_exclude_composites_and_floats() {
        for ok in [
            "Int8",
            "UInt64",
            "Int256",
            "String",
            "FixedString(16)",
            "UUID",
            "Date",
            "DateTime64(3)",
            "Decimal(9, 2)",
            "Enum8('a' = 1)",
            "Bool",
            "IPv6",
            "Nullable(Int32)",
            "LowCardinality(String)",
        ] {
            assert!(is_comparable(ok), "{ok} should carry an identity");
        }
        for no in [
            "Float32",
            "Float64",
            "Array(Int32)",
            "Tuple(a Int32, b String)",
            "Map(String, UInt64)",
            "Nested(x Int32)",
            "JSON",
            "Object('json')",
            "Variant(Int32, String)",
            "Dynamic",
            "AggregateFunction(sum, UInt64)",
            "SimpleAggregateFunction(sum, UInt64)",
            "Point",
            "Polygon",
        ] {
            assert!(!is_comparable(no), "{no} must not carry an identity");
        }
    }

    #[test]
    fn sorting_key_leads_the_identity() {
        let mut ts = col("ts", "DateTime");
        ts.in_sorting_key = true;
        ts.in_primary_key = true;
        let facts = ChTableFacts {
            engine: "MergeTree".into(),
            columns: vec![col("name", "String"), ts, col("score", "Float64")],
        };
        let caps = facts.edit_caps();
        assert_eq!(caps.mode, EditMode::BestEffort);
        assert_eq!(
            caps.identity,
            vec!["ts".to_string(), "name".to_string()],
            "sorting key first (part pruning), float excluded"
        );
        assert_eq!(
            caps.no_update,
            vec!["ts".to_string()],
            "key isn't updatable"
        );
        assert!(caps.no_insert.is_empty());
    }

    #[test]
    fn computed_columns_are_never_written() {
        let mut mat = col("day", "Date");
        mat.default_kind = "MATERIALIZED".into();
        let mut alias = col("alias", "String");
        alias.default_kind = "ALIAS".into();
        let mut def = col("n", "Int32");
        def.default_kind = "DEFAULT".into();
        let facts = ChTableFacts {
            engine: "ReplicatedMergeTree".into(),
            columns: vec![col("id", "UInt64"), mat, alias, def],
        };
        let caps = facts.edit_caps();
        assert_eq!(caps.no_insert, vec!["day".to_string(), "alias".to_string()]);
        assert!(
            caps.no_update.contains(&"day".to_string())
                && caps.no_update.contains(&"alias".to_string()),
            "computed columns are also un-updatable"
        );
        assert_eq!(
            caps.identity,
            vec!["id".to_string(), "n".to_string()],
            "a plain DEFAULT column is a normal, writable column"
        );
    }

    #[test]
    fn non_mergetree_engines_report_why_not() {
        for (engine, needle) in [
            ("Memory", "only MergeTree mutates"),
            ("View", "not an edit target"),
            ("MaterializedView", "not an edit target"),
            ("Distributed", "ON CLUSTER"),
        ] {
            let facts = ChTableFacts {
                engine: engine.into(),
                columns: vec![col("id", "UInt64")],
            };
            let caps = facts.edit_caps();
            assert_eq!(caps.mode, EditMode::None, "{engine} can't mutate");
            assert!(caps.identity.is_empty());
            let note = caps.note.unwrap_or_default();
            assert!(note.contains(needle), "{engine} note was {note:?}");
        }
    }

    fn cv(column: &str, value: Value, decl: Option<&str>) -> ColumnValue {
        ColumnValue {
            column: column.into(),
            value,
            decl_type: decl.map(Into::into),
        }
    }

    #[test]
    fn renders_identity_conjunction_with_typed_binds() {
        let op = EditOp::Update {
            table: red_core::TableRef {
                schema: Some("db".into()),
                name: "t".into(),
            },
            keys: vec![
                cv(
                    "ts",
                    Value::Text("2024-01-01 00:00:00".into()),
                    Some("DateTime"),
                ),
                cv("id", Value::Integer(7), Some("UInt64")),
                cv("region", Value::Null, Some("Nullable(String)")),
            ],
            set: vec![cv("name", Value::Text("new".into()), Some("String"))],
        };
        let out = render_op("`db`.`t`", &op, Form::Mutation).unwrap();
        assert_eq!(
            out.sql,
            "ALTER TABLE `db`.`t` UPDATE `name` = {v0:String} \
             WHERE `ts` = {i0:DateTime} AND `id` = {i1:UInt64} AND `region` IS NULL"
        );
        // Sorting-key-first ordering is the caller's (`edit_caps`); what matters here
        // is that every non-null member binds and the null one doesn't.
        assert_eq!(
            out.params,
            vec![
                ("param_v0".to_string(), "new".to_string()),
                ("param_i0".to_string(), "2024-01-01 00:00:00".to_string()),
                ("param_i1".to_string(), "7".to_string()),
            ]
        );
        assert_eq!(
            out.count_sql,
            "SELECT count() FROM `db`.`t` \
             WHERE `ts` = {i0:DateTime} AND `id` = {i1:UInt64} AND `region` IS NULL"
        );
        // The dialog is shown the same statement with values inline.
        assert_eq!(
            out.display,
            "ALTER TABLE `db`.`t` UPDATE `name` = 'new' \
             WHERE `ts` = '2024-01-01 00:00:00' AND `id` = 7 AND `region` IS NULL"
        );
    }

    #[test]
    fn lightweight_forms_render_when_available() {
        let table = red_core::TableRef {
            schema: Some("db".into()),
            name: "t".into(),
        };
        let keys = vec![cv("id", Value::Integer(1), Some("Int32"))];
        let del = EditOp::Delete {
            table: table.clone(),
            keys: keys.clone(),
        };
        assert_eq!(
            render_op("`db`.`t`", &del, Form::Mutation).unwrap().sql,
            "ALTER TABLE `db`.`t` DELETE WHERE `id` = {i0:Int32}"
        );
        assert_eq!(
            render_op("`db`.`t`", &del, Form::Lightweight).unwrap().sql,
            "DELETE FROM `db`.`t` WHERE `id` = {i0:Int32}"
        );
        let upd = EditOp::Update {
            table,
            keys,
            set: vec![cv("n", Value::Integer(2), Some("Int32"))],
        };
        assert_eq!(
            render_op("`db`.`t`", &upd, Form::Lightweight).unwrap().sql,
            "UPDATE `db`.`t` SET `n` = {v0:Int32} WHERE `id` = {i0:Int32}"
        );
    }

    #[test]
    fn refuses_ops_that_cant_address_one_row() {
        let table = red_core::TableRef {
            schema: Some("db".into()),
            name: "t".into(),
        };
        // No identity at all would address the whole table.
        let no_identity = EditOp::Delete {
            table: table.clone(),
            keys: Vec::new(),
        };
        assert!(render_op("`db`.`t`", &no_identity, Form::Mutation).is_err());

        // A display-clipped cell is a prefix, not the value.
        let clipped = EditOp::Delete {
            table: table.clone(),
            keys: vec![cv("s", Value::capped_text("abcdef", 3), Some("String"))],
        };
        let err = match render_op("`db`.`t`", &clipped, Form::Mutation) {
            Err(e) => e,
            Ok(_) => panic!("a display-clipped identity value must be refused"),
        };
        assert!(
            err.to_string().contains("display-clipped"),
            "the refusal names the reason: {err}"
        );

        // A float can't carry an identity through the JSON read path.
        let float = EditOp::Delete {
            table: table.clone(),
            keys: vec![cv("x", Value::Real(1.5), None)],
        };
        assert!(render_op("`db`.`t`", &float, Form::Mutation).is_err());

        // An insert is not a mutation.
        let insert = EditOp::Insert {
            table,
            values: vec![cv("id", Value::Integer(1), None)],
        };
        assert!(render_op("`db`.`t`", &insert, Form::Mutation).is_err());
    }

    #[test]
    fn bind_types_follow_the_declared_column() {
        let int = Value::Integer(1);
        let text = Value::Text("x".into());
        // Parameterised scalars keep their parameter: a Decimal compared against a
        // bare String param is an error, not a coercion.
        assert_eq!(
            bind_type(Some("Decimal(9, 2)"), &text).unwrap(),
            "Decimal(9, 2)"
        );
        assert_eq!(
            bind_type(Some("Nullable(DateTime64(3))"), &text).unwrap(),
            "DateTime64(3)"
        );
        // FixedString binds as String: binding the shorter grid value as FixedString
        // would space-pad it into a non-match.
        assert_eq!(bind_type(Some("FixedString(16)"), &text).unwrap(), "String");
        // An Enum compares against its label, and its type text carries quotes.
        assert_eq!(
            bind_type(Some("Enum8('a' = 1, 'b' = 2)"), &text).unwrap(),
            "String"
        );
        assert_eq!(
            bind_type(Some("LowCardinality(String)"), &text).unwrap(),
            "String"
        );
        assert_eq!(bind_type(Some("UInt256"), &int).unwrap(), "UInt256");
        // No declared type, or one with no usable spelling: fall back to the value.
        assert_eq!(bind_type(None, &int).unwrap(), "Int64");
        assert_eq!(bind_type(Some("Array(Int32)"), &text).unwrap(), "String");
    }

    #[test]
    fn features_are_probed_not_assumed() {
        let setting = |name: &str, value: &str| (name.to_string(), value.to_string());

        // An old server: no lightweight anything, and only the settings it has.
        let old = features_from("22.8.1.2", &[setting("mutations_sync", "0")]);
        assert!(!old.lightweight_delete);
        assert!(!old.lightweight_update);
        assert!(old.has_mutations_sync);
        assert!(!old.has_lightweight_deletes_sync);
        assert_eq!(old.form("Delete"), Form::Mutation);

        // 23.3 brings lightweight DELETE; its sync setting came later, so it is sent
        // only where it was observed.
        let ga = features_from("23.3.1", &[setting("mutations_sync", "0")]);
        assert!(ga.lightweight_delete);
        assert_eq!(ga.form("Delete"), Form::Lightweight);
        assert_eq!(
            ga.sync_settings(Form::Lightweight, false),
            vec![setting("mutations_sync", "1")]
        );
        let newer = features_from(
            "23.11.1",
            &[
                setting("mutations_sync", "0"),
                setting("lightweight_deletes_sync", "2"),
            ],
        );
        assert_eq!(
            newer.sync_settings(Form::Lightweight, true),
            vec![
                setting("mutations_sync", "2"),
                setting("lightweight_deletes_sync", "1")
            ],
            "a Replicated engine waits for every replica"
        );

        // The experimental lightweight UPDATE is used only when the operator has
        // already switched it on; merely having the setting is not consent.
        let present_but_off = features_from(
            "25.7.1",
            &[setting("allow_experimental_lightweight_update", "0")],
        );
        assert!(!present_but_off.lightweight_update);
        assert_eq!(present_but_off.form("Update"), Form::Mutation);
        let switched_on = features_from(
            "25.7.1",
            &[setting("allow_experimental_lightweight_update", "1")],
        );
        assert!(switched_on.lightweight_update);
        assert_eq!(switched_on.form("Update"), Form::Lightweight);

        // An unreadable version reads as "older", never as "newer".
        assert!(!features_from("", &[]).lightweight_delete);
        assert!(!features_from("unknown", &[]).lightweight_delete);
        assert!(at_least("24.3.1", 23, 8) && !at_least("23.3.1", 23, 8));
    }

    #[test]
    fn a_table_of_only_composites_is_not_editable() {
        let facts = ChTableFacts {
            engine: "MergeTree".into(),
            columns: vec![col("tags", "Array(String)"), col("score", "Float64")],
        };
        let caps = facts.edit_caps();
        assert_eq!(caps.mode, EditMode::None);
        assert!(
            caps.note
                .unwrap_or_default()
                .contains("no comparable column"),
            "the refusal names its reason"
        );
    }
}
