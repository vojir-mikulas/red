//! The data-transfer plan: one value that describes a copy, a duplicate and a
//! migrate alike.
//!
//! RED had two execution paths for moving rows (one table from a result, one
//! whole schema) and no vocabulary shared between them, so neither could express
//! "duplicate this database, but these two tables empty and that one filtered".
//! [`TransferPlan`] is that vocabulary: the GUI wizard, the CLI and the saved-plan
//! file all build the same value and hand it to `red-service`, which executes it
//! item by item.
//!
//! Invariants this module holds:
//!
//! - **Nothing is silently dropped.** An item that cannot run is
//!   [`ItemAction::Skip`] (visible in the plan and in the summary), never a no-op
//!   inside the executor.
//! - **User text never lands in an identifier position.** [`ItemContent::Where`]
//!   rides as an expression and is only ever rendered into a `WHERE` clause whose
//!   table name came from the driver's own quoting helper.
//! - **No row counting by default.** A plan describes what to move; it does not
//!   pre-scan to say how much. Row totals are `Option` throughout, filled in only
//!   by an explicit dry run.

use crate::{ColumnMap, CopyMode};

/// Everything one transfer job needs. Built by the UI or the CLI, executed by
/// `red-service`.
///
/// The namespaces are the *defaults* for the items: a [`TransferItem`] names only
/// its target table, and the job qualifies it with
/// [`target_namespace`](Self::target_namespace). `None` means "the connection's
/// own current namespace", which is what SQLite (one implicit database) and a
/// namespace-bound MySQL session both want.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TransferPlan {
    /// The namespace the [`ItemSource::Table`] items are read from.
    pub source_namespace: Option<String>,
    /// The namespace every item is written into.
    pub target_namespace: Option<String>,
    pub items: Vec<TransferItem>,
    pub options: TransferOptions,
}

/// One object moving in a [`TransferPlan`]: where it comes from, what it is
/// called on the other side, how the target is prepared, and which rows ride.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TransferItem {
    pub source: ItemSource,
    /// Target table name; defaults to the source's, editable per item.
    pub target_name: String,
    pub action: ItemAction,
    pub content: ItemContent,
    /// An explicit source-column → target-column projection.
    ///
    /// **Empty means identity**: the job reads the source's own column shape and
    /// maps it onto the target one-to-one. That is deliberate, not a missing
    /// value: pinning a mapping costs one `describe_table` per item, and a
    /// 40-table plan should not pay 40 round trips to say "everything, as-is".
    /// The Content step fills this in for the one item the user actually opened.
    pub mapping: Vec<ColumnMap>,
}

impl TransferItem {
    /// A plain table-to-table item under the plan's defaults: same name, create
    /// the target, all rows, identity mapping. The shape almost every item in a
    /// database duplicate has.
    pub fn table(name: impl Into<String>) -> Self {
        let name = name.into();
        Self {
            source: ItemSource::Table {
                schema: None,
                name: name.clone(),
            },
            target_name: name,
            action: ItemAction::Create,
            content: ItemContent::AllRows,
            mapping: Vec::new(),
        }
    }

    /// The source's own label, for progress lines and the summary. A `Result` or
    /// free-SQL item has no source table name, so it borrows its target's.
    pub fn source_label(&self) -> &str {
        match &self.source {
            ItemSource::Table { name, .. } => name,
            ItemSource::Result { .. } | ItemSource::Sql(_) => &self.target_name,
        }
    }

    /// Whether this item writes anything at all. A `Skip` never touches the
    /// target; everything else at least issues DDL.
    pub fn is_active(&self) -> bool {
        !matches!(self.action, ItemAction::Skip)
    }

    /// Whether this item clears or drops data that is already on the target: the
    /// predicate the destructive confirm counts.
    pub fn is_destructive(&self) -> bool {
        matches!(
            self.action,
            ItemAction::Recreate
                | ItemAction::Existing {
                    mode: CopyMode::TruncateInsert
                }
        )
    }
}

/// Where one item's rows come from.
///
/// An enum rather than a table name so views, routines and non-SQL engines can
/// join later without reshaping the plan (see the plan doc's P5).
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum ItemSource {
    /// A table on the source connection. `schema` overrides the plan's
    /// [`source_namespace`](TransferPlan::source_namespace) for this item.
    Table {
        schema: Option<String>,
        name: String,
    },
    /// An already-open result, by its raw `red_service::Epoch` value: its
    /// wrapped SQL is re-read at full fidelity, filter and sort included.
    ///
    /// The raw `u64` rather than the `Epoch` newtype because `red-core` sits
    /// below the protocol crate that mints it; the service re-wraps it on
    /// arrival.
    Result { epoch: u64 },
    /// Free SQL typed into the wizard or the plan file.
    Sql(String),
}

/// How the target table is prepared before rows land in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum ItemAction {
    /// Plan it, show it, write nothing. The visible "no" (see the module docs).
    Skip,
    /// `create_table` from the source's shape (`IF NOT EXISTS`), then load.
    Create,
    /// Load into a table that already exists, mapped by name.
    Existing { mode: CopyMode },
    /// Drop the target, then `Create`. Destructive: the target's rows *and* its
    /// constraints are gone, so it is gated by the destructive confirm.
    Recreate,
}

impl ItemAction {
    /// The past-tense verb for a summary line ("created", "appended into").
    pub fn verb(self) -> &'static str {
        match self {
            ItemAction::Skip => "skipped",
            ItemAction::Create => "created",
            ItemAction::Existing {
                mode: CopyMode::Append,
            } => "appended into",
            ItemAction::Existing {
                mode: CopyMode::TruncateInsert,
            } => "cleared and refilled",
            ItemAction::Recreate => "recreated",
        }
    }
}

/// Which rows of the source ride along.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum ItemContent {
    /// DDL only: create the table, load no rows. The "empty table" ask.
    StructureOnly,
    /// Every row.
    AllRows,
    /// `SELECT * FROM src WHERE <expr>`. The expression is user text in
    /// *expression* position and is never concatenated into an identifier
    /// position, the same rule the result filter follows.
    Where(String),
    /// First N rows, for sampling a big table into a dev database.
    Limit(u64),
}

impl ItemContent {
    /// Whether any rows move. `StructureOnly` (and a degenerate `Limit(0)`) do
    /// not, so the executor can skip opening a cursor at all.
    pub fn moves_rows(&self) -> bool {
        !matches!(self, ItemContent::StructureOnly | ItemContent::Limit(0))
    }

    /// The row-selecting SQL for an already-quoted source table.
    ///
    /// `quoted_table` **must** come from the driver's own `quote_table`; this
    /// function never quotes and never sees a raw identifier. Returns `None`
    /// when no rows move, so a caller cannot accidentally run a `SELECT` for a
    /// structure-only item.
    ///
    /// `LIMIT n` is spelled the same way by every engine RED drives (SQLite,
    /// Postgres, MySQL, ClickHouse); an engine that needs `TOP` would override
    /// this at the driver seam rather than here.
    pub fn select_sql(&self, quoted_table: &str) -> Option<String> {
        match self {
            ItemContent::StructureOnly => None,
            ItemContent::AllRows => Some(format!("SELECT * FROM {quoted_table}")),
            ItemContent::Where(expr) if expr.trim().is_empty() => {
                Some(format!("SELECT * FROM {quoted_table}"))
            }
            ItemContent::Where(expr) => {
                Some(format!("SELECT * FROM {quoted_table} WHERE ({expr})"))
            }
            ItemContent::Limit(0) => None,
            ItemContent::Limit(n) => Some(format!("SELECT * FROM {quoted_table} LIMIT {n}")),
        }
    }

    /// A short label for the Objects row and the review summary.
    pub fn label(&self) -> &'static str {
        match self {
            ItemContent::StructureOnly => "structure only",
            ItemContent::AllRows => "all rows",
            ItemContent::Where(_) => "filtered",
            ItemContent::Limit(_) => "first rows",
        }
    }
}

/// What a failing item does to the rest of the job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum OnError {
    /// Stop the whole job at the first failure (earlier items stay committed).
    #[default]
    Stop,
    /// Record the failure against that item and carry on with the next.
    SkipItem,
}

/// Job-wide options: the things the Review step asks once rather than per item.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TransferOptions {
    /// Carry primary keys into the created tables.
    pub primary_keys: bool,
    /// Recreate secondary indexes after the data loads (deferred pass).
    pub indexes: bool,
    /// Recreate foreign keys among the transferred set after every table is
    /// filled (deferred pass).
    pub foreign_keys: bool,
    pub on_error: OnError,
    /// Plan and render, write nothing: answers with the script and per-item row
    /// estimates instead of running.
    pub dry_run: bool,
}

impl Default for TransferOptions {
    fn default() -> Self {
        Self {
            primary_keys: true,
            indexes: true,
            foreign_keys: true,
            on_error: OnError::Stop,
            dry_run: false,
        }
    }
}

/// What actually happened to one item, reported whether it succeeded or not.
///
/// `migrate_job` used to log a skipped table at `warn` and move on, which is
/// right for an index and wrong for a table. Every item lands here.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum ItemOutcome {
    /// Created on the target and filled.
    Created,
    /// Created on the target with no rows (`StructureOnly`).
    CreatedEmpty,
    /// Dropped, recreated, then filled.
    Recreated,
    /// Loaded into a table that already existed.
    Appended,
    /// Target cleared, then filled.
    Replaced,
    /// Never attempted: planned `Skip`, or dropped by the executor with a
    /// reason (an unreadable source, a view with no columns).
    Skipped { reason: String },
    /// Attempted and failed. Under [`OnError::SkipItem`] the job continued.
    Failed { message: String },
}

impl ItemOutcome {
    /// Whether the item ended in a state the user should look at.
    pub fn is_problem(&self) -> bool {
        matches!(self, ItemOutcome::Failed { .. })
    }
}

/// One item's line in the finished-job report.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ItemReport {
    /// The target table, which is what the user is looking for in the report.
    pub table: String,
    pub outcome: ItemOutcome,
    pub rows: u64,
    /// Non-fatal notes: an index or foreign key the target refused, a column the
    /// mapping could not place. The data is in; these are decoration that isn't.
    pub warnings: Vec<String>,
}

/// The whole job's report: one line per planned item, in execution order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TransferSummary {
    pub items: Vec<ItemReport>,
    /// Rows committed across every item.
    pub rows: u64,
}

impl TransferSummary {
    /// Items whose outcome the user must read (a failure).
    pub fn failures(&self) -> usize {
        self.items.iter().filter(|i| i.outcome.is_problem()).count()
    }

    /// Every non-fatal note across every item, prefixed with its table.
    pub fn warnings(&self) -> Vec<String> {
        self.items
            .iter()
            .flat_map(|i| i.warnings.iter().map(|w| format!("{}: {w}", i.table)))
            .collect()
    }
}

/// A reason the plan is not runnable yet, addressed to the step that can fix it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanIssue {
    /// The item it belongs to, if it is item-scoped.
    pub item: Option<usize>,
    pub message: String,
}

impl PlanIssue {
    /// A plan-wide issue (no destination, nothing selected).
    pub fn plan(message: impl Into<String>) -> Self {
        Self {
            item: None,
            message: message.into(),
        }
    }

    /// An issue the user fixes on one item's row.
    pub fn item(index: usize, message: impl Into<String>) -> Self {
        Self {
            item: Some(index),
            message: message.into(),
        }
    }
}

/// The default action for a target name: writing into something that already
/// exists is `Existing`, everything else is `Create`.
///
/// This is what `migrate_job` used to decide silently (it skipped any table
/// already on the target). Surfacing it as a *planning* input is the point: the
/// wizard shows `Existing ⚠` on that row and the user chooses.
pub fn default_action(target_name: &str, target_objects: &[String]) -> ItemAction {
    if target_objects
        .iter()
        .any(|o| o.eq_ignore_ascii_case(target_name))
    {
        ItemAction::Existing {
            mode: CopyMode::Append,
        }
    } else {
        ItemAction::Create
    }
}

/// [`default_action`] over a whole source list, in order: the wizard's Objects
/// step calls this once when the target namespace is chosen.
pub fn plan_actions(source_names: &[String], target_objects: &[String]) -> Vec<ItemAction> {
    source_names
        .iter()
        .map(|n| default_action(n, target_objects))
        .collect()
}

/// Validate a plan, returning every reason it cannot run.
///
/// Total rather than fail-fast: the wizard badges each step with the issues that
/// belong to it, so the user fixes all of them in one pass instead of
/// rediscovering the next one after each attempt.
pub fn validate(plan: &TransferPlan) -> Result<(), Vec<PlanIssue>> {
    let mut issues = Vec::new();
    if !plan.items.iter().any(TransferItem::is_active) {
        issues.push(PlanIssue::plan("nothing selected to transfer"));
    }
    for (i, item) in plan.items.iter().enumerate() {
        if !item.is_active() {
            continue;
        }
        if item.target_name.trim().is_empty() {
            issues.push(PlanIssue::item(i, "the target table needs a name"));
        }
        if let ItemContent::Where(expr) = &item.content
            && expr.trim().is_empty()
        {
            issues.push(PlanIssue::item(i, "the filter is empty"));
        }
        if matches!(item.source, ItemSource::Sql(ref s) if s.trim().is_empty()) {
            issues.push(PlanIssue::item(i, "the source query is empty"));
        }
    }
    for (i, name) in collisions(plan) {
        issues.push(PlanIssue::item(
            i,
            format!("two items both write into “{name}”"),
        ));
    }
    if issues.is_empty() {
        Ok(())
    } else {
        Err(issues)
    }
}

/// Active items whose target names collide with an earlier item's, as
/// `(index, name)`. Case-insensitive, because no engine RED drives would treat
/// `Users` and `users` as two tables in one namespace.
pub fn collisions(plan: &TransferPlan) -> Vec<(usize, String)> {
    let mut seen: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for (i, item) in plan.items.iter().enumerate() {
        if !item.is_active() {
            continue;
        }
        let key = item.target_name.to_ascii_lowercase();
        if seen.contains(&key) {
            out.push((i, item.target_name.clone()));
        } else {
            seen.push(key);
        }
    }
    out
}

/// A one-line count summary for the wizard footer ("6 tables · 2 structure only
/// · 1 skipped"), so the consequence of the last click is visible on every step.
pub fn summarize(plan: &TransferPlan) -> String {
    let active = plan.items.iter().filter(|i| i.is_active()).count();
    let structure = plan
        .items
        .iter()
        .filter(|i| i.is_active() && !i.content.moves_rows())
        .count();
    let skipped = plan.items.len() - active;
    let mut parts = vec![format!("{active} table{}", plural(active))];
    if structure > 0 {
        parts.push(format!("{structure} structure only"));
    }
    if skipped > 0 {
        parts.push(format!("{skipped} skipped"));
    }
    let destructive = plan.items.iter().filter(|i| i.is_destructive()).count();
    if destructive > 0 {
        parts.push(format!("{destructive} overwritten"));
    }
    parts.join(" · ")
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan_of(items: Vec<TransferItem>) -> TransferPlan {
        TransferPlan {
            source_namespace: Some("src".into()),
            target_namespace: Some("dst".into()),
            items,
            options: TransferOptions::default(),
        }
    }

    #[test]
    fn present_on_the_target_defaults_to_existing() {
        let targets = vec!["Users".to_string()];
        assert_eq!(
            default_action("users", &targets),
            ItemAction::Existing {
                mode: CopyMode::Append
            }
        );
        assert_eq!(default_action("orders", &targets), ItemAction::Create);
    }

    #[test]
    fn plan_actions_keeps_source_order() {
        let sources = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let targets = vec!["b".to_string()];
        assert_eq!(
            plan_actions(&sources, &targets),
            vec![
                ItemAction::Create,
                ItemAction::Existing {
                    mode: CopyMode::Append
                },
                ItemAction::Create
            ]
        );
    }

    #[test]
    fn content_renders_rows_or_nothing() {
        assert_eq!(
            ItemContent::AllRows.select_sql("\"s\".\"t\""),
            Some("SELECT * FROM \"s\".\"t\"".into())
        );
        assert_eq!(
            ItemContent::Where("id > 3".into()).select_sql("\"t\""),
            Some("SELECT * FROM \"t\" WHERE (id > 3)".into())
        );
        assert_eq!(
            ItemContent::Limit(10).select_sql("\"t\""),
            Some("SELECT * FROM \"t\" LIMIT 10".into())
        );
        // Structure-only never yields a SELECT, so a caller cannot read rows for
        // an item that is meant to create an empty table.
        assert_eq!(ItemContent::StructureOnly.select_sql("\"t\""), None);
        assert_eq!(ItemContent::Limit(0).select_sql("\"t\""), None);
    }

    #[test]
    fn a_blank_filter_degrades_to_all_rows_rather_than_broken_sql() {
        assert_eq!(
            ItemContent::Where("   ".into()).select_sql("\"t\""),
            Some("SELECT * FROM \"t\"".into())
        );
    }

    #[test]
    fn collisions_are_case_insensitive_and_ignore_skips() {
        let mut a = TransferItem::table("users");
        a.target_name = "Users".into();
        let mut b = TransferItem::table("people");
        b.target_name = "users".into();
        let mut skipped = TransferItem::table("noise");
        skipped.target_name = "users".into();
        skipped.action = ItemAction::Skip;
        let plan = plan_of(vec![a, b, skipped]);
        assert_eq!(collisions(&plan), vec![(1, "users".to_string())]);
    }

    #[test]
    fn validate_reports_every_problem_at_once() {
        let mut blank = TransferItem::table("a");
        blank.target_name = "  ".into();
        let mut filtered = TransferItem::table("b");
        filtered.content = ItemContent::Where(String::new());
        let issues = validate(&plan_of(vec![blank, filtered])).unwrap_err();
        assert_eq!(issues.len(), 2);
        assert_eq!(issues[0].item, Some(0));
        assert_eq!(issues[1].item, Some(1));
    }

    #[test]
    fn validate_rejects_a_plan_with_nothing_active() {
        let mut only = TransferItem::table("a");
        only.action = ItemAction::Skip;
        let issues = validate(&plan_of(vec![only])).unwrap_err();
        assert_eq!(issues[0].item, None);
    }

    #[test]
    fn validate_accepts_a_plain_duplicate() {
        assert!(validate(&plan_of(vec![TransferItem::table("users")])).is_ok());
    }

    #[test]
    fn destructive_is_recreate_or_truncate_only() {
        let mut item = TransferItem::table("t");
        assert!(!item.is_destructive());
        item.action = ItemAction::Existing {
            mode: CopyMode::Append,
        };
        assert!(!item.is_destructive());
        item.action = ItemAction::Existing {
            mode: CopyMode::TruncateInsert,
        };
        assert!(item.is_destructive());
        item.action = ItemAction::Recreate;
        assert!(item.is_destructive());
    }

    #[test]
    fn summary_counts_what_the_footer_shows() {
        let mut structure = TransferItem::table("audit_log");
        structure.content = ItemContent::StructureOnly;
        let mut skipped = TransferItem::table("migrations");
        skipped.action = ItemAction::Skip;
        let plan = plan_of(vec![TransferItem::table("users"), structure, skipped]);
        assert_eq!(summarize(&plan), "2 tables · 1 structure only · 1 skipped");
    }
}
