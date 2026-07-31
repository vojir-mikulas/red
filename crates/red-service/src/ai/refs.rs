//! Resolving what the user pointed at into the text the model reads.
//!
//! Half of what a schema-grounded agent gets wrong is *which object the user
//! meant*. A reference removes that: the user points, and the same formatter
//! `describe_table` uses renders the answer — so a dragged table and a tool call
//! describe it identically, which is the property most likely to rot and the one
//! most worth holding.
//!
//! Resolution is late and best-effort. A table dropped and then dropped from the
//! database resolves to a line saying so rather than failing the turn: the user
//! asked a question, and "that table is gone" is a better answer than an error.

use red_core::{AiPolicy, AiTier};

use super::AiBackend;
use crate::protocol::ContextRefSpec;

/// How many references one turn may carry into the prompt. Past this the block
/// stops being context and starts being a schema dump in every message.
const MAX_REFERENCES: usize = 10;

/// The grounding block for `specs`, or `None` when there is nothing to say.
///
/// Ordered as the user dropped them, and capped: the note on the cap is there so
/// a silently-truncated list never reads as a complete one.
pub(crate) async fn resolve(
    backend: &AiBackend,
    specs: &[ContextRefSpec],
    policy: &AiPolicy,
) -> Option<String> {
    if specs.is_empty() || policy.tier == AiTier::Off {
        return None;
    }
    let mut out = String::from("Referenced by the user:\n\n");
    for spec in specs.iter().take(MAX_REFERENCES) {
        out.push_str(&resolve_one(backend, spec, policy).await);
        out.push('\n');
    }
    if specs.len() > MAX_REFERENCES {
        out.push_str(&format!(
            "({} more reference(s) were dropped to keep this message small; ask about them by \
             name.)\n",
            specs.len() - MAX_REFERENCES
        ));
    }
    Some(out)
}

async fn resolve_one(backend: &AiBackend, spec: &ContextRefSpec, policy: &AiPolicy) -> String {
    match spec {
        ContextRefSpec::Table { schema, name } => match table_detail(backend, schema, name).await {
            Ok(detail) => format!("# Table {}\n{detail}", qualified(schema, name)),
            Err(why) => format!("# Table {}\n{why}\n", qualified(schema, name)),
        },
        ContextRefSpec::Column {
            schema,
            table,
            name,
        } => match table_detail(backend, schema, table).await {
            // The column's own line out of the table's description, so the model
            // gets its type, nullability and keys rather than just a name.
            Ok(detail) => {
                let line = detail
                    .lines()
                    .find(|l| l.trim_start().starts_with(&format!("{name} ")))
                    .map(str::trim)
                    .unwrap_or("(no longer present in this table)");
                format!("# Column {}.{name}\n  {line}\n", qualified(schema, table))
            }
            Err(why) => format!("# Column {}.{name}\n{why}\n", qualified(schema, table)),
        },
        ContextRefSpec::Schema { name } => match schema_objects(backend, name).await {
            Ok(list) => format!("# Schema {name}\n{list}"),
            Err(why) => format!("# Schema {name}\n{why}\n"),
        },
        ContextRefSpec::Sql { label, sql } => {
            format!("# {label}\n```sql\n{}\n```\n", sql.trim())
        }
        // Row data is data: the tier that withholds `run_select` withholds this
        // too, or a `schema`-tier chat becomes a way to read rows by dragging.
        ContextRefSpec::Rows { label, .. }
            if !matches!(policy.tier, AiTier::Read | AiTier::Write) =>
        {
            format!("# {label}\n(row data is not available at this agent's access tier)\n")
        }
        ContextRefSpec::Rows { label, text } => format!("# {label}\n{}\n", text.trim_end()),
    }
}

/// `schema.name`, or just `name` when the engine has no namespaces.
fn qualified(schema: &str, name: &str) -> String {
    if schema.is_empty() {
        name.to_string()
    } else {
        format!("{schema}.{name}")
    }
}

/// A table's description, through the *same* formatter `describe_table` returns,
/// so the two can never drift apart.
async fn table_detail(backend: &AiBackend, schema: &str, table: &str) -> Result<String, String> {
    let AiBackend::Sql { driver, .. } = backend else {
        return Err("(tables are only available on a SQL connection)".into());
    };
    let detail = driver
        .describe_table(schema, table)
        .await
        .map_err(|e| format!("(could not be read: {e})"))?;
    // Not every engine errors on a table that is gone -- SQLite answers with an
    // empty description -- and a table rendered with no columns reads as a real
    // table that happens to have none. Say what actually happened instead.
    if detail.columns.is_empty() {
        return Err("(no longer exists, or is not readable by this connection)".into());
    }
    Ok(super::sql::format::format_table_detail(
        schema, table, &detail,
    ))
}

/// One namespace's objects, out of the same listing `list_schema` returns.
async fn schema_objects(backend: &AiBackend, name: &str) -> Result<String, String> {
    let AiBackend::Sql { driver, .. } = backend else {
        return Err("(schemas are only available on a SQL connection)".into());
    };
    let schemas = driver
        .list_objects()
        .await
        .map_err(|e| format!("(could not be read: {e})"))?;
    let wanted: Vec<red_core::SchemaMeta> = schemas
        .into_iter()
        .filter(|s| s.name.eq_ignore_ascii_case(name))
        .collect();
    if wanted.is_empty() {
        return Err("(no longer exists)".into());
    }
    Ok(super::sql::format::format_schema(&wanted))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(tier: AiTier) -> AiPolicy {
        AiPolicy {
            tier,
            ..AiPolicy::default()
        }
    }

    async fn sqlite(tag: &str) -> (std::path::PathBuf, AiBackend) {
        let db = std::env::temp_dir().join(format!("red-refs-{tag}-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&db);
        rusqlite::Connection::open(&db)
            .unwrap()
            .execute_batch("CREATE TABLE orders (id INTEGER PRIMARY KEY, total INTEGER NOT NULL);")
            .unwrap();
        let driver: std::sync::Arc<dyn red_driver::DatabaseDriver> =
            std::sync::Arc::new(red_driver::SqliteDriver::new(db.clone(), true));
        (
            db,
            AiBackend::Sql {
                driver,
                dialect: red_core::sql::Dialect::Sqlite,
            },
        )
    }

    /// The no-duplicate-formatter property: a dragged table reads exactly as
    /// `describe_table` would have answered. This is the one most likely to rot,
    /// because the obvious way to "improve" the block is to write a second
    /// renderer for it.
    #[tokio::test]
    async fn a_table_reference_reads_as_describe_table_would() {
        let (db, backend) = sqlite("table").await;
        let AiBackend::Sql { driver, .. } = &backend else {
            unreachable!()
        };
        let expected = super::super::sql::format::format_table_detail(
            "",
            "orders",
            &driver.describe_table("", "orders").await.unwrap(),
        );

        let block = resolve(
            &backend,
            &[ContextRefSpec::Table {
                schema: String::new(),
                name: "orders".into(),
            }],
            &policy(AiTier::Schema),
        )
        .await
        .unwrap();
        assert!(block.starts_with("Referenced by the user:\n\n# Table orders\n"));
        assert!(block.contains(&expected), "{block}");
        let _ = std::fs::remove_file(&db);
    }

    /// A table that is gone answers the question rather than failing the turn.
    #[tokio::test]
    async fn a_dropped_table_resolves_to_a_line_not_an_error() {
        let (db, backend) = sqlite("gone").await;
        let block = resolve(
            &backend,
            &[ContextRefSpec::Table {
                schema: String::new(),
                name: "ghosts".into(),
            }],
            &policy(AiTier::Read),
        )
        .await
        .unwrap();
        assert!(block.contains("# Table ghosts"), "{block}");
        assert!(block.contains("no longer exists"), "{block}");
        let _ = std::fs::remove_file(&db);
    }

    /// Row data is data. A `schema`-tier chat withholds `run_select`, so dragging
    /// a selection in must not be a way around it.
    #[tokio::test]
    async fn rows_are_refused_below_the_read_tier() {
        let (db, backend) = sqlite("rows").await;
        let rows = [ContextRefSpec::Rows {
            label: "Selected rows".into(),
            text: "id | total\n1 | 500".into(),
        }];
        let withheld = resolve(&backend, &rows, &policy(AiTier::Schema))
            .await
            .unwrap();
        assert!(!withheld.contains("500"), "{withheld}");
        assert!(withheld.contains("access tier"), "{withheld}");

        let allowed = resolve(&backend, &rows, &policy(AiTier::Read))
            .await
            .unwrap();
        assert!(allowed.contains("500"), "{allowed}");
        let _ = std::fs::remove_file(&db);
    }

    /// A capped list says it was capped: a silently-truncated reference block
    /// reads as a complete one.
    #[tokio::test]
    async fn too_many_references_are_capped_with_a_note() {
        let (db, backend) = sqlite("cap").await;
        let specs: Vec<ContextRefSpec> = (0..MAX_REFERENCES + 3)
            .map(|i| ContextRefSpec::Sql {
                label: format!("Tab {i}"),
                sql: "SELECT 1".into(),
            })
            .collect();
        let block = resolve(&backend, &specs, &policy(AiTier::Read))
            .await
            .unwrap();
        assert!(block.contains("# Tab 9"), "the cap is inclusive: {block}");
        assert!(!block.contains("# Tab 10"), "{block}");
        assert!(
            block.contains("3 more reference(s) were dropped"),
            "{block}"
        );
        let _ = std::fs::remove_file(&db);
    }

    /// Nothing pointed at, nothing said — and an `off`-tier chat gets no
    /// grounding at all, references included.
    #[tokio::test]
    async fn nothing_to_resolve_produces_no_block() {
        let (db, backend) = sqlite("empty").await;
        assert!(
            resolve(&backend, &[], &policy(AiTier::Read))
                .await
                .is_none()
        );
        let specs = [ContextRefSpec::Sql {
            label: "Tab".into(),
            sql: "SELECT 1".into(),
        }];
        assert!(
            resolve(&backend, &specs, &policy(AiTier::Off))
                .await
                .is_none()
        );
        let _ = std::fs::remove_file(&db);
    }
}
