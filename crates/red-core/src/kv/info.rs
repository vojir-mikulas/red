//! `INFO` into a [`ServerSnapshot`]: the Redis arm of the shared server panel.
//!
//! Pure text-in, types-out, so the whole of it is unit-testable without a
//! server, which matters because the interesting inputs are the *partial* ones:
//! a managed provider that strips `INFO` sections, an old server that never had
//! a field, a truncated reply. The rule throughout is that a section RED did not
//! receive becomes an [`unavailable`](ServerSnapshot::unavailable) line, never a
//! metric reading zero.
//!
//! Section presence is what the degradation keys off, not field presence.
//! `# Replication` missing means the provider withheld it; `connected_slaves`
//! missing from a `# Replication` that *is* present means this server genuinely
//! has none, and the two must not be reported the same way.

use std::collections::HashMap;
use std::time::Duration;

use crate::DbKind;
use crate::server::{MetricGroup, MetricValue, ServerMetric, ServerSnapshot};

/// The `INFO` sections each metric group is read out of, so a stripped section
/// names itself in the unavailable list rather than leaving a silent hole.
const SECTIONS: [(&str, &str); 6] = [
    ("Server", "version and uptime"),
    ("Clients", "connection counts"),
    ("Memory", "memory use and fragmentation"),
    ("Stats", "throughput, hit rate and evictions"),
    ("Replication", "role and replica lag"),
    ("Persistence", "RDB/AOF save state"),
];

/// One `INFO` reply, split into the sections it declared and the fields it
/// carried. Parsing once into this keeps every metric below reading from the
/// same view of the text instead of re-scanning it per field.
struct Info<'a> {
    fields: HashMap<&'a str, &'a str>,
    sections: Vec<&'a str>,
    /// The `# Keyspace` `dbN:keys=..,expires=..` lines, which are the one part
    /// of `INFO` that is a list rather than a flat field set.
    keyspace: Vec<(&'a str, &'a str)>,
}

impl<'a> Info<'a> {
    fn parse(text: &'a str) -> Self {
        let mut fields = HashMap::new();
        let mut sections = Vec::new();
        let mut keyspace = Vec::new();
        let mut in_keyspace = false;
        for line in text.lines().map(str::trim) {
            if let Some(name) = line.strip_prefix('#') {
                let name = name.trim();
                in_keyspace = name.eq_ignore_ascii_case("Keyspace");
                sections.push(name);
                continue;
            }
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            if in_keyspace {
                keyspace.push((key, value));
            }
            fields.insert(key, value);
        }
        Self {
            fields,
            sections,
            keyspace,
        }
    }

    fn has(&self, section: &str) -> bool {
        self.sections
            .iter()
            .any(|s| s.eq_ignore_ascii_case(section))
    }

    fn text(&self, key: &str) -> Option<String> {
        self.fields
            .get(key)
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    }

    fn num(&self, key: &str) -> Option<u64> {
        self.fields.get(key)?.trim().parse().ok()
    }

    fn float(&self, key: &str) -> Option<f64> {
        self.fields.get(key)?.trim().parse().ok()
    }
}

/// Parse an `INFO` reply into a snapshot, `taken_at` stamped by the caller (this
/// crate has no clock).
///
/// Never fails: an empty or unrecognisable reply yields a snapshot with no
/// metrics and every section listed as unavailable, which is the honest reading
/// of "the server told us nothing".
pub fn parse_info(text: &str, taken_at: i64) -> ServerSnapshot {
    let info = Info::parse(text);
    let mut snap = ServerSnapshot::new(DbKind::Redis, taken_at);

    for (section, what) in SECTIONS {
        if !info.has(section) {
            snap.note_unavailable(format!(
                "{what}: this server did not return the INFO {section} section"
            ));
        }
    }

    memory(&info, &mut snap);
    throughput(&info, &mut snap);
    connections(&info, &mut snap);
    storage(&info, &mut snap);
    replication(&info, &mut snap);
    persistence(&info, &mut snap);
    server(&info, &mut snap);
    snap
}

fn memory(info: &Info<'_>, snap: &mut ServerSnapshot) {
    if !info.has("Memory") {
        return;
    }
    if let Some(used) = info.num("used_memory") {
        // `maxmemory: 0` is Redis for unlimited, which `MetricValue::Ratio`
        // already renders as such rather than dividing by it.
        let metric = ServerMetric::new(
            MetricGroup::Memory,
            "used_memory",
            "Used memory",
            MetricValue::Ratio {
                used,
                total: info.num("maxmemory").unwrap_or(0),
            },
        );
        snap.push(match info.text("maxmemory_policy") {
            Some(policy) => metric.with_detail(format!("eviction policy: {policy}")),
            None => metric,
        });
    }
    if let Some(rss) = info.num("used_memory_rss") {
        snap.push(ServerMetric::new(
            MetricGroup::Memory,
            "used_memory_rss",
            "Resident set",
            MetricValue::Bytes(rss),
        ));
    }
    if let Some(frag) = info.float("mem_fragmentation_ratio") {
        snap.push(
            ServerMetric::new(
                MetricGroup::Memory,
                "mem_fragmentation_ratio",
                "Fragmentation",
                MetricValue::Factor(frag),
            )
            .with_detail("resident over allocated; well above 1.0 means fragmented"),
        );
    }
    if let Some(peak) = info.num("used_memory_peak") {
        snap.push(ServerMetric::new(
            MetricGroup::Memory,
            "used_memory_peak",
            "Peak memory",
            MetricValue::Bytes(peak),
        ));
    }
}

fn throughput(info: &Info<'_>, snap: &mut ServerSnapshot) {
    if !info.has("Stats") {
        return;
    }
    if let Some(ops) = info.float("instantaneous_ops_per_sec") {
        snap.push(ServerMetric::new(
            MetricGroup::Throughput,
            "instantaneous_ops_per_sec",
            "Operations",
            MetricValue::Rate(ops),
        ));
    }
    if let Some(total) = info.num("total_commands_processed") {
        snap.push(ServerMetric::new(
            MetricGroup::Throughput,
            "total_commands_processed",
            "Commands processed",
            MetricValue::Total(total),
        ));
    }
    // A server that has served nothing yet has no hit rate; 0% would read as a
    // cache that is missing everything, which is the opposite of the truth.
    if let (Some(hits), Some(misses)) = (info.num("keyspace_hits"), info.num("keyspace_misses"))
        && hits + misses > 0
    {
        snap.push(
            ServerMetric::new(
                MetricGroup::Throughput,
                "keyspace_hit_rate",
                "Keyspace hit rate",
                MetricValue::Percent(hits as f64 / (hits + misses) as f64),
            )
            .with_detail(format!("{hits} hits / {misses} misses")),
        );
    }
}

fn connections(info: &Info<'_>, snap: &mut ServerSnapshot) {
    if !info.has("Clients") {
        return;
    }
    if let Some(connected) = info.num("connected_clients") {
        snap.push(ServerMetric::new(
            MetricGroup::Connections,
            "connected_clients",
            "Connected clients",
            MetricValue::Ratio {
                used: connected,
                total: info.num("maxclients").unwrap_or(0),
            },
        ));
    }
    if let Some(blocked) = info.num("blocked_clients") {
        snap.push(
            ServerMetric::new(
                MetricGroup::Connections,
                "blocked_clients",
                "Blocked clients",
                MetricValue::Count(blocked),
            )
            .with_detail("waiting in BLPOP / BRPOP / XREAD BLOCK"),
        );
    }
    if let Some(rejected) = info.num("rejected_connections") {
        snap.push(ServerMetric::new(
            MetricGroup::Connections,
            "rejected_connections",
            "Rejected connections",
            MetricValue::Total(rejected),
        ));
    }
}

fn storage(info: &Info<'_>, snap: &mut ServerSnapshot) {
    if info.has("Keyspace") {
        let mut keys = 0u64;
        let mut expiring = 0u64;
        let mut per_db = Vec::new();
        for (db, stats) in &info.keyspace {
            let field = |name: &str| {
                stats
                    .split(',')
                    .filter_map(|p| p.split_once('='))
                    .find(|(k, _)| *k == name)
                    .and_then(|(_, v)| v.parse::<u64>().ok())
                    .unwrap_or(0)
            };
            let db_keys = field("keys");
            keys += db_keys;
            expiring += field("expires");
            per_db.push(format!("{db}: {db_keys}"));
        }
        snap.push(
            ServerMetric::new(
                MetricGroup::Storage,
                "keys",
                "Keys",
                MetricValue::Count(keys),
            )
            .with_detail(per_db.join(" \u{b7} ")),
        );
        snap.push(
            ServerMetric::new(
                MetricGroup::Storage,
                "expires",
                "With a TTL",
                MetricValue::Count(expiring),
            )
            .with_detail("keys that will expire on their own"),
        );
    }
    // Evictions and expiries describe the keyspace but live in `# Stats`, so
    // they are gated on that section rather than on `# Keyspace`.
    if info.has("Stats") {
        if let Some(evicted) = info.num("evicted_keys") {
            snap.push(
                ServerMetric::new(
                    MetricGroup::Storage,
                    "evicted_keys",
                    "Evicted keys",
                    MetricValue::Total(evicted),
                )
                .with_detail("dropped to stay under maxmemory"),
            );
        }
        if let Some(expired) = info.num("expired_keys") {
            snap.push(ServerMetric::new(
                MetricGroup::Storage,
                "expired_keys",
                "Expired keys",
                MetricValue::Total(expired),
            ));
        }
    }
}

fn replication(info: &Info<'_>, snap: &mut ServerSnapshot) {
    if !info.has("Replication") {
        return;
    }
    let role = info.text("role");
    if let Some(role) = &role {
        snap.push(ServerMetric::new(
            MetricGroup::Replication,
            "role",
            "Role",
            MetricValue::Text(role.clone()),
        ));
    }
    if let Some(replicas) = info.num("connected_slaves") {
        snap.push(ServerMetric::new(
            MetricGroup::Replication,
            "connected_slaves",
            "Connected replicas",
            MetricValue::Count(replicas),
        ));
    }
    if role.as_deref() == Some("slave") {
        if let Some(link) = info.text("master_link_status") {
            snap.push(ServerMetric::new(
                MetricGroup::Replication,
                "master_link_status",
                "Link to primary",
                MetricValue::Text(link),
            ));
        }
        // The replica's own view of how far behind it is. Reported as a
        // duration, which is the unit an operator decides on; the byte offset
        // difference is not comparable across servers.
        if let Some(behind) = info.num("master_last_io_seconds_ago") {
            snap.push(ServerMetric::new(
                MetricGroup::Replication,
                "master_last_io_seconds_ago",
                "Last contact with primary",
                MetricValue::Duration(Duration::from_secs(behind)),
            ));
        }
        if let (Some(primary), Some(mine)) = (
            info.num("master_repl_offset"),
            info.num("slave_repl_offset"),
        ) {
            snap.push(
                ServerMetric::new(
                    MetricGroup::Replication,
                    "repl_offset_lag",
                    "Replication lag",
                    MetricValue::Bytes(primary.saturating_sub(mine)),
                )
                .with_detail("replication stream this replica has yet to apply"),
            );
        }
    }
}

fn persistence(info: &Info<'_>, snap: &mut ServerSnapshot) {
    if !info.has("Persistence") {
        return;
    }
    if let Some(status) = info.text("rdb_last_bgsave_status") {
        snap.push(ServerMetric::new(
            MetricGroup::Persistence,
            "rdb_last_bgsave_status",
            "Last RDB save",
            MetricValue::Text(status),
        ));
    }
    if let Some(changes) = info.num("rdb_changes_since_last_save") {
        snap.push(
            ServerMetric::new(
                MetricGroup::Persistence,
                "rdb_changes_since_last_save",
                "Unsaved changes",
                MetricValue::Count(changes),
            )
            .with_detail("writes since the last RDB snapshot"),
        );
    }
    // Only meaningful where the append-only file is on; `aof_enabled:0` servers
    // report a permanently "ok" write status that means nothing.
    if info.num("aof_enabled") == Some(1)
        && let Some(status) = info.text("aof_last_write_status")
    {
        snap.push(ServerMetric::new(
            MetricGroup::Persistence,
            "aof_last_write_status",
            "Last AOF write",
            MetricValue::Text(status),
        ));
    }
}

fn server(info: &Info<'_>, snap: &mut ServerSnapshot) {
    if !info.has("Server") {
        return;
    }
    if let Some(version) = info.text("redis_version") {
        let metric = ServerMetric::new(
            MetricGroup::Server,
            "redis_version",
            "Version",
            MetricValue::Text(version),
        );
        snap.push(match info.text("redis_mode") {
            Some(mode) => metric.with_detail(format!("mode: {mode}")),
            None => metric,
        });
    }
    if let Some(uptime) = info.num("uptime_in_seconds") {
        snap.push(ServerMetric::new(
            MetricGroup::Server,
            "uptime_in_seconds",
            "Uptime",
            MetricValue::Duration(Duration::from_secs(uptime)),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trimmed but structurally faithful `INFO` reply.
    const FULL: &str = "\
# Server
redis_version:7.2.4
redis_mode:standalone
uptime_in_seconds:93784

# Clients
connected_clients:12
maxclients:10000
blocked_clients:1

# Memory
used_memory:2097152
used_memory_rss:3145728
used_memory_peak:4194304
maxmemory:8388608
maxmemory_policy:allkeys-lru
mem_fragmentation_ratio:1.50

# Persistence
aof_enabled:0
rdb_changes_since_last_save:7
rdb_last_bgsave_status:ok
aof_last_write_status:ok

# Stats
total_commands_processed:5000
instantaneous_ops_per_sec:42
rejected_connections:3
expired_keys:11
evicted_keys:2
keyspace_hits:750
keyspace_misses:250

# Replication
role:master
connected_slaves:1

# Keyspace
db0:keys=100,expires=10,avg_ttl=0
db1:keys=5,expires=0,avg_ttl=0
";

    fn value(snap: &ServerSnapshot, key: &str) -> MetricValue {
        snap.get(key)
            .unwrap_or_else(|| panic!("metric {key} is missing"))
            .value
            .clone()
    }

    #[test]
    fn a_full_info_reply_leaves_nothing_unavailable() {
        let snap = parse_info(FULL, 1_700_000_000);
        assert_eq!(snap.engine, DbKind::Redis);
        assert_eq!(snap.taken_at, 1_700_000_000);
        assert!(
            snap.unavailable.is_empty(),
            "unexpected gaps: {:?}",
            snap.unavailable
        );
    }

    #[test]
    fn memory_reads_against_its_ceiling_with_the_policy_alongside() {
        let snap = parse_info(FULL, 0);
        assert_eq!(
            value(&snap, "used_memory"),
            MetricValue::Ratio {
                used: 2_097_152,
                total: 8_388_608
            }
        );
        assert_eq!(
            snap.get("used_memory").and_then(|m| m.detail.as_deref()),
            Some("eviction policy: allkeys-lru")
        );
        assert_eq!(
            value(&snap, "mem_fragmentation_ratio"),
            MetricValue::Factor(1.5)
        );
    }

    #[test]
    fn the_hit_rate_is_derived_from_hits_and_misses() {
        let snap = parse_info(FULL, 0);
        assert_eq!(
            value(&snap, "keyspace_hit_rate"),
            MetricValue::Percent(0.75)
        );
        assert_eq!(
            snap.get("keyspace_hit_rate")
                .and_then(|m| m.detail.as_deref()),
            Some("750 hits / 250 misses")
        );
    }

    #[test]
    fn keyspace_counts_sum_across_databases_and_keep_the_breakdown() {
        let snap = parse_info(FULL, 0);
        assert_eq!(value(&snap, "keys"), MetricValue::Count(105));
        assert_eq!(value(&snap, "expires"), MetricValue::Count(10));
        assert_eq!(
            snap.get("keys").and_then(|m| m.detail.as_deref()),
            Some("db0: 100 \u{b7} db1: 5")
        );
    }

    #[test]
    fn totals_and_gauges_keep_their_kinds() {
        let snap = parse_info(FULL, 0);
        // A cumulative counter, so the panel can derive a rate from two samples.
        assert_eq!(
            value(&snap, "total_commands_processed"),
            MetricValue::Total(5000)
        );
        // A gauge: 12 clients now, not 12 clients ever.
        assert_eq!(
            value(&snap, "connected_clients"),
            MetricValue::Ratio {
                used: 12,
                total: 10_000
            }
        );
        assert_eq!(
            value(&snap, "uptime_in_seconds"),
            MetricValue::Duration(Duration::from_secs(93_784))
        );
    }

    #[test]
    fn a_stripped_section_is_reported_rather_than_read_as_zero() {
        // What a managed provider does: `INFO` answers, minus the sections it
        // does not want exposed.
        let trimmed = "\
# Server
redis_version:7.2.4
uptime_in_seconds:10

# Clients
connected_clients:2
";
        let snap = parse_info(trimmed, 0);
        assert!(snap.get("used_memory").is_none());
        assert!(snap.get("keyspace_hit_rate").is_none());
        assert!(
            snap.unavailable
                .iter()
                .any(|u| u.contains("INFO Memory section")),
            "{:?}",
            snap.unavailable
        );
        assert!(
            snap.unavailable
                .iter()
                .any(|u| u.contains("INFO Replication section"))
        );
        // The sections that *were* returned still produce their metrics.
        assert_eq!(value(&snap, "connected_clients").render(), "2 (no limit)");
    }

    #[test]
    fn a_truncated_reply_yields_no_metrics_and_names_every_gap() {
        let snap = parse_info("# Server\nredis_ver", 0);
        assert!(snap.get("redis_version").is_none());
        assert_eq!(snap.unavailable.len(), SECTIONS.len() - 1);
    }

    #[test]
    fn an_empty_reply_is_not_an_error() {
        let snap = parse_info("", 0);
        assert!(snap.metrics.is_empty());
        assert_eq!(snap.unavailable.len(), SECTIONS.len());
    }

    #[test]
    fn a_replica_reports_its_link_and_lag_but_a_primary_does_not() {
        let replica = "\
# Replication
role:slave
master_link_status:up
master_last_io_seconds_ago:2
master_repl_offset:1000
slave_repl_offset:940
connected_slaves:0
";
        let snap = parse_info(replica, 0);
        assert_eq!(value(&snap, "role"), MetricValue::Text("slave".into()));
        assert_eq!(
            value(&snap, "master_link_status"),
            MetricValue::Text("up".into())
        );
        assert_eq!(value(&snap, "repl_offset_lag"), MetricValue::Bytes(60));

        let primary = parse_info(FULL, 0);
        assert!(primary.get("master_link_status").is_none());
        assert!(primary.get("repl_offset_lag").is_none());
    }

    #[test]
    fn aof_write_status_is_withheld_while_the_append_only_file_is_off() {
        // `aof_last_write_status` reads "ok" on a server with AOF disabled,
        // which would show a green persistence line for a file nobody writes.
        let snap = parse_info(FULL, 0);
        assert!(snap.get("aof_last_write_status").is_none());

        let with_aof = FULL.replace("aof_enabled:0", "aof_enabled:1");
        let snap = parse_info(&with_aof, 0);
        assert_eq!(
            value(&snap, "aof_last_write_status"),
            MetricValue::Text("ok".into())
        );
    }

    #[test]
    fn a_server_that_has_served_nothing_reports_no_hit_rate() {
        // 0% would read as a cache missing everything rather than an idle one.
        let idle = "# Stats\nkeyspace_hits:0\nkeyspace_misses:0\n";
        assert!(parse_info(idle, 0).get("keyspace_hit_rate").is_none());
    }

    #[test]
    fn a_non_numeric_field_is_skipped_rather_than_read_as_zero() {
        let odd = "# Memory\nused_memory:not-a-number\nused_memory_rss:1024\n";
        let snap = parse_info(odd, 0);
        assert!(snap.get("used_memory").is_none());
        assert_eq!(value(&snap, "used_memory_rss"), MetricValue::Bytes(1024));
    }
}
