//! `CREATE TABLE` rendering, shared by the drivers that execute it and the UI
//! that previews it.
//!
//! It lives here rather than in `red-driver` for one reason: the transfer
//! wizard shows the user the `CREATE` it is about to run, and a preview
//! assembled by a second, view-side string builder is a preview that drifts from
//! what executes. One function, two callers, differing only in the identifier
//! quoter they pass ([`quote_generic`] for a preview, the engine's own for the
//! live statement).
//!
//! Type spelling comes from [`crate::typemap`], so a cross-engine create emits
//! the *target's* type names. Nothing here interpolates raw user text: every
//! identifier goes through `quote`, and the only free-form text is a column
//! `DEFAULT`, which callers must strip unless both ends are the same engine (see
//! [`create_table_sql`]).

use crate::{ColumnMeta, DbKind, TableRef};

/// Double-quote an identifier, doubling any embedded quote. The engine-agnostic
/// quoter for a *preview* (the live statement uses the driver's own), matching
/// what `EditOp::preview_sql` already does for a pending write.
pub fn quote_generic(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// Qualify and quote a table reference (`schema.name`, or just `name` when
/// there's no schema) using `quote`.
pub fn qualify_table(table: &TableRef, quote: impl Fn(&str) -> String) -> String {
    match &table.schema {
        Some(s) if !s.is_empty() => format!("{}.{}", quote(s), quote(&table.name)),
        _ => quote(&table.name),
    }
}

/// Build a `CREATE TABLE IF NOT EXISTS` for `table` from `columns`, spelling each
/// column's declared type into `kind`'s dialect via [`crate::typemap`]. A
/// Postgres `int4`/`numeric(10,2)`/`jsonb`/`uuid` becomes a faithful column in the
/// target engine instead of invalid DDL; a type the lattice can't classify falls
/// through verbatim (the engine accepts or rejects it, like dbgate). `NOT NULL` is
/// emitted, primary-key columns are gathered into a trailing `PRIMARY KEY (…)`, and an
/// auto-increment column is re-spelled per dialect (SQLite `INTEGER PRIMARY KEY`,
/// Postgres `serial`/`bigserial`, MySQL `… AUTO_INCREMENT`) so the migrated table keeps
/// auto-numbering. Indexes and foreign keys are **not** emitted here; they ride a
/// deferred pass after the data loads.
///
/// A column's [`default`](ColumnMeta::default) is emitted verbatim when present,
/// because a default is engine text (`nextval('…')`, `CURRENT_TIMESTAMP`, a
/// typed literal) that only the engine that produced it is guaranteed to parse.
/// **Callers moving between two engines must clear it first** ([`strip_defaults`]);
/// a same-engine duplicate keeps it, which is the whole point of carrying it. It
/// is skipped on auto-increment columns, whose per-dialect spelling already
/// implies one.
///
/// Identifiers are quoted by `quote`; the only other interpolated text comes from
/// the fixed per-engine spelling table.
pub fn create_table_sql(
    table: &TableRef,
    columns: &[ColumnMeta],
    kind: DbKind,
    quote: impl Fn(&str) -> String,
) -> String {
    use crate::typemap::{NormType, normalize, spell};
    let qualify = qualify_table(table, &quote);
    let pk_count = columns.iter().filter(|c| c.primary_key).count();
    // SQLite expresses a sole-INTEGER-PK auto-increment column *inline* as
    // `INTEGER PRIMARY KEY` (the rowid alias), which then must NOT also appear in a
    // trailing PRIMARY KEY clause.
    let sqlite_inline_pk = kind == DbKind::Sqlite
        && pk_count == 1
        && columns.iter().any(|c| c.primary_key && c.auto_increment);
    let mut defs: Vec<String> = columns
        .iter()
        .map(|c| {
            let nt = normalize(c.type_name.as_deref().unwrap_or(""));
            if c.auto_increment {
                match kind {
                    DbKind::Sqlite if sqlite_inline_pk && c.primary_key => {
                        format!("{} INTEGER PRIMARY KEY", quote(&c.name))
                    }
                    // A non-sole-PK auto-inc in SQLite can't be the rowid alias; emit a
                    // plain INTEGER (the values still carry across; future auto-numbering
                    // is the only loss).
                    DbKind::Sqlite => format!("{} INTEGER", quote(&c.name)),
                    DbKind::Postgres => {
                        let serial = if matches!(nt, NormType::BigInt) {
                            "bigserial"
                        } else {
                            "serial"
                        };
                        format!("{} {serial}", quote(&c.name))
                    }
                    DbKind::Mysql => {
                        format!("{} {} AUTO_INCREMENT", quote(&c.name), spell(kind, &nt))
                    }
                    DbKind::Clickhouse => format!("{} {}", quote(&c.name), spell(kind, &nt)),
                    // No column/DDL model, no `DatabaseDriver` impl, so this
                    // never sees `DbKind::Redis` (see `typemap::spell`). Degrade
                    // rather than panic on the backend thread if that ever breaks.
                    DbKind::Redis => {
                        debug_assert!(false, "Redis has no column/DDL model");
                        format!("{} {}", quote(&c.name), spell(kind, &nt))
                    }
                    // Schemaless document store, no `DatabaseDriver`/DDL model, so
                    // this never sees `DbKind::Mongo` (see `typemap::spell`).
                    DbKind::Mongo => {
                        debug_assert!(false, "MongoDB has no column/DDL model");
                        format!("{} {}", quote(&c.name), spell(kind, &nt))
                    }
                }
            } else {
                let ty = spell(kind, &nt);
                let null = if c.not_null { " NOT NULL" } else { "" };
                let default = match c.default.as_deref().map(str::trim) {
                    Some(d) if !d.is_empty() => format!(" DEFAULT {d}"),
                    _ => String::new(),
                };
                format!("{} {ty}{null}{default}", quote(&c.name))
            }
        })
        .collect();
    if pk_count > 0 && !sqlite_inline_pk {
        let pk = columns
            .iter()
            .filter(|c| c.primary_key)
            .map(|c| quote(&c.name))
            .collect::<Vec<_>>()
            .join(", ");
        defs.push(format!("PRIMARY KEY ({pk})"));
    }
    format!("CREATE TABLE IF NOT EXISTS {qualify} ({})", defs.join(", "))
}

/// Build a `CREATE [UNIQUE] INDEX` for `table` over `columns` in `kind`'s dialect: the
/// shared body of every driver's `create_index`, run as the transfer's deferred
/// index pass. `IF NOT EXISTS` is used where the engine
/// supports it (not MySQL). Identifiers are quoted by `quote`, never interpolated raw.
pub fn create_index_sql(
    table: &TableRef,
    name: &str,
    unique: bool,
    columns: &[String],
    kind: DbKind,
    quote: impl Fn(&str) -> String,
) -> String {
    let uniq = if unique { "UNIQUE " } else { "" };
    // MySQL has no `IF NOT EXISTS` for `CREATE INDEX`; SQLite and Postgres do.
    let guard = if matches!(kind, DbKind::Mysql) {
        ""
    } else {
        "IF NOT EXISTS "
    };
    let cols = columns
        .iter()
        .map(|c| quote(c))
        .collect::<Vec<_>>()
        .join(", ");
    match kind {
        // SQLite puts the schema on the *index name*; the table name in `CREATE INDEX`
        // is never schema-qualified (`CREATE INDEX main.ix ON child(...)`).
        DbKind::Sqlite => {
            let idx = match &table.schema {
                Some(s) if !s.is_empty() => format!("{}.{}", quote(s), quote(name)),
                _ => quote(name),
            };
            format!(
                "CREATE {uniq}INDEX {guard}{idx} ON {} ({cols})",
                quote(&table.name)
            )
        }
        // Postgres/MySQL: bare index name, schema-qualified table.
        _ => format!(
            "CREATE {uniq}INDEX {guard}{} ON {} ({cols})",
            quote(name),
            qualify_table(table, &quote)
        ),
    }
}

/// Build an `ALTER TABLE … ADD FOREIGN KEY (…) REFERENCES … (…)`, the shared body
/// of every (FK-capable) driver's `add_foreign_key`. No referential
/// actions are emitted (`FkEdge` doesn't carry them). Identifiers are quoted, never
/// interpolated raw. The syntax is the same on Postgres and MySQL, so there is no
/// `kind` parameter; an engine that needs its own spelling would not share this body.
pub fn add_fk_sql(
    child: &TableRef,
    columns: &[String],
    parent: &TableRef,
    ref_columns: &[String],
    quote: impl Fn(&str) -> String,
) -> String {
    let cols = columns
        .iter()
        .map(|c| quote(c))
        .collect::<Vec<_>>()
        .join(", ");
    let refs = ref_columns
        .iter()
        .map(|c| quote(c))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "ALTER TABLE {} ADD FOREIGN KEY ({cols}) REFERENCES {} ({refs})",
        qualify_table(child, &quote),
        qualify_table(parent, &quote)
    )
}

/// Build a `DROP TABLE IF EXISTS`: the destructive half of a recreate, shared by
/// the drivers that run it and the dry-run script that shows it.
///
/// No `CASCADE`. A recreate is meant to replace one table, not to take whatever
/// depends on it; an engine that refuses the drop because something references
/// the table is telling the user something they need to hear.
pub fn drop_table_sql(table: &TableRef, quote: impl Fn(&str) -> String) -> String {
    format!("DROP TABLE IF EXISTS {}", qualify_table(table, quote))
}

/// Clear every column's `DEFAULT`, returning the columns a *cross-engine* create
/// should use.
///
/// A default is the one piece of [`ColumnMeta`] that is verbatim engine text,
/// so carrying `nextval('users_id_seq'::regclass)` into SQLite produces DDL that
/// fails at execute time and takes the whole transfer with it. Dropping it costs
/// the target its defaults, which is recoverable; keeping it costs the transfer,
/// which is not.
pub fn strip_defaults(columns: &[ColumnMeta]) -> Vec<ColumnMeta> {
    columns
        .iter()
        .map(|c| ColumnMeta {
            default: None,
            ..c.clone()
        })
        .collect()
}

/// Clear the flags a [`crate::transfer::TransferOptions`] turned off, so the
/// created table carries only what the user asked for. Defaults ride with the
/// column shape and are handled by [`strip_defaults`] instead, because their
/// hazard is cross-engine text rather than user preference.
pub fn apply_shape_options(columns: &[ColumnMeta], primary_keys: bool) -> Vec<ColumnMeta> {
    if primary_keys {
        return columns.to_vec();
    }
    columns
        .iter()
        .map(|c| ColumnMeta {
            primary_key: false,
            // An auto-increment column is a primary key on every engine that
            // spells one, so dropping the PK has to drop the auto-numbering with
            // it or the DDL contradicts itself.
            auto_increment: false,
            ..c.clone()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, ty: &str) -> ColumnMeta {
        ColumnMeta {
            name: name.into(),
            type_name: Some(ty.into()),
            not_null: false,
            primary_key: false,
            default: None,
            auto_increment: false,
        }
    }

    fn tref() -> TableRef {
        TableRef {
            schema: None,
            name: "t".into(),
        }
    }

    #[test]
    fn create_table_sql_emits_auto_increment_per_dialect() {
        let cols = vec![
            ColumnMeta {
                name: "id".into(),
                type_name: Some("bigint".into()),
                not_null: true,
                primary_key: true,
                default: None,
                auto_increment: true,
            },
            ColumnMeta {
                name: "name".into(),
                type_name: Some("text".into()),
                not_null: false,
                primary_key: false,
                default: None,
                auto_increment: false,
            },
        ];
        let t = tref();
        // SQLite: a sole INTEGER PK auto-inc column is emitted inline as the rowid
        // alias, and must NOT also appear in a trailing PRIMARY KEY clause.
        let s = create_table_sql(&t, &cols, DbKind::Sqlite, quote_generic);
        assert!(s.contains("\"id\" INTEGER PRIMARY KEY"), "{s}");
        assert!(!s.contains("PRIMARY KEY (\"id\")"), "{s}");
        // Postgres: bigserial (Int → serial) + a trailing PK clause.
        let p = create_table_sql(&t, &cols, DbKind::Postgres, quote_generic);
        assert!(p.contains("\"id\" bigserial"), "{p}");
        assert!(p.contains("PRIMARY KEY (\"id\")"), "{p}");
        // MySQL: `<type> AUTO_INCREMENT` + a trailing PK clause.
        let m = create_table_sql(&t, &cols, DbKind::Mysql, |i| format!("`{i}`"));
        assert!(m.contains("`id` bigint AUTO_INCREMENT"), "{m}");
        assert!(m.contains("PRIMARY KEY (`id`)"), "{m}");
    }

    #[test]
    fn add_fk_and_create_index_sql_quote_identifiers() {
        let child = TableRef {
            schema: Some("public".into()),
            name: "child".into(),
        };
        let parent = TableRef {
            schema: Some("public".into()),
            name: "parent".into(),
        };
        let q = quote_generic;
        assert_eq!(
            add_fk_sql(&child, &["parent_id".into()], &parent, &["id".into()], q),
            "ALTER TABLE \"public\".\"child\" ADD FOREIGN KEY (\"parent_id\") \
             REFERENCES \"public\".\"parent\" (\"id\")"
        );
        // Postgres: UNIQUE off, `IF NOT EXISTS` supported.
        assert_eq!(
            create_index_sql(
                &child,
                "ix_child_pid",
                false,
                &["parent_id".into()],
                DbKind::Postgres,
                q
            ),
            "CREATE INDEX IF NOT EXISTS \"ix_child_pid\" ON \"public\".\"child\" (\"parent_id\")"
        );
        // MySQL: UNIQUE on, no `IF NOT EXISTS`, composite columns.
        let myq = |i: &str| format!("`{i}`");
        assert_eq!(
            create_index_sql(
                &child,
                "ix",
                true,
                &["a".into(), "b".into()],
                DbKind::Mysql,
                myq
            ),
            "CREATE UNIQUE INDEX `ix` ON `public`.`child` (`a`, `b`)"
        );
        // SQLite: the schema rides on the *index name*, the table is bare.
        assert_eq!(
            create_index_sql(
                &child,
                "ix",
                false,
                &["parent_id".into()],
                DbKind::Sqlite,
                q
            ),
            "CREATE INDEX IF NOT EXISTS \"public\".\"ix\" ON \"child\" (\"parent_id\")"
        );
    }

    #[test]
    fn defaults_ride_when_present() {
        let mut c = col("created_at", "timestamp");
        c.default = Some("CURRENT_TIMESTAMP".into());
        c.not_null = true;
        let sql = create_table_sql(&tref(), &[c], DbKind::Postgres, quote_generic);
        assert!(
            sql.contains("NOT NULL DEFAULT CURRENT_TIMESTAMP"),
            "got {sql}"
        );
    }

    #[test]
    fn stripping_defaults_leaves_the_rest_of_the_shape() {
        let mut c = col("n", "integer");
        c.default = Some("42".into());
        c.not_null = true;
        let stripped = strip_defaults(&[c]);
        assert_eq!(stripped[0].default, None);
        assert!(stripped[0].not_null);
        let sql = create_table_sql(&tref(), &stripped, DbKind::Sqlite, quote_generic);
        assert!(!sql.contains("DEFAULT"), "got {sql}");
    }

    #[test]
    fn an_auto_increment_column_never_also_gets_a_default() {
        // Postgres reports `serial` columns with a `nextval(…)` default; emitting
        // both would be `serial DEFAULT nextval(…)`, which duplicates the sequence.
        let mut c = col("id", "integer");
        c.auto_increment = true;
        c.primary_key = true;
        c.default = Some("nextval('t_id_seq'::regclass)".into());
        let sql = create_table_sql(&tref(), &[c], DbKind::Postgres, quote_generic);
        assert!(sql.contains("\"id\" serial"), "got {sql}");
        assert!(!sql.contains("nextval"), "got {sql}");
    }

    #[test]
    fn dropping_primary_keys_drops_auto_increment_with_them() {
        let mut c = col("id", "integer");
        c.auto_increment = true;
        c.primary_key = true;
        let shaped = apply_shape_options(&[c], false);
        assert!(!shaped[0].primary_key && !shaped[0].auto_increment);
        let sql = create_table_sql(&tref(), &shaped, DbKind::Postgres, quote_generic);
        assert!(!sql.contains("PRIMARY KEY"), "got {sql}");
        assert!(!sql.contains("serial"), "got {sql}");
    }

    #[test]
    fn an_empty_default_is_not_emitted() {
        let mut c = col("n", "integer");
        c.default = Some("   ".into());
        let sql = create_table_sql(&tref(), &[c], DbKind::Sqlite, quote_generic);
        assert!(!sql.contains("DEFAULT"), "got {sql}");
    }
}
