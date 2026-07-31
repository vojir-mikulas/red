//! Which Redis Stack modules a server has loaded, read once at connect.
//!
//! The point of this descriptor is to never offer an affordance the server
//! cannot answer: a `JSON.*` command sent to a plain Redis comes back
//! "unknown command", which reads to a user as RED being broken rather than the
//! server lacking a module. It gates the *offers* (the type-filter entry, the
//! new-key type, the agent's JSON tools), not the reads -- a key whose `TYPE` is
//! already `ReJSON-RL` is itself proof the module is loaded, so reading one
//! never consults this.
//!
//! Constructed only by parsing a probe reply, so the derived flags cannot drift
//! from the module list they came from.

use super::RespValue;

/// The wire name RedisJSON reports through `MODULE LIST`.
const REJSON: &str = "ReJSON";

/// Lowest RedisJSON version whose path parser understands the `$`-rooted
/// JSONPath syntax every path RED builds uses (2.0, encoded `2_00_00`). An
/// older module is reported present but not usable, rather than being probed by
/// sending a command and reading the error.
const REJSON_MIN_VER: u32 = 20_000;

/// One loaded module, as `MODULE LIST` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KvModule {
    pub name: String,
    /// The module's own version integer (`2.6.8` reports as `20608`); `0` when
    /// the server did not report one.
    pub version: u32,
}

/// The Redis Stack modules a connection's server has loaded.
///
/// Fields are private and the only constructors parse a probe reply, so
/// [`json`](Self::json) can never disagree with [`loaded`](Self::loaded): the
/// two legitimately differ (a managed provider may restrict `MODULE LIST` while
/// still running the module, which [`with_json_probe`](Self::with_json_probe)
/// covers) and the parse is what reconciles them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KvModules {
    loaded: Vec<KvModule>,
    json: bool,
}

impl KvModules {
    /// No modules known: a plain Redis, or a server that refused every probe.
    /// Never an error -- a missing module list must not fail a connect.
    pub const NONE: KvModules = KvModules {
        loaded: Vec::new(),
        json: false,
    };

    /// Parse a `MODULE LIST` reply: an array of per-module flat `field, value`
    /// maps (`["name", "ReJSON", "ver", 20608, ...]`). Tolerant by design -- an
    /// error reply, an unexpected shape, or a row without a name yields no
    /// module rather than a failure, because the fallback for "we don't know" is
    /// "offer nothing extra", not "refuse to connect".
    pub fn from_module_list(reply: &RespValue) -> KvModules {
        let RespValue::Array(rows) = reply else {
            return KvModules::NONE;
        };
        let mut loaded = Vec::with_capacity(rows.len());
        for row in rows {
            let RespValue::Array(fields) = row else {
                continue;
            };
            let mut name = None;
            let mut version = 0;
            for pair in fields.chunks_exact(2) {
                match pair[0].as_text() {
                    Some("name") => name = pair[1].as_text().map(str::to_string),
                    Some("ver") => version = pair[1].as_int().unwrap_or(0).max(0) as u32,
                    _ => {}
                }
            }
            if let Some(name) = name.filter(|n| !n.is_empty()) {
                loaded.push(KvModule { name, version });
            }
        }
        let json = loaded.iter().any(usable_json);
        KvModules { loaded, json }
    }

    /// Record a direct `COMMAND INFO JSON.GET`-style probe, for a server whose
    /// `MODULE LIST` was refused. `false` never clears a module the list already
    /// named: the list is the stronger evidence, and this only ever adds.
    pub fn with_json_probe(mut self, present: bool) -> KvModules {
        self.json |= present;
        self
    }

    /// Whether RedisJSON is loaded *and* new enough for `$`-rooted paths.
    pub fn json(&self) -> bool {
        self.json
    }

    /// Every module the server reported, for display.
    pub fn loaded(&self) -> &[KvModule] {
        &self.loaded
    }
}

/// Whether a listed module is a RedisJSON RED can actually drive. A `0` version
/// means the server did not report one, which is accepted rather than assumed
/// old -- refusing on a missing field would disable the feature on any server
/// that reports its modules differently.
fn usable_json(m: &KvModule) -> bool {
    m.name.eq_ignore_ascii_case(REJSON) && (m.version == 0 || m.version >= REJSON_MIN_VER)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bulk(s: &str) -> RespValue {
        RespValue::Bulk(s.to_string())
    }

    fn module_row(name: &str, ver: i64) -> RespValue {
        RespValue::Array(vec![
            bulk("name"),
            bulk(name),
            bulk("ver"),
            RespValue::Int(ver),
        ])
    }

    #[test]
    fn module_list_reports_json_when_the_module_is_loaded() {
        let reply = RespValue::Array(vec![
            module_row("ReJSON", 20_608),
            module_row("search", 21_007),
        ]);
        let m = KvModules::from_module_list(&reply);
        assert!(m.json());
        assert_eq!(m.loaded().len(), 2);
        assert_eq!(m.loaded()[0].name, "ReJSON");
        assert_eq!(m.loaded()[0].version, 20_608);
    }

    #[test]
    fn module_list_handles_the_empty_and_error_replies() {
        // A server with no modules.
        let empty = KvModules::from_module_list(&RespValue::Array(Vec::new()));
        assert!(!empty.json());
        assert!(empty.loaded().is_empty());
        // A refusal (NOPERM/unknown command) is "nothing known", not a failure.
        let refused = KvModules::from_module_list(&RespValue::Error("NOPERM".into()));
        assert_eq!(refused, KvModules::NONE);
        // A torn row without a name contributes nothing.
        let torn = KvModules::from_module_list(&RespValue::Array(vec![RespValue::Array(vec![
            bulk("ver"),
            RespValue::Int(1),
        ])]));
        assert!(torn.loaded().is_empty());
    }

    /// `$`-rooted paths are RedisJSON 2.0+; an older module is listed but not
    /// driven, rather than found out by sending a command and reading the error.
    #[test]
    fn module_list_refuses_a_prehistoric_rejson_but_accepts_an_unreported_version() {
        let old =
            KvModules::from_module_list(&RespValue::Array(vec![module_row("ReJSON", 10_007)]));
        assert!(!old.json());
        assert_eq!(old.loaded().len(), 1);
        let unknown_ver =
            KvModules::from_module_list(&RespValue::Array(vec![RespValue::Array(vec![
                bulk("name"),
                bulk("rejson"),
            ])]));
        assert!(
            unknown_ver.json(),
            "a missing `ver` must not disable the feature"
        );
    }

    /// A managed provider can restrict `MODULE LIST` while still running the
    /// module; the direct command probe is what recovers that case.
    #[test]
    fn json_probe_adds_but_never_clears() {
        assert!(KvModules::NONE.with_json_probe(true).json());
        assert!(!KvModules::NONE.with_json_probe(false).json());
        let listed =
            KvModules::from_module_list(&RespValue::Array(vec![module_row("ReJSON", 20_608)]));
        assert!(listed.with_json_probe(false).json());
    }
}
