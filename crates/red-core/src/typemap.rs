//! A small normalized type lattice for cross-engine table creation, the keystone of
//! the database-migration / "copy into a *new* table" path.
//!
//! dbgate carries a column's declared-type string **verbatim** across engines and
//! lets a foreign type (`jsonb`, `uuid`, an array) fail at execute time. Red instead
//! parses each engine's declared type into the small [`NormType`] lattice and spells
//! it back out for the *target* engine, so a Postgres `int4` / `numeric(10,2)` /
//! `bool` / `timestamptz` becomes a faithful MySQL/SQLite/Postgres column rather than
//! invalid DDL. Types the lattice can't classify (`tsvector`, arrays, custom enums)
//! fall through **verbatim** (exactly dbgate's behaviour) and report `true` from
//! [`is_lossy`] so the UI can warn before the user consents.
//!
//! This lives in `red-core` (not a driver) because both sides need it: the drivers
//! spell DDL for their own engine, and the UI flags lossy columns in the create
//! preview. Same-engine creates round-trip faithfully (e.g. SQLite `INTEGER` →
//! [`NormType::Int`] → `INTEGER`); the lattice only earns its keep cross-engine.

use crate::DbKind;

/// An engine-neutral column type. Deliberately coarse: enough to recreate a column
/// in another dialect, not a full type system. Length/precision ride along where they
/// matter (so `varchar(255)` survives a cross-engine create); everything the lattice
/// can't model is an [`NormType::Unknown`] carrying the original spelling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NormType {
    Bool,
    Int,
    BigInt,
    Real,
    Decimal {
        precision: Option<u8>,
        scale: Option<u8>,
    },
    /// Variable-length text; `length` is the declared cap when the source carried one
    /// (`None` = unbounded / unspecified).
    Text {
        length: Option<u32>,
    },
    Blob,
    Uuid,
    Json,
    Date,
    Time,
    Timestamp,
    TimestampTz,
    /// An engine-specific type the lattice doesn't model (array, geometry, enum, …):
    /// the original declared spelling, passed through verbatim and treated as lossy
    /// for any cross-engine create.
    Unknown(String),
}

/// Parse an engine-native declared type (`"character varying(255)"`, `"int4"`,
/// `"Nullable(Int64)"`, `"DECIMAL(10,2)"`) into the lattice: best-effort, dialect-
/// agnostic, case-insensitive. An empty/`None` type maps to unbounded text.
pub fn normalize(decl: &str) -> NormType {
    let raw = decl.trim();
    if raw.is_empty() {
        return NormType::Text { length: None };
    }
    let lower = raw.to_ascii_lowercase();

    // ClickHouse wrappers: `Nullable(T)`, `LowCardinality(T)` → classify the inner T.
    for wrap in ["nullable(", "lowcardinality("] {
        if let Some(rest) = lower.strip_prefix(wrap) {
            if let Some(inner) = rest.strip_suffix(')') {
                return normalize(inner);
            }
        }
    }

    let has_tz = lower.contains("with time zone") || lower.ends_with("tz");
    let (head, args) = match lower.split_once('(') {
        Some((h, rest)) => (h.trim(), rest.trim_end_matches(')').trim()),
        None => (lower.as_str(), ""),
    };
    // The base keyword: first word of the head (drops "varying", "precision", "unsigned").
    let base = head.split_whitespace().next().unwrap_or(head);

    match base {
        "bool" | "boolean" => NormType::Bool,
        // MySQL's idiomatic boolean is `tinyint(1)`; wider tinyint is a small int.
        "tinyint" if args.starts_with('1') => NormType::Bool,

        "bigint" | "int8" | "bigserial" | "serial8" | "int64" | "uint64" | "long" => {
            NormType::BigInt
        }
        "int" | "integer" | "int4" | "serial" | "serial4" | "int32" | "uint32" | "smallint"
        | "int2" | "smallserial" | "serial2" | "int16" | "uint16" | "mediumint" | "tinyint"
        | "uint8" | "year" => NormType::Int,

        "real" | "float" | "float4" | "float8" | "double" | "float32" | "float64" => NormType::Real,

        "decimal" | "numeric" | "dec" | "money" | "fixed" => {
            let (precision, scale) = parse_decimal_args(args);
            NormType::Decimal { precision, scale }
        }

        "uuid" | "uniqueidentifier" => NormType::Uuid,
        "json" | "jsonb" => NormType::Json,

        "varchar" | "char" | "character" | "varchar2" | "nvarchar" | "nchar" | "varying"
        | "text" | "string" | "clob" | "tinytext" | "mediumtext" | "longtext" | "ntext"
        | "enum" | "set" | "name" | "citext" | "fixedstring" => NormType::Text {
            length: parse_length(args),
        },

        "blob" | "bytea" | "binary" | "varbinary" | "tinyblob" | "mediumblob" | "longblob"
        | "bytes" | "image" => NormType::Blob,

        "date" => NormType::Date,
        "time" => NormType::Time,
        "timestamptz" => NormType::TimestampTz,
        "timestamp" | "datetime" | "datetime64" | "datetime2" | "smalldatetime" => {
            if has_tz {
                NormType::TimestampTz
            } else {
                NormType::Timestamp
            }
        }

        _ => NormType::Unknown(raw.to_string()),
    }
}

/// Spell a [`NormType`] as a `CREATE TABLE` column type for `kind`'s dialect.
/// [`NormType::Unknown`] passes through verbatim (the source spelling); the engine
/// accepts it (SQLite, by affinity) or rejects it at execute time, like dbgate.
pub fn spell(kind: DbKind, t: &NormType) -> String {
    match kind {
        DbKind::Sqlite => spell_sqlite(t),
        DbKind::Postgres => spell_postgres(t),
        DbKind::Mysql => spell_mysql(t),
        DbKind::Clickhouse => spell_clickhouse(t),
        // Redis has no column/DDL model at all (see docs/plans/redis.md); it
        // isn't a `DatabaseDriver`, so it can never appear in a create-table or
        // migration target picker (`write_caps().insert` is false, which the
        // pickers filter on). Degrade to an empty type rather than panicking on
        // the backend thread if that invariant ever regresses; debug builds still
        // trip the assert.
        DbKind::Redis => {
            debug_assert!(false, "Redis has no column/DDL model");
            String::new()
        }
    }
}

/// Whether spelling `t` for `target` loses fidelity worth warning about in the create
/// preview: an [`NormType::Unknown`] (verbatim, maybe foreign), a type SQLite collapses
/// to an affinity (`uuid`/`json`/temporal → `TEXT`, `bool` → `INTEGER`), or a type with
/// no native target equivalent (`uuid` on MySQL → `char(36)`). Best-effort and one-
/// sided (it doesn't know the source engine), so it over-reports rather than hides a
/// silent lossy create.
pub fn is_lossy(target: DbKind, t: &NormType) -> bool {
    if let NormType::Unknown(_) = t {
        return true;
    }
    match target {
        DbKind::Sqlite => !matches!(
            t,
            NormType::Int
                | NormType::BigInt
                | NormType::Real
                | NormType::Text { .. }
                | NormType::Blob
        ),
        DbKind::Mysql => matches!(t, NormType::Uuid),
        DbKind::Postgres => false,
        // ClickHouse is never a create target; treat as lossy for completeness.
        DbKind::Clickhouse => true,
        // Redis is never a create target either (no column/DDL model).
        DbKind::Redis => true,
    }
}

fn spell_sqlite(t: &NormType) -> String {
    // SQLite's type affinity collapses most types; that's correct, not lossy, except
    // uuid/json/temporal which round-trip as TEXT (flagged by `is_lossy`).
    match t {
        NormType::Bool | NormType::Int | NormType::BigInt => "INTEGER",
        NormType::Real => "REAL",
        NormType::Decimal { .. } => "NUMERIC",
        NormType::Text { .. }
        | NormType::Uuid
        | NormType::Json
        | NormType::Date
        | NormType::Time
        | NormType::Timestamp
        | NormType::TimestampTz => "TEXT",
        NormType::Blob => "BLOB",
        NormType::Unknown(s) => return s.clone(),
    }
    .to_string()
}

fn spell_postgres(t: &NormType) -> String {
    match t {
        NormType::Bool => "boolean".into(),
        NormType::Int => "integer".into(),
        NormType::BigInt => "bigint".into(),
        NormType::Real => "double precision".into(),
        NormType::Decimal { precision, scale } => decimal_spelling("numeric", *precision, *scale),
        NormType::Text { length } => match length {
            Some(l) => format!("varchar({l})"),
            None => "text".into(),
        },
        NormType::Blob => "bytea".into(),
        NormType::Uuid => "uuid".into(),
        NormType::Json => "jsonb".into(),
        NormType::Date => "date".into(),
        NormType::Time => "time".into(),
        NormType::Timestamp => "timestamp".into(),
        NormType::TimestampTz => "timestamptz".into(),
        NormType::Unknown(s) => s.clone(),
    }
}

fn spell_mysql(t: &NormType) -> String {
    match t {
        NormType::Bool => "tinyint(1)".into(),
        NormType::Int => "int".into(),
        NormType::BigInt => "bigint".into(),
        NormType::Real => "double".into(),
        NormType::Decimal { precision, scale } => decimal_spelling("decimal", *precision, *scale),
        // MySQL `varchar` needs a length and is row-size limited; widen long/unbounded
        // text to `longtext` so a create never fails on the cap.
        NormType::Text { length } => match length {
            Some(l) if *l <= 16_383 => format!("varchar({l})"),
            _ => "longtext".into(),
        },
        NormType::Blob => "longblob".into(),
        NormType::Uuid => "char(36)".into(),
        NormType::Json => "json".into(),
        NormType::Date => "date".into(),
        NormType::Time => "time".into(),
        NormType::Timestamp | NormType::TimestampTz => "datetime".into(),
        NormType::Unknown(s) => s.clone(),
    }
}

fn spell_clickhouse(t: &NormType) -> String {
    // ClickHouse is read-only in v1 (never a create target); this keeps `spell`
    // total and is a sensible mapping should that change.
    match t {
        NormType::Bool => "Bool".into(),
        NormType::Int => "Int32".into(),
        NormType::BigInt => "Int64".into(),
        NormType::Real => "Float64".into(),
        NormType::Decimal { precision, scale } => decimal_spelling("Decimal", *precision, *scale),
        NormType::Text { .. } | NormType::Json => "String".into(),
        NormType::Blob => "String".into(),
        NormType::Uuid => "UUID".into(),
        NormType::Date => "Date".into(),
        NormType::Time => "String".into(),
        NormType::Timestamp | NormType::TimestampTz => "DateTime".into(),
        NormType::Unknown(s) => s.clone(),
    }
}

fn decimal_spelling(name: &str, precision: Option<u8>, scale: Option<u8>) -> String {
    match (precision, scale) {
        (Some(p), Some(s)) => format!("{name}({p},{s})"),
        (Some(p), None) => format!("{name}({p})"),
        _ => name.to_string(),
    }
}

/// Parse `"10,2"` / `"10"` / `""` into `(precision, scale)`.
fn parse_decimal_args(args: &str) -> (Option<u8>, Option<u8>) {
    let mut parts = args.split(',').map(|p| p.trim().parse::<u8>().ok());
    let precision = parts.next().flatten();
    let scale = parts.next().flatten();
    (precision, scale)
}

/// Parse a text length from `"255"` / `"255 char"`; `"max"`/non-numeric → `None`.
fn parse_length(args: &str) -> Option<u32> {
    args.split([',', ' '])
        .next()
        .and_then(|s| s.trim().parse::<u32>().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_common_dialect_spellings() {
        assert_eq!(normalize("int4"), NormType::Int);
        assert_eq!(normalize("INTEGER"), NormType::Int);
        assert_eq!(normalize("bigint"), NormType::BigInt);
        assert_eq!(normalize("int8"), NormType::BigInt);
        assert_eq!(normalize("bool"), NormType::Bool);
        assert_eq!(normalize("tinyint(1)"), NormType::Bool);
        assert_eq!(normalize("tinyint(4)"), NormType::Int);
        assert_eq!(normalize("double precision"), NormType::Real);
        assert_eq!(normalize("jsonb"), NormType::Json);
        assert_eq!(normalize("uuid"), NormType::Uuid);
        assert_eq!(normalize("bytea"), NormType::Blob);
        assert_eq!(normalize("date"), NormType::Date);
        assert_eq!(
            normalize("timestamp without time zone"),
            NormType::Timestamp
        );
        assert_eq!(normalize("timestamp with time zone"), NormType::TimestampTz);
        assert_eq!(normalize("timestamptz"), NormType::TimestampTz);
        assert_eq!(normalize("datetime"), NormType::Timestamp);
        assert_eq!(normalize("Nullable(Int64)"), NormType::BigInt);
        assert_eq!(
            normalize("LowCardinality(String)"),
            NormType::Text { length: None }
        );
    }

    #[test]
    fn keeps_length_and_precision() {
        assert_eq!(
            normalize("varchar(255)"),
            NormType::Text { length: Some(255) }
        );
        assert_eq!(
            normalize("character varying(40)"),
            NormType::Text { length: Some(40) }
        );
        assert_eq!(
            normalize("character varying"),
            NormType::Text { length: None }
        );
        assert_eq!(
            normalize("numeric(10,2)"),
            NormType::Decimal {
                precision: Some(10),
                scale: Some(2)
            }
        );
        assert_eq!(
            normalize("decimal(12)"),
            NormType::Decimal {
                precision: Some(12),
                scale: None
            }
        );
    }

    #[test]
    fn unknown_passes_through_verbatim() {
        assert_eq!(normalize("tsvector"), NormType::Unknown("tsvector".into()));
        assert_eq!(normalize("text[]"), NormType::Unknown("text[]".into()));
        // …and spells back out unchanged, on every engine with a column/DDL
        // model at all. Not `DbKind::all()`: Redis has none (`spell` is
        // `unreachable!()` for it), so it isn't "every engine" here even
        // though the connection form lists it alongside these four.
        for k in [
            DbKind::Sqlite,
            DbKind::Postgres,
            DbKind::Mysql,
            DbKind::Clickhouse,
        ] {
            assert_eq!(spell(k, &NormType::Unknown("tsvector".into())), "tsvector");
        }
    }

    #[test]
    fn same_engine_roundtrips_faithfully() {
        // SQLite
        assert_eq!(spell(DbKind::Sqlite, &normalize("INTEGER")), "INTEGER");
        assert_eq!(spell(DbKind::Sqlite, &normalize("TEXT")), "TEXT");
        assert_eq!(spell(DbKind::Sqlite, &normalize("REAL")), "REAL");
        // Postgres
        assert_eq!(spell(DbKind::Postgres, &normalize("integer")), "integer");
        assert_eq!(spell(DbKind::Postgres, &normalize("bigint")), "bigint");
        assert_eq!(
            spell(DbKind::Postgres, &normalize("varchar(80)")),
            "varchar(80)"
        );
        assert_eq!(
            spell(DbKind::Postgres, &normalize("numeric(10,2)")),
            "numeric(10,2)"
        );
        assert_eq!(spell(DbKind::Postgres, &normalize("jsonb")), "jsonb");
        // MySQL
        assert_eq!(spell(DbKind::Mysql, &normalize("int")), "int");
        assert_eq!(
            spell(DbKind::Mysql, &normalize("varchar(120)")),
            "varchar(120)"
        );
        assert_eq!(spell(DbKind::Mysql, &normalize("datetime")), "datetime");
    }

    #[test]
    fn cross_engine_maps_sensibly() {
        // Postgres → MySQL
        assert_eq!(spell(DbKind::Mysql, &normalize("boolean")), "tinyint(1)");
        assert_eq!(spell(DbKind::Mysql, &normalize("int4")), "int");
        assert_eq!(spell(DbKind::Mysql, &normalize("timestamptz")), "datetime");
        assert_eq!(spell(DbKind::Mysql, &normalize("uuid")), "char(36)");
        // Postgres → SQLite (affinity)
        assert_eq!(spell(DbKind::Sqlite, &normalize("boolean")), "INTEGER");
        assert_eq!(spell(DbKind::Sqlite, &normalize("jsonb")), "TEXT");
        assert_eq!(spell(DbKind::Sqlite, &normalize("uuid")), "TEXT");
        // MySQL → Postgres
        assert_eq!(spell(DbKind::Postgres, &normalize("tinyint(1)")), "boolean");
        assert_eq!(spell(DbKind::Postgres, &normalize("longtext")), "text");
        assert_eq!(spell(DbKind::Postgres, &normalize("datetime")), "timestamp");
    }

    #[test]
    fn lossy_flags_the_right_columns() {
        // SQLite collapses these.
        assert!(is_lossy(DbKind::Sqlite, &normalize("uuid")));
        assert!(is_lossy(DbKind::Sqlite, &normalize("jsonb")));
        assert!(is_lossy(DbKind::Sqlite, &normalize("timestamptz")));
        assert!(!is_lossy(DbKind::Sqlite, &normalize("int")));
        assert!(!is_lossy(DbKind::Sqlite, &normalize("varchar(20)")));
        // MySQL has no native uuid.
        assert!(is_lossy(DbKind::Mysql, &normalize("uuid")));
        assert!(!is_lossy(DbKind::Mysql, &normalize("int")));
        // Postgres is the most expressive target.
        assert!(!is_lossy(DbKind::Postgres, &normalize("jsonb")));
        // Unknown is always lossy cross-engine.
        assert!(is_lossy(DbKind::Postgres, &normalize("tsvector")));
    }
}
