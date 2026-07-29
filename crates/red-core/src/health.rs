//! The connection health report: a saved, point-in-time answer to "what is wrong
//! in here" for a SQL connection (see
//! `docs/plans/todo/connection-health-report.md`).
//!
//! The SQL analog of [`kv::RedisAnalysis`](crate::kv::RedisAnalysis), but computed
//! the opposite way. Redis has no catalog, so its report is rolled up UI-side from
//! a scan sweep. Every fact in *this* report already exists pre-aggregated in a
//! catalog view, so it is a handful of bounded `SELECT`s pushed down to the
//! server: walking rows to derive it would break the never-materialize rule and
//! be strictly worse than asking.
//!
//! Nothing here is executed by RED. A finding's `suggested_sql` is text to read
//! and paste; `CREATE INDEX` on a large production table is a locking event, and
//! the decision to run one belongs to the operator, through the editor, behind
//! the same guards as any other statement.

use crate::{DbKind, TableRef};

/// One connection's health snapshot.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, PartialEq)]
pub struct HealthReport {
    /// Unix seconds on the local clock, for the "as of" line and to tell a stale
    /// saved report from a fresh one. Same contract as
    /// [`RedisAnalysis::generated_at`](crate::kv::RedisAnalysis::generated_at).
    pub generated_at: i64,
    pub engine: DbKind,
    /// The namespace the report covers, when it is scoped to one.
    pub namespace: Option<String>,
    pub totals: SizeTotals,
    /// Largest objects first, capped driver-side. The "where did the disk go"
    /// answer, and the most-read part of the report.
    pub tables: Vec<TableSize>,
    pub findings: Vec<Finding>,
    /// Checks that could not run here: a missing extension, an absent `sys`
    /// schema, an insufficient privilege.
    ///
    /// Load-bearing. A report that silently omits the unused-index check on a
    /// MariaDB without `sys` is a report that lies by omission, and "no findings"
    /// would read as "healthy".
    pub unavailable: Vec<UnavailableCheck>,
}

#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SizeTotals {
    pub bytes: u64,
    /// Of [`bytes`](Self::bytes), how much is index rather than table data.
    pub index_bytes: u64,
    pub table_count: u64,
}

#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableSize {
    pub table: TableRef,
    pub bytes: u64,
    pub index_bytes: u64,
    /// The engine's row estimate, not a `COUNT(*)`: this report must not scan.
    pub estimated_rows: i64,
}

/// How much a finding matters. Three levels, because a fourth would be a
/// distinction nobody acts on differently.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Worth knowing, not worth doing anything about today.
    Info,
    /// Will bite under load or growth.
    Warn,
    /// Is biting now.
    Bad,
}

/// What kind of problem a finding is. A closed enum rather than a free string, so
/// the UI can group, icon, and filter, and so a new check is a compile-time
/// addition everywhere it matters.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FindingKind {
    UnusedIndex,
    RedundantIndex,
    /// A foreign key with no index on the child side: every parent delete or
    /// update scans the child table.
    MissingFkIndex,
    NoPrimaryKey,
    /// Dead tuples / vacuum lag (Postgres), or fragmentation elsewhere.
    DeadTuples,
    /// Estimated bloat. Always `Info`: the estimate is approximate by nature.
    Bloat,
    /// Reads dominated by sequential scans on a table big enough to care.
    SeqScanHeavy,
    /// ClickHouse: too many parts in a partition.
    TooManyParts,
    /// A non-transactional or otherwise legacy storage engine.
    LegacyEngine,
    /// SQLite: free pages worth reclaiming with VACUUM.
    Fragmentation,
}

#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub severity: Severity,
    pub kind: FindingKind,
    pub object: Option<TableRef>,
    /// One line, already phrased for a human: "8 indexes have never been used".
    pub title: String,
    /// The numbers behind the title.
    pub detail: String,
    /// The remediation, as text to paste. **Never executed by RED.**
    pub suggested_sql: Option<String>,
}

/// A check that could not run, and why. Reported rather than skipped.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnavailableCheck {
    pub kind: FindingKind,
    /// Why, in one line the user can act on ("needs the sys schema").
    pub reason: String,
}

impl HealthReport {
    pub fn new(engine: DbKind, namespace: Option<String>, generated_at: i64) -> Self {
        Self {
            generated_at,
            engine,
            namespace,
            totals: SizeTotals::default(),
            tables: Vec::new(),
            findings: Vec::new(),
            unavailable: Vec::new(),
        }
    }

    /// Findings worst-first, then by kind, so the panel's order is the reading
    /// order and two runs of the same database list the same way.
    pub fn sorted_findings(&self) -> Vec<&Finding> {
        let mut out: Vec<&Finding> = self.findings.iter().collect();
        out.sort_by(|a, b| {
            b.severity
                .cmp(&a.severity)
                .then_with(|| a.title.cmp(&b.title))
        });
        out
    }

    pub fn count_at_least(&self, severity: Severity) -> usize {
        self.findings
            .iter()
            .filter(|f| f.severity >= severity)
            .count()
    }
}

/// Row-count and size floors under which a finding is noise rather than news.
///
/// An unused index on a 40-row lookup table is not a problem, and reporting it
/// trains the user to ignore the panel: twenty warnings on a healthy database is
/// worse than none. Every check that can apply a floor does.
pub mod floors {
    /// Below this many estimated rows, a table's index and scan habits do not
    /// matter.
    pub const ROWS: i64 = 10_000;
    /// Below this size, neither does its disk usage (16 MiB).
    pub const BYTES: u64 = 16 * 1024 * 1024;
}
