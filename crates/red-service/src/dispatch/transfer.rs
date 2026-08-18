//! The transfer executor: one [`TransferPlan`] in, one [`TransferSummary`] out.
//!
//! The general form of `copy_job` (one result into one table) and `migrate_job`
//! (a whole schema into a fresh database). The skeleton is `migrate_job`'s -
//! order the items FK-parents-first, create and stream each, then a deferred
//! index pass and a deferred foreign-key pass - with three differences that are
//! the whole point of the plan model:
//!
//! - the per-item loop reads the item's `action` and `content` instead of
//!   assuming create-and-load, so "this one empty, that one filtered, that one
//!   appended into" is expressible;
//! - a table already present on the target is a *planning* input (the wizard
//!   showed `Existing`), not a silent skip inside the loop;
//! - every item lands in the summary, including the ones that were skipped or
//!   failed, so nothing is swallowed.
//!
//! Memory is bounded exactly as before: one window resident per item, committed
//! per chunk, so cancelling or failing mid-job leaves a meaningful count.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use red_core::ddl;
use red_core::transfer::{
    ItemAction, ItemContent, ItemOutcome, ItemReport, ItemSource, TransferItem, TransferPlan,
    TransferSummary,
};
use red_core::{Column, ColumnMap, ColumnMeta, CopyMode, DbKind, FkEdge, TableDetail, TableRef};
use red_driver::{AbortSignal, DatabaseDriver};

use crate::Event;

use super::jobs::{copy_read_opts, order_by_fk, stream_cursor_into};
use super::*;

/// Everything the executor needs that isn't in the plan: the two drivers, their
/// engines (a cross-engine create must not carry the source's `DEFAULT` text),
/// and the already-open results a `ItemSource::Result` item names.
pub(crate) struct TransferJob {
    pub(crate) src: Arc<dyn DatabaseDriver>,
    pub(crate) dst: Arc<dyn DatabaseDriver>,
    pub(crate) src_kind: DbKind,
    pub(crate) dst_kind: DbKind,
    pub(crate) plan: TransferPlan,
    /// Open results' already-wrapped (filtered/sorted) SQL, keyed by raw epoch.
    /// Snapshotted by the dispatch arm: the result map is session state and the
    /// job runs off-loop.
    pub(crate) result_sql: HashMap<u64, String>,
    pub(crate) cancel: Arc<AtomicBool>,
    pub(crate) events: Events,
    pub(crate) id: OpId,
}

/// How a transfer ended. Every arm carries the summary built so far, because a
/// job that failed on item 12 of 40 still moved eleven tables and the user needs
/// to know which.
pub(crate) enum TransferOutcome {
    Finished(TransferSummary),
    Failed {
        item: Option<usize>,
        message: String,
        summary: TransferSummary,
    },
    Cancelled(TransferSummary),
    /// A dry run: the rendered script and per-item row estimates. Nothing written.
    Planned {
        script: String,
        estimates: Vec<(String, Option<u64>)>,
    },
}

/// What one item resolved to before any write: where its rows come from, what
/// shape the target should have, and the source detail the deferred passes need.
struct Resolved {
    /// `SELECT` for the rows, or `None` when the item moves none.
    select_sql: Option<String>,
    /// The column shape a `Create`/`Recreate` builds the target from.
    create_columns: Vec<ColumnMeta>,
    /// The source table's full detail, when the source *is* a table: the
    /// deferred index and foreign-key passes read it. `None` for a result or
    /// free-SQL source, which have no indexes to recreate.
    detail: Option<TableDetail>,
    /// Notes worth showing next to the item's outcome (a stripped default, a
    /// column the mapping could not place).
    warnings: Vec<String>,
}

impl TransferJob {
    /// The target reference for one item, under the plan's target namespace.
    fn target_ref(&self, item: &TransferItem) -> TableRef {
        TableRef {
            schema: self.plan.target_namespace.clone(),
            name: item.target_name.clone(),
        }
    }

    /// The source reference for a table item: its own schema when it names one,
    /// the plan's source namespace otherwise.
    fn source_ref(&self, item: &TransferItem) -> Option<TableRef> {
        match &item.source {
            ItemSource::Table { schema, name } => Some(TableRef {
                schema: schema
                    .clone()
                    .or_else(|| self.plan.source_namespace.clone()),
                name: name.clone(),
            }),
            _ => None,
        }
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

/// Run one plan. Emits `TransferProgress` / `TransferItemDone` as it goes; the
/// caller turns the returned outcome into the terminal event.
pub(crate) async fn transfer_job(job: TransferJob) -> TransferOutcome {
    // The FK graph, read once: it orders the items and feeds the deferred FK
    // pass. Best-effort, like the migrate job it generalises - a failure just
    // falls back to the plan's own order.
    let fks = job.src.foreign_keys().await.unwrap_or_default();

    if job.plan.options.dry_run {
        let (script, estimates) = render_script(&job, &fks).await;
        return TransferOutcome::Planned { script, estimates };
    }

    let order = order_items(&job.plan, &fks);
    let items = job.plan.items.len();
    let mut summary = TransferSummary::default();
    // Items whose target was created from a source table, in execution order:
    // the deferred index and FK passes walk exactly these.
    let mut created: Vec<(usize, TableDetail)> = Vec::new();

    for (position, &index) in order.iter().enumerate() {
        let item = &job.plan.items[index];
        if job.cancelled() {
            return TransferOutcome::Cancelled(summary);
        }
        if !item.is_active() {
            push_report(
                &job,
                &mut summary,
                index,
                ItemReport {
                    table: item.target_name.clone(),
                    outcome: ItemOutcome::Skipped {
                        reason: "not selected".into(),
                    },
                    rows: 0,
                    warnings: Vec::new(),
                },
            );
            continue;
        }

        let base = summary.rows;
        let outcome = run_item(&job, item, position, items, base).await;
        match outcome {
            Ok((report, detail)) => {
                summary.rows += report.rows;
                if let Some(detail) = detail {
                    created.push((index, detail));
                }
                push_report(&job, &mut summary, index, report);
            }
            Err(ItemFailure::Cancelled { rows }) => {
                summary.rows += rows;
                push_report(
                    &job,
                    &mut summary,
                    index,
                    ItemReport {
                        table: item.target_name.clone(),
                        outcome: ItemOutcome::Skipped {
                            reason: "cancelled".into(),
                        },
                        rows,
                        warnings: Vec::new(),
                    },
                );
                return TransferOutcome::Cancelled(summary);
            }
            Err(ItemFailure::Error { rows, message }) => {
                summary.rows += rows;
                push_report(
                    &job,
                    &mut summary,
                    index,
                    ItemReport {
                        table: item.target_name.clone(),
                        outcome: ItemOutcome::Failed {
                            message: message.clone(),
                        },
                        rows,
                        warnings: Vec::new(),
                    },
                );
                if job.plan.options.on_error == red_core::transfer::OnError::Stop {
                    return TransferOutcome::Failed {
                        item: Some(index),
                        message,
                        summary,
                    };
                }
            }
        }
    }

    // Deferred index + foreign-key passes, after every item's data has landed so
    // dependency order cannot block them. Best-effort by design: a refused index
    // is decoration the data does not depend on, so it becomes a warning on the
    // item rather than a failed job.
    if job.plan.options.indexes {
        deferred_indexes(&job, &created, &mut summary).await;
    }
    if job.plan.options.foreign_keys {
        deferred_foreign_keys(&job, &created, &fks, &mut summary).await;
    }
    if job.cancelled() {
        return TransferOutcome::Cancelled(summary);
    }
    TransferOutcome::Finished(summary)
}

/// Why one item stopped short. `Cancelled` is separate from `Error` because a
/// cancel ends the whole job while an error may not (`OnError::SkipItem`).
enum ItemFailure {
    Cancelled { rows: u64 },
    Error { rows: u64, message: String },
}

/// Run one active item end to end: resolve its shape, prepare the target
/// (create / drop+create / clear), then stream its rows. Returns the item's
/// report plus, when the target was created from a source *table*, that table's
/// detail for the deferred passes.
async fn run_item(
    job: &TransferJob,
    item: &TransferItem,
    position: usize,
    items: usize,
    base: u64,
) -> Result<(ItemReport, Option<TableDetail>), ItemFailure> {
    let target = job.target_ref(item);
    let label = item.target_name.clone();

    let (resolved, cursor) = match resolve(job, item).await {
        Ok(r) => r,
        Err(ResolveOutcome::Skip(reason)) => {
            return Ok((
                ItemReport {
                    table: label,
                    outcome: ItemOutcome::Skipped { reason },
                    rows: 0,
                    warnings: Vec::new(),
                },
                None,
            ));
        }
        Err(ResolveOutcome::Failed(message)) => {
            return Err(ItemFailure::Error { rows: 0, message });
        }
    };
    let mut warnings = resolved.warnings;

    // Prepare the target. A recreate drops first; a create is `IF NOT EXISTS` so
    // it is a no-op against a table that is already there. Both run before the
    // truncate, so a truncate+insert into a freshly-created table can't fail on a
    // missing table (the same ordering `copy_job` holds).
    if matches!(item.action, ItemAction::Recreate)
        && let Err(e) = job.dst.drop_table(&target).await
    {
        return Err(ItemFailure::Error {
            rows: 0,
            message: e.to_string(),
        });
    }
    if matches!(item.action, ItemAction::Create | ItemAction::Recreate)
        && let Err(e) = job
            .dst
            .create_table(&target, &resolved.create_columns)
            .await
    {
        return Err(ItemFailure::Error {
            rows: 0,
            message: e.to_string(),
        });
    }
    if matches!(
        item.action,
        ItemAction::Existing {
            mode: CopyMode::TruncateInsert
        }
    ) && let Err(e) = job.dst.clear_table(&target).await
    {
        return Err(ItemFailure::Error {
            rows: 0,
            message: e.to_string(),
        });
    }

    // The projection onto the target's columns. An explicit mapping wins; for an
    // existing target the columns are matched by name; otherwise it is identity.
    let source_columns: Vec<Column> = resolved
        .create_columns
        .iter()
        .map(|c| Column {
            name: c.name.clone(),
            decl_type: c.type_name.clone(),
        })
        .collect();
    let mapping = match resolve_mapping(job, item, &target, &source_columns, &mut warnings).await {
        Ok(m) => m,
        Err(message) => return Err(ItemFailure::Error { rows: 0, message }),
    };

    let mut rows = 0u64;
    if item.content.moves_rows() {
        if mapping.is_empty() {
            return Err(ItemFailure::Error {
                rows: 0,
                message: format!("no source columns map onto “{}”", item.target_name),
            });
        }
        let target_columns: Vec<Column> = mapping
            .iter()
            .map(|m| Column {
                name: m.column.clone(),
                decl_type: m.decl_type.clone(),
            })
            .collect();
        let events = job.events.clone();
        let id = job.id;
        let table = label.clone();
        let mut on_progress = move |total: u64| {
            emit(
                &events,
                None,
                Event::TransferProgress {
                    id,
                    item: position,
                    items,
                    table: table.clone(),
                    item_rows: total - base,
                    rows: total,
                },
            );
        };
        // A table source opens its cursor here; a result / free-SQL source opened
        // one during `resolve` (that is how its column shape was learned) and
        // hands it straight to the streamer rather than re-running the query.
        let (delta, err) = match cursor {
            Some(cursor) => {
                stream_cursor_into(
                    cursor,
                    &job.dst,
                    &target,
                    &mapping,
                    &target_columns,
                    &job.cancel,
                    base,
                    &mut on_progress,
                )
                .await
            }
            None => {
                let Some(sql) = resolved.select_sql.as_deref() else {
                    return Err(ItemFailure::Error {
                        rows: 0,
                        message: format!("no rows to read for “{}”", item.target_name),
                    });
                };
                match job.src.open_cursor(sql, copy_read_opts()).await {
                    Ok(cursor) => {
                        stream_cursor_into(
                            cursor,
                            &job.dst,
                            &target,
                            &mapping,
                            &target_columns,
                            &job.cancel,
                            base,
                            &mut on_progress,
                        )
                        .await
                    }
                    Err(e) => (0, Some(e)),
                }
            }
        };
        rows = delta;
        match err {
            None => {}
            Some(red_core::RedError::Interrupted) => {
                return Err(ItemFailure::Cancelled { rows });
            }
            Some(e) => {
                return Err(ItemFailure::Error {
                    rows,
                    message: e.to_string(),
                });
            }
        }
    }

    let outcome = match (item.action, item.content.moves_rows()) {
        (ItemAction::Create, true) => ItemOutcome::Created,
        (ItemAction::Create, false) => ItemOutcome::CreatedEmpty,
        (ItemAction::Recreate, _) => ItemOutcome::Recreated,
        (
            ItemAction::Existing {
                mode: CopyMode::Append,
            },
            _,
        ) => ItemOutcome::Appended,
        (
            ItemAction::Existing {
                mode: CopyMode::TruncateInsert,
            },
            _,
        ) => ItemOutcome::Replaced,
        // Guarded by `is_active` at the call site.
        (ItemAction::Skip, _) => ItemOutcome::Skipped {
            reason: "not selected".into(),
        },
    };
    // Only a *created* target gets the deferred passes: an existing table already
    // has whatever indexes and constraints its owner chose, and re-adding the
    // source's would be RED editing a schema the user didn't ask it to touch.
    let created_detail = matches!(item.action, ItemAction::Create | ItemAction::Recreate)
        .then_some(resolved.detail)
        .flatten();
    Ok((
        ItemReport {
            table: label,
            outcome,
            rows,
            warnings,
        },
        created_detail,
    ))
}

/// Why `resolve` did not produce a shape.
enum ResolveOutcome {
    /// Nothing to do, and that is not an error (a view with no columns).
    Skip(String),
    Failed(String),
}

/// Work out one item's source SQL and column shape.
///
/// A table source is described through the catalog, which is where primary keys,
/// defaults and auto-increment come from. A result or free-SQL source has no
/// catalog entry, so its cursor is opened here to read the shape off the result
/// itself - and handed back so the streamer drains that same cursor.
#[allow(clippy::type_complexity)]
async fn resolve(
    job: &TransferJob,
    item: &TransferItem,
) -> Result<(Resolved, Option<Box<dyn red_driver::QueryCursor>>), ResolveOutcome> {
    let mut warnings = Vec::new();
    match &item.source {
        ItemSource::Table { .. } => {
            let Some(source) = job.source_ref(item) else {
                return Err(ResolveOutcome::Failed("no source table".into()));
            };
            let detail = job
                .src
                .describe_table(source.schema.as_deref().unwrap_or(""), &source.name)
                .await
                .map_err(|e| ResolveOutcome::Failed(e.to_string()))?;
            if detail.columns.is_empty() {
                return Err(ResolveOutcome::Skip(
                    "the source has no columns to create from".into(),
                ));
            }
            let create_columns = shape_columns(job, &detail.columns, &mut warnings);
            let select_sql = item.content.select_sql(&job.src.quote_table(&source));
            Ok((
                Resolved {
                    select_sql,
                    create_columns,
                    detail: Some(detail),
                    warnings,
                },
                None,
            ))
        }
        ItemSource::Result { epoch } => {
            let Some(sql) = job.result_sql.get(epoch).cloned() else {
                return Err(ResolveOutcome::Failed(
                    "the source result is no longer open".into(),
                ));
            };
            open_query_source(job, item, sql, warnings).await
        }
        ItemSource::Sql(sql) => {
            let sql = sql.trim().to_string();
            if sql.is_empty() {
                return Err(ResolveOutcome::Failed("the source query is empty".into()));
            }
            open_query_source(job, item, sql, warnings).await
        }
    }
}

/// Resolve a result / free-SQL source: open its cursor, take the column shape
/// from it, and pass the cursor on so the rows are read exactly once.
#[allow(clippy::type_complexity)]
async fn open_query_source(
    job: &TransferJob,
    item: &TransferItem,
    sql: String,
    mut warnings: Vec<String>,
) -> Result<(Resolved, Option<Box<dyn red_driver::QueryCursor>>), ResolveOutcome> {
    // A filter or a row cap on a query source wraps the query rather than
    // rebuilding it, so an already-filtered result stays filtered.
    let sql = match &item.content {
        ItemContent::Where(expr) if !expr.trim().is_empty() => {
            format!("SELECT * FROM ({sql}) AS _red_src WHERE ({expr})")
        }
        ItemContent::Limit(n) if *n > 0 => format!("SELECT * FROM ({sql}) AS _red_src LIMIT {n}"),
        _ => sql,
    };
    let cursor = job
        .src
        .open_cursor(&sql, copy_read_opts())
        .await
        .map_err(|e| ResolveOutcome::Failed(e.to_string()))?;
    let columns: Vec<ColumnMeta> = cursor
        .columns()
        .iter()
        .map(|c| ColumnMeta {
            name: c.name.clone(),
            type_name: c.decl_type.clone(),
            // A result carries no schema facts: its columns are plain and
            // nullable, and the target's primary key (if any) is the target's
            // business.
            not_null: false,
            primary_key: false,
            default: None,
            auto_increment: false,
        })
        .collect();
    if columns.is_empty() {
        return Err(ResolveOutcome::Skip("the source has no columns".into()));
    }
    let create_columns = shape_columns(job, &columns, &mut warnings);
    Ok((
        Resolved {
            select_sql: Some(sql),
            create_columns,
            detail: None,
            warnings,
        },
        Some(cursor),
    ))
}

/// Apply the plan's shape options to a source column list, and strip the one
/// piece of it that does not travel between engines.
fn shape_columns(
    job: &TransferJob,
    columns: &[ColumnMeta],
    warnings: &mut Vec<String>,
) -> Vec<ColumnMeta> {
    let carried = if job.src_kind == job.dst_kind {
        columns.to_vec()
    } else {
        let dropped: Vec<&str> = columns
            .iter()
            .filter(|c| c.default.is_some() && !c.auto_increment)
            .map(|c| c.name.as_str())
            .collect();
        if !dropped.is_empty() {
            warnings.push(format!(
                "column defaults dropped (source and target are different engines): {}",
                dropped.join(", ")
            ));
        }
        ddl::strip_defaults(columns)
    };
    ddl::apply_shape_options(&carried, job.plan.options.primary_keys)
}

/// The projection onto the target's columns.
///
/// An explicit mapping from the plan wins. Otherwise: an existing target is
/// described and matched by name (a column that has no counterpart on either
/// side is a warning, not a failure - the target's own defaults fill it), and a
/// created target takes the identity mapping.
async fn resolve_mapping(
    job: &TransferJob,
    item: &TransferItem,
    target: &TableRef,
    source_columns: &[Column],
    warnings: &mut Vec<String>,
) -> Result<Vec<ColumnMap>, String> {
    if !item.mapping.is_empty() {
        return Ok(item.mapping.clone());
    }
    if !matches!(item.action, ItemAction::Existing { .. }) {
        return Ok(source_columns
            .iter()
            .enumerate()
            .map(|(i, c)| ColumnMap {
                source: i,
                column: c.name.clone(),
                decl_type: c.decl_type.clone(),
            })
            .collect());
    }
    let detail = job
        .dst
        .describe_table(target.schema.as_deref().unwrap_or(""), &target.name)
        .await
        .map_err(|e| e.to_string())?;
    let mut mapping = Vec::new();
    let mut unmatched = Vec::new();
    for tcol in &detail.columns {
        match source_columns
            .iter()
            .position(|s| s.name.eq_ignore_ascii_case(&tcol.name))
        {
            Some(idx) => mapping.push(ColumnMap {
                source: idx,
                column: tcol.name.clone(),
                decl_type: tcol.type_name.clone(),
            }),
            None => unmatched.push(tcol.name.clone()),
        }
    }
    if !unmatched.is_empty() {
        warnings.push(format!(
            "target columns left to their default: {}",
            unmatched.join(", ")
        ));
    }
    let ignored: Vec<&str> = source_columns
        .iter()
        .enumerate()
        .filter(|(i, _)| !mapping.iter().any(|m| m.source == *i))
        .map(|(_, c)| c.name.as_str())
        .collect();
    if !ignored.is_empty() {
        warnings.push(format!("source columns ignored: {}", ignored.join(", ")));
    }
    Ok(mapping)
}

/// Record one item's report and tell the UI about it in the same breath, so the
/// progress list fills in as the job runs rather than all at once at the end.
fn push_report(job: &TransferJob, summary: &mut TransferSummary, index: usize, report: ItemReport) {
    emit(
        &job.events,
        None,
        Event::TransferItemDone {
            id: job.id,
            item: index,
            report: report.clone(),
        },
    );
    summary.items.push(report);
}

/// Recreate each created table's secondary indexes, skipping the one that backs
/// the primary key (already created with the table).
async fn deferred_indexes(
    job: &TransferJob,
    created: &[(usize, TableDetail)],
    summary: &mut TransferSummary,
) {
    for (index, detail) in created {
        if job.cancelled() {
            return;
        }
        let item = &job.plan.items[*index];
        let target = job.target_ref(item);
        let pk: Vec<String> = detail
            .columns
            .iter()
            .filter(|c| c.primary_key)
            .map(|c| c.name.to_ascii_lowercase())
            .collect();
        let mut renames: Vec<String> = Vec::new();
        for idx in &detail.indexes {
            if idx.columns.is_empty() || backs_primary_key(idx, &pk) {
                continue;
            }
            let name = index_name_for(job, item, &idx.name);
            if name != idx.name {
                renames.push(format!("{} -> {name}", idx.name));
            }
            if let Err(e) = job
                .dst
                .create_index(&target, &name, idx.unique, &idx.columns)
                .await
            {
                note(summary, &item.target_name, format!("index {name}: {e}"));
            }
        }
        if !renames.is_empty() {
            note(
                summary,
                &item.target_name,
                format!(
                    "indexes renamed to stay unique in this namespace: {}",
                    renames.join(", ")
                ),
            );
        }
    }
}

/// The name to create one of `item`'s indexes under on the target.
///
/// Index names are *namespace*-scoped on Postgres and *database*-scoped on
/// SQLite, so duplicating a table inside its own namespace under a new name
/// would ask for an index name the source already owns - and `IF NOT EXISTS`
/// would swallow that as a no-op, leaving a copy with no indexes and no
/// complaint. When (and only when) the target lands in the source's namespace
/// under a different name, the index is renamed the same way the table was: the
/// source table's name is swapped for the target's where it appears, and
/// prefixed otherwise. MySQL scopes index names per table and never needs this,
/// but the rename is harmless there.
fn index_name_for(job: &TransferJob, item: &TransferItem, index: &str) -> String {
    let ItemSource::Table { schema, name } = &item.source else {
        return index.to_string();
    };
    let source_ns = schema.as_ref().or(job.plan.source_namespace.as_ref());
    let same_namespace = match (source_ns, job.plan.target_namespace.as_ref()) {
        (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
        (None, None) => true,
        _ => false,
    };
    if !same_namespace || name.eq_ignore_ascii_case(&item.target_name) {
        return index.to_string();
    }
    rename_index(index, name, &item.target_name)
}

/// Swap `source_table` for `target_table` inside an index name, or prefix the
/// target name when the index name doesn't mention its table.
fn rename_index(index: &str, source_table: &str, target_table: &str) -> String {
    let lower = index.to_ascii_lowercase();
    match lower.find(&source_table.to_ascii_lowercase()) {
        Some(pos) => {
            let mut out = String::with_capacity(index.len() + target_table.len());
            out.push_str(&index[..pos]);
            out.push_str(target_table);
            out.push_str(&index[pos + source_table.len()..]);
            out
        }
        None => format!("{target_table}_{index}"),
    }
}

/// Whether an index is the engine's own primary-key index, which the `CREATE
/// TABLE` already made. Matched by column set *or* by the per-engine naming
/// convention, because not every engine reports the same one.
fn backs_primary_key(idx: &red_core::IndexMeta, pk: &[String]) -> bool {
    let cols: Vec<String> = idx.columns.iter().map(|c| c.to_ascii_lowercase()).collect();
    let same_columns =
        !pk.is_empty() && cols.len() == pk.len() && pk.iter().all(|c| cols.contains(c));
    let name = idx.name.to_ascii_lowercase();
    same_columns
        || name == "primary"
        || name.starts_with("sqlite_autoindex")
        || name.ends_with("_pkey")
}

/// Recreate the foreign keys whose *both* endpoints were created by this job,
/// re-pointed at the items' target names so a renamed table still references the
/// right one.
async fn deferred_foreign_keys(
    job: &TransferJob,
    created: &[(usize, TableDetail)],
    fks: &[FkEdge],
    summary: &mut TransferSummary,
) {
    // Source table name (lowercased) → the target name it landed under.
    let renamed: HashMap<String, String> = created
        .iter()
        .filter_map(|(index, _)| {
            let item = &job.plan.items[*index];
            match &item.source {
                ItemSource::Table { name, .. } => {
                    Some((name.to_ascii_lowercase(), item.target_name.clone()))
                }
                _ => None,
            }
        })
        .collect();
    let in_scope = |s: &Option<String>| {
        job.plan
            .source_namespace
            .as_deref()
            .is_none_or(|sc| s.as_deref().is_none_or(|x| x.eq_ignore_ascii_case(sc)))
    };
    for fk in fks {
        if job.cancelled() {
            return;
        }
        let (Some(child_name), Some(parent_name)) = (
            renamed.get(&fk.from_table.to_ascii_lowercase()),
            renamed.get(&fk.to_table.to_ascii_lowercase()),
        ) else {
            continue;
        };
        if !in_scope(&fk.from_schema) || !in_scope(&fk.to_schema) {
            continue;
        }
        let child = TableRef {
            schema: job.plan.target_namespace.clone(),
            name: child_name.clone(),
        };
        let parent = TableRef {
            schema: job.plan.target_namespace.clone(),
            name: parent_name.clone(),
        };
        let cols: Vec<String> = fk.columns.iter().map(|(f, _)| f.clone()).collect();
        let refs: Vec<String> = fk.columns.iter().map(|(_, t)| t.clone()).collect();
        if let Err(e) = job.dst.add_foreign_key(&child, &cols, &parent, &refs).await {
            note(
                summary,
                child_name,
                format!("foreign key to {parent_name}: {e}"),
            );
        }
    }
}

/// Attach a non-fatal note to an already-reported item.
fn note(summary: &mut TransferSummary, table: &str, warning: String) {
    tracing::warn!(table = %table, warning = %warning, "transfer: deferred step skipped");
    if let Some(report) = summary.items.iter_mut().find(|r| r.table == table) {
        report.warnings.push(warning);
    }
}

/// Order the plan's items so a foreign-key parent is transferred before its
/// child. Table items are sequenced by [`order_by_fk`]; result and free-SQL
/// items keep their plan order and go last, since they can only depend on tables
/// and never the other way around.
fn order_items(plan: &TransferPlan, fks: &[FkEdge]) -> Vec<usize> {
    let mut table_indices: Vec<usize> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    let mut rest: Vec<usize> = Vec::new();
    for (i, item) in plan.items.iter().enumerate() {
        match &item.source {
            ItemSource::Table { name, .. } => {
                table_indices.push(i);
                names.push(name.clone());
            }
            _ => rest.push(i),
        }
    }
    let ordered = order_by_fk(&names, plan.source_namespace.as_deref(), fks);
    let mut used = vec![false; table_indices.len()];
    let mut out: Vec<usize> = Vec::with_capacity(plan.items.len());
    for name in &ordered {
        // The same table can appear twice (two items, two target names), so take
        // the first unused slot rather than the first match.
        if let Some(slot) =
            (0..table_indices.len()).find(|&s| !used[s] && names[s].eq_ignore_ascii_case(name))
        {
            used[slot] = true;
            out.push(table_indices[slot]);
        }
    }
    // Defensive: anything `order_by_fk` did not emit (it never drops a name, but
    // a future change must not silently lose an item).
    for (slot, &index) in table_indices.iter().enumerate() {
        if !used[slot] {
            out.push(index);
        }
    }
    out.extend(rest);
    out
}

/// Render the plan as a script and estimate each item's rows, writing nothing.
///
/// The script is what the job *would* run, in execution order, built from the
/// same `red_core::ddl` builders the drivers use, so it cannot drift from the
/// real statements. Identifiers are quoted by the target driver's own quoter.
/// The estimate is one pushed-down `count(*)` per item, which is the one place a
/// transfer counts rows - and only because the user explicitly asked for a
/// preview instead of a run.
async fn render_script(job: &TransferJob, fks: &[FkEdge]) -> (String, Vec<(String, Option<u64>)>) {
    let quote = |id: &str| job.dst.quote_ident(id);
    let order = order_items(&job.plan, fks);
    let abort = AbortSignal::new();
    let mut script = String::new();
    let mut estimates: Vec<(String, Option<u64>)> = Vec::new();
    let mut created: Vec<(usize, TableDetail)> = Vec::new();

    script.push_str("-- Transfer dry run. Nothing was written.\n");
    if let Some(ns) = &job.plan.target_namespace {
        script.push_str(&format!("-- Target namespace: {ns}\n"));
    }

    for &index in &order {
        let item = &job.plan.items[index];
        let target = job.target_ref(item);
        if !item.is_active() {
            script.push_str(&format!("\n-- {} - skipped\n", item.target_name));
            estimates.push((item.target_name.clone(), None));
            continue;
        }
        let resolved = match resolve(job, item).await {
            Ok((resolved, _cursor)) => resolved,
            Err(ResolveOutcome::Skip(reason)) | Err(ResolveOutcome::Failed(reason)) => {
                script.push_str(&format!("\n-- {} - {reason}\n", item.target_name));
                estimates.push((item.target_name.clone(), None));
                continue;
            }
        };
        script.push_str(&format!(
            "\n-- {} ({}, {})\n",
            item.target_name,
            item.action.verb(),
            item.content.label()
        ));
        if matches!(item.action, ItemAction::Recreate) {
            script.push_str(&ddl::drop_table_sql(&target, quote));
            script.push_str(";\n");
        }
        if matches!(item.action, ItemAction::Create | ItemAction::Recreate) {
            script.push_str(&ddl::create_table_sql(
                &target,
                &resolved.create_columns,
                job.dst_kind,
                quote,
            ));
            script.push_str(";\n");
            if let Some(detail) = &resolved.detail {
                created.push((index, detail.clone()));
            }
        }
        if matches!(
            item.action,
            ItemAction::Existing {
                mode: CopyMode::TruncateInsert
            }
        ) {
            script.push_str(&format!(
                "DELETE FROM {};\n",
                ddl::qualify_table(&target, quote)
            ));
        }
        match resolved.select_sql.as_deref() {
            Some(sql) => {
                let cols = resolved
                    .create_columns
                    .iter()
                    .map(|c| quote(&c.name))
                    .collect::<Vec<_>>()
                    .join(", ");
                script.push_str(&format!(
                    "-- rows read with: {sql}\nINSERT INTO {} ({cols}) VALUES (...);\n",
                    ddl::qualify_table(&target, quote)
                ));
                // `count` is pushed down as a subquery over the same SQL, so the
                // estimate matches what the run would move (filter included).
                let estimate = job
                    .src
                    .count(sql, &abort)
                    .await
                    .ok()
                    .map(|n| n.max(0) as u64);
                estimates.push((item.target_name.clone(), estimate));
            }
            None => {
                script.push_str("-- structure only, no rows\n");
                estimates.push((item.target_name.clone(), Some(0)));
            }
        }
    }

    if job.plan.options.indexes {
        let mut header = false;
        for (index, detail) in &created {
            let item = &job.plan.items[*index];
            let target = job.target_ref(item);
            let pk: Vec<String> = detail
                .columns
                .iter()
                .filter(|c| c.primary_key)
                .map(|c| c.name.to_ascii_lowercase())
                .collect();
            for idx in &detail.indexes {
                if idx.columns.is_empty() || backs_primary_key(idx, &pk) {
                    continue;
                }
                if !header {
                    script.push_str("\n-- Deferred index pass\n");
                    header = true;
                }
                script.push_str(&ddl::create_index_sql(
                    &target,
                    &index_name_for(job, item, &idx.name),
                    idx.unique,
                    &idx.columns,
                    job.dst_kind,
                    quote,
                ));
                script.push_str(";\n");
            }
        }
    }

    if job.plan.options.foreign_keys {
        let renamed: HashMap<String, String> = created
            .iter()
            .filter_map(|(index, _)| match &job.plan.items[*index].source {
                ItemSource::Table { name, .. } => Some((
                    name.to_ascii_lowercase(),
                    job.plan.items[*index].target_name.clone(),
                )),
                _ => None,
            })
            .collect();
        let mut header = false;
        for fk in fks {
            let (Some(child_name), Some(parent_name)) = (
                renamed.get(&fk.from_table.to_ascii_lowercase()),
                renamed.get(&fk.to_table.to_ascii_lowercase()),
            ) else {
                continue;
            };
            if !header {
                script.push_str("\n-- Deferred foreign-key pass\n");
                header = true;
            }
            let child = TableRef {
                schema: job.plan.target_namespace.clone(),
                name: child_name.clone(),
            };
            let parent = TableRef {
                schema: job.plan.target_namespace.clone(),
                name: parent_name.clone(),
            };
            let cols: Vec<String> = fk.columns.iter().map(|(f, _)| f.clone()).collect();
            let refs: Vec<String> = fk.columns.iter().map(|(_, t)| t.clone()).collect();
            script.push_str(&ddl::add_fk_sql(&child, &cols, &parent, &refs, quote));
            script.push_str(";\n");
        }
    }

    (script, estimates)
}

#[cfg(test)]
mod tests {
    use super::*;
    use red_core::transfer::{TransferItem, TransferOptions};

    fn plan(items: Vec<TransferItem>) -> TransferPlan {
        TransferPlan {
            source_namespace: None,
            target_namespace: None,
            items,
            options: TransferOptions::default(),
        }
    }

    fn fk(from: &str, to: &str) -> FkEdge {
        FkEdge {
            from_schema: None,
            from_table: from.into(),
            to_schema: None,
            to_table: to.into(),
            columns: vec![],
        }
    }

    #[test]
    fn items_run_fk_parents_first() {
        let p = plan(vec![
            TransferItem::table("child"),
            TransferItem::table("parent"),
        ]);
        assert_eq!(order_items(&p, &[fk("child", "parent")]), vec![1, 0]);
    }

    #[test]
    fn query_sources_run_after_every_table() {
        let mut sql_item = TransferItem::table("report");
        sql_item.source = ItemSource::Sql("SELECT 1".into());
        let p = plan(vec![sql_item, TransferItem::table("users")]);
        assert_eq!(order_items(&p, &[]), vec![1, 0]);
    }

    #[test]
    fn every_item_is_emitted_exactly_once_even_when_two_share_a_source() {
        // "Duplicate this table twice under different names" is legal, and the
        // order must not drop or double either one.
        let mut a = TransferItem::table("users");
        a.target_name = "users_a".into();
        let mut b = TransferItem::table("users");
        b.target_name = "users_b".into();
        let p = plan(vec![a, b]);
        let mut order = order_items(&p, &[]);
        order.sort_unstable();
        assert_eq!(order, vec![0, 1]);
    }

    #[test]
    fn the_primary_key_index_is_not_recreated() {
        let idx = red_core::IndexMeta {
            name: "ix_custom".into(),
            unique: true,
            columns: vec!["Id".into()],
        };
        assert!(backs_primary_key(&idx, &["id".to_string()]));
        assert!(!backs_primary_key(&idx, &["other".to_string()]));
    }

    #[test]
    fn a_renamed_table_takes_its_indexes_with_it() {
        // The name must change, or `CREATE INDEX IF NOT EXISTS` silently no-ops
        // against the source's own index and the copy ends up unindexed.
        assert_eq!(
            rename_index("ix_child_n", "child", "child_copy"),
            "ix_child_copy_n"
        );
        // An index whose name doesn't mention its table still has to be unique.
        assert_eq!(
            rename_index("by_date", "child", "child_copy"),
            "child_copy_by_date"
        );
    }

    #[test]
    fn a_pk_named_index_is_recognised_without_a_column_match() {
        let idx = red_core::IndexMeta {
            name: "users_pkey".into(),
            unique: true,
            columns: vec!["id".into()],
        };
        assert!(backs_primary_key(&idx, &[]));
    }
}
