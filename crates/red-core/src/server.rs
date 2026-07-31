//! One live sample of a server's state, in a shape all three driver seams answer
//! in: what it is using, what it is doing, and how long it has been up.
//!
//! The sibling of [`health`](crate::health) and the deliberate opposite of it.
//! The health report answers "what is structurally wrong in here" from catalog
//! views and is worth saving; this answers "what is happening right now" and is
//! worth re-reading. Nothing here is persisted.
//!
//! A [`ServerMetric`] carries a *typed* [`MetricValue`] rather than a formatted
//! string, so the panel can right-align a number, colour a ratio, and draw a bar
//! for a bounded value without parsing text back out. It is also why the model
//! and the human read the same numbers: the agent's `kv_server_info` tool
//! renders this same snapshot instead of formatting its own.
//!
//! [`ServerSnapshot::unavailable`] is load-bearing, and is copied deliberately
//! from [`HealthReport::unavailable`](crate::health::HealthReport::unavailable):
//! a Postgres role without `pg_monitor`, a managed Redis with `INFO` sections
//! stripped, and a Mongo user without `clusterMonitor` all produce partial
//! answers, and a panel that silently drops a metric reads as "zero", which is a
//! lie. Every driver arm reports what it could not see rather than omitting it.

use std::time::Duration;

use crate::DbKind;

/// One live server metric.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerMetric {
    /// Stable identifier across refreshes, so a sample can be diffed against the
    /// previous one (see [`ServerSnapshot::rate_since`]): `"used_memory"`,
    /// `"total_commands_processed"`. Engine-native spelling, never shown.
    pub key: &'static str,
    /// The name a human reads, already phrased for the panel.
    pub label: String,
    pub value: MetricValue,
    pub group: MetricGroup,
    /// The engine's own note, when there is one worth showing verbatim: the
    /// eviction policy behind a memory ratio, the hit/miss counts behind a rate.
    pub detail: Option<String>,
}

impl ServerMetric {
    pub fn new(
        group: MetricGroup,
        key: &'static str,
        label: impl Into<String>,
        value: MetricValue,
    ) -> Self {
        Self {
            key,
            label: label.into(),
            value,
            group,
            detail: None,
        }
    }

    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

/// What one metric *is*, so the panel renders it without re-parsing text.
///
/// The [`Count`](Self::Count) / [`Total`](Self::Total) split is the one that
/// earns its keep: a gauge is meaningful on its own, while a monotonic total is
/// only meaningful as a rate, and the panel derives that rate from two samples
/// (see [`ServerSnapshot::rate_since`]). A single `Count` arm plus a `bool`
/// would let the two be transposed at the call site; two arms cannot be.
#[derive(Debug, Clone, PartialEq)]
pub enum MetricValue {
    /// A gauge: how many of something there are *now* (connected clients).
    Count(u64),
    /// A cumulative counter that only goes up until the server restarts
    /// (`total_commands_processed`). Rendered with a derived per-second rate
    /// when a previous sample is available.
    Total(u64),
    Bytes(u64),
    /// A bounded value and its ceiling, rendered as a bar: connections of
    /// `max_connections`, memory of `maxmemory`. A `total` of `0` means
    /// *unlimited* on every engine that reports one, and
    /// [`fraction`](Self::fraction) answers `None` there rather than dividing by
    /// zero.
    Ratio {
        used: u64,
        total: u64,
    },
    /// A rate the engine itself computed, per second.
    Rate(f64),
    /// `0.0..=1.0`, rendered as a percentage: a cache hit ratio.
    Percent(f64),
    /// A dimensionless multiplier the engine reports as one (Redis
    /// `mem_fragmentation_ratio` is `1.03`, not `103%`, and rendering it as a
    /// percentage would misread as "using 103% of memory").
    Factor(f64),
    Duration(Duration),
    /// A value with no numeric meaning: a version, a role, `"ok"`.
    Text(String),
}

impl MetricValue {
    /// The bar fill for a bounded value, or `None` when there is no ceiling to
    /// fill against. `Ratio { total: 0 }` is *unlimited*, not full.
    pub fn fraction(&self) -> Option<f32> {
        match self {
            MetricValue::Ratio { used, total } if *total > 0 => {
                Some((*used as f64 / *total as f64).clamp(0.0, 1.0) as f32)
            }
            MetricValue::Percent(p) => Some(p.clamp(0.0, 1.0) as f32),
            _ => None,
        }
    }

    /// The value as one line of text. The single rendering of a metric for the
    /// whole app: the panel draws a bar *beside* this, and the agent's tool
    /// output is this string, so the two can never disagree about a number.
    pub fn render(&self) -> String {
        match self {
            MetricValue::Count(n) | MetricValue::Total(n) => group_digits(*n),
            MetricValue::Bytes(n) => human_bytes(*n),
            MetricValue::Ratio { used, total } => {
                if *total == 0 {
                    format!("{} (no limit)", group_digits(*used))
                } else {
                    format!("{} / {}", group_digits(*used), group_digits(*total))
                }
            }
            MetricValue::Rate(r) => format!("{r:.1}/s"),
            MetricValue::Percent(p) => format!("{:.1}%", p * 100.0),
            MetricValue::Factor(f) => format!("{f:.2}x"),
            MetricValue::Duration(d) => human_duration(*d),
            MetricValue::Text(t) => t.clone(),
        }
    }
}

/// The heading a metric sits under. A closed set, because the point of the
/// shared panel is that Postgres, Redis and Mongo answer the *same* questions in
/// the same order; a metric that fits none of these belongs in its engine's own
/// tab rather than widening this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MetricGroup {
    /// Is it running out of memory.
    Memory,
    /// Is it busy.
    Throughput,
    /// Who is connected, and is the ceiling close.
    Connections,
    /// How much data is in there (keys, database size).
    Storage,
    Replication,
    /// Is the data safely on disk (Redis RDB/AOF, checkpoints).
    Persistence,
    /// Version, uptime, and what the deployment is.
    Server,
}

impl MetricGroup {
    /// Every group, in the order the panel draws them: the two that predict an
    /// outage first, the descriptive ones last.
    pub const ORDER: [MetricGroup; 7] = [
        MetricGroup::Memory,
        MetricGroup::Throughput,
        MetricGroup::Connections,
        MetricGroup::Storage,
        MetricGroup::Replication,
        MetricGroup::Persistence,
        MetricGroup::Server,
    ];

    pub const fn heading(self) -> &'static str {
        match self {
            MetricGroup::Memory => "Memory",
            MetricGroup::Throughput => "Throughput",
            MetricGroup::Connections => "Connections",
            MetricGroup::Storage => "Storage",
            MetricGroup::Replication => "Replication",
            MetricGroup::Persistence => "Persistence",
            MetricGroup::Server => "Server",
        }
    }
}

/// One sample of a server's live state.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerSnapshot {
    /// Unix seconds on the local clock, for the "as of" line and as the base of
    /// the interval [`rate_since`](Self::rate_since) divides by. Local rather
    /// than server time on purpose: the interval between two RED refreshes is a
    /// local measurement, and mixing clocks would let a skewed server produce a
    /// negative one.
    pub taken_at: i64,
    pub engine: DbKind,
    pub metrics: Vec<ServerMetric>,
    /// Checks this engine or this role could not answer, reported rather than
    /// omitted. One line each, phrased so the user can act on it ("replication:
    /// this role is not a member of pg_monitor").
    pub unavailable: Vec<String>,
}

impl ServerSnapshot {
    pub fn new(engine: DbKind, taken_at: i64) -> Self {
        Self {
            taken_at,
            engine,
            metrics: Vec::new(),
            unavailable: Vec::new(),
        }
    }

    pub fn push(&mut self, metric: ServerMetric) {
        self.metrics.push(metric);
    }

    /// Record a metric, or the reason it is missing, in one call. Drivers read a
    /// server's answer as a `Result` far more often than as a value, and this
    /// keeps the "report it, do not drop it" contract from being a discipline
    /// the next arm has to remember.
    pub fn push_or_note(&mut self, metric: Option<ServerMetric>, missing: impl Into<String>) {
        match metric {
            Some(m) => self.metrics.push(m),
            None => self.unavailable.push(missing.into()),
        }
    }

    pub fn note_unavailable(&mut self, reason: impl Into<String>) {
        self.unavailable.push(reason.into());
    }

    pub fn get(&self, key: &str) -> Option<&ServerMetric> {
        self.metrics.iter().find(|m| m.key == key)
    }

    /// The metrics of one group, in insertion order (which is the driver's own
    /// reading order, and the one the panel keeps).
    pub fn group(&self, group: MetricGroup) -> impl Iterator<Item = &ServerMetric> {
        self.metrics.iter().filter(move |m| m.group == group)
    }

    /// The per-second rate of a [`MetricValue::Total`] between `prev` and this
    /// sample, or `None` when it cannot be computed honestly.
    ///
    /// `None` covers four real cases, all of which must *not* render as zero: the
    /// key is absent from one sample, it is not a cumulative total, the samples
    /// share a timestamp (two refreshes inside the same second), or the counter
    /// went **backwards**, which means the server restarted between them and the
    /// delta is meaningless rather than negative.
    pub fn rate_since(&self, prev: &ServerSnapshot, key: &str) -> Option<f64> {
        let (MetricValue::Total(now), MetricValue::Total(then)) =
            (&self.get(key)?.value, &prev.get(key)?.value)
        else {
            return None;
        };
        let elapsed = self.taken_at.checked_sub(prev.taken_at)?;
        if elapsed <= 0 || now < then {
            return None;
        }
        Some((now - then) as f64 / elapsed as f64)
    }
}

/// Group a number's digits in threes so a large total reads at a glance.
///
/// Duplicated from the UI crate's formatter rather than shared, because
/// `red-core` must not depend on the UI and the alternative -- returning raw
/// numbers and formatting twice -- is how the agent's answer and the panel's
/// answer drift apart.
fn group_digits(n: u64) -> String {
    let digits = n.to_string();
    let bytes = digits.as_bytes();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

/// A byte size in IEC units, one decimal past the bytes tier.
fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// An uptime a human reads at a glance. Coarser than the session panel's
/// elapsed-time formatter on purpose: nobody needs a server's uptime to the
/// tenth of a second, and "12d 4h" is the answer to the question being asked.
fn human_duration(d: Duration) -> String {
    let secs = d.as_secs();
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3_599 => format!("{}m {}s", secs / 60, secs % 60),
        3_600..=86_399 => format!("{}h {}m", secs / 3_600, (secs % 3_600) / 60),
        _ => format!("{}d {}h", secs / 86_400, (secs % 86_400) / 3_600),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(taken_at: i64, metrics: Vec<ServerMetric>) -> ServerSnapshot {
        ServerSnapshot {
            taken_at,
            engine: DbKind::Redis,
            metrics,
            unavailable: Vec::new(),
        }
    }

    fn total(key: &'static str, n: u64) -> ServerMetric {
        ServerMetric::new(MetricGroup::Throughput, key, key, MetricValue::Total(n))
    }

    #[test]
    fn an_unlimited_ratio_has_no_bar_and_does_not_divide_by_zero() {
        // `maxmemory 0` is Redis for "no limit". Rendering it as "used / 0" or
        // filling the bar would both read as a server at its ceiling.
        let v = MetricValue::Ratio {
            used: 4096,
            total: 0,
        };
        assert_eq!(v.fraction(), None);
        assert_eq!(v.render(), "4,096 (no limit)");
    }

    #[test]
    fn a_bounded_ratio_fills_its_bar() {
        let v = MetricValue::Ratio {
            used: 25,
            total: 100,
        };
        assert_eq!(v.fraction(), Some(0.25));
        assert_eq!(v.render(), "25 / 100");
    }

    #[test]
    fn a_ratio_over_its_ceiling_clamps_rather_than_overflowing_the_bar() {
        let v = MetricValue::Ratio {
            used: 150,
            total: 100,
        };
        assert_eq!(v.fraction(), Some(1.0));
    }

    #[test]
    fn percent_renders_as_a_percentage_not_a_fraction() {
        assert_eq!(MetricValue::Percent(0.9973).render(), "99.7%");
        assert_eq!(MetricValue::Percent(0.0).render(), "0.0%");
    }

    #[test]
    fn a_factor_is_not_rendered_as_a_percentage() {
        // 1.03 fragmentation is healthy; "103%" would read as over-capacity.
        assert_eq!(MetricValue::Factor(1.03).render(), "1.03x");
    }

    #[test]
    fn bytes_and_counts_render_in_the_units_a_reader_expects() {
        assert_eq!(MetricValue::Bytes(512).render(), "512 B");
        assert_eq!(MetricValue::Bytes(1024 * 1024 * 3 / 2).render(), "1.5 MiB");
        assert_eq!(MetricValue::Count(1_234_567).render(), "1,234,567");
    }

    #[test]
    fn durations_coarsen_as_they_grow() {
        assert_eq!(
            MetricValue::Duration(Duration::from_secs(42)).render(),
            "42s"
        );
        assert_eq!(
            MetricValue::Duration(Duration::from_secs(90)).render(),
            "1m 30s"
        );
        assert_eq!(
            MetricValue::Duration(Duration::from_secs(3 * 3600 + 25 * 60)).render(),
            "3h 25m"
        );
        assert_eq!(
            MetricValue::Duration(Duration::from_secs(12 * 86_400 + 4 * 3600)).render(),
            "12d 4h"
        );
    }

    #[test]
    fn a_rate_is_derived_from_two_samples_of_a_total() {
        let prev = snap(1000, vec![total("cmds", 100)]);
        let now = snap(1010, vec![total("cmds", 600)]);
        assert_eq!(now.rate_since(&prev, "cmds"), Some(50.0));
    }

    #[test]
    fn a_counter_that_went_backwards_reports_no_rate() {
        // The server restarted between samples: the delta is meaningless, and a
        // negative or huge rate would be worse than saying nothing.
        let prev = snap(1000, vec![total("cmds", 9_000)]);
        let now = snap(1010, vec![total("cmds", 12)]);
        assert_eq!(now.rate_since(&prev, "cmds"), None);
    }

    #[test]
    fn two_samples_in_the_same_second_report_no_rate() {
        let prev = snap(1000, vec![total("cmds", 100)]);
        let now = snap(1000, vec![total("cmds", 600)]);
        assert_eq!(now.rate_since(&prev, "cmds"), None);
    }

    #[test]
    fn a_gauge_never_yields_a_rate() {
        let gauge = |n| {
            ServerMetric::new(
                MetricGroup::Connections,
                "clients",
                "clients",
                MetricValue::Count(n),
            )
        };
        let prev = snap(1000, vec![gauge(10)]);
        let now = snap(1010, vec![gauge(20)]);
        assert_eq!(now.rate_since(&prev, "clients"), None);
    }

    #[test]
    fn a_key_missing_from_either_sample_yields_no_rate() {
        let prev = snap(1000, vec![]);
        let now = snap(1010, vec![total("cmds", 600)]);
        assert_eq!(now.rate_since(&prev, "cmds"), None);
        assert_eq!(prev.rate_since(&now, "cmds"), None);
    }

    #[test]
    fn push_or_note_records_the_reason_instead_of_dropping_the_metric() {
        let mut s = ServerSnapshot::new(DbKind::Postgres, 0);
        s.push_or_note(None, "replication: not a member of pg_monitor");
        assert!(s.metrics.is_empty());
        assert_eq!(s.unavailable, ["replication: not a member of pg_monitor"]);
    }

    #[test]
    fn groups_read_back_in_insertion_order() {
        let mut s = ServerSnapshot::new(DbKind::Redis, 0);
        s.push(total("a", 1));
        s.push(ServerMetric::new(
            MetricGroup::Memory,
            "m",
            "m",
            MetricValue::Bytes(1),
        ));
        s.push(total("b", 2));
        let keys: Vec<_> = s.group(MetricGroup::Throughput).map(|m| m.key).collect();
        assert_eq!(keys, ["a", "b"]);
    }
}
