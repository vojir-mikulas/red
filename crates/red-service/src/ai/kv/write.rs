//! Every Redis mutation, and the approval shape each one is shown under.
//!
//! The load-bearing pattern is that a write is **parsed once** into a plan
//! ([`KvSetPlan`](super::set::KvSetPlan), [`kv_delete_targets`]) that both the approval prompt and the
//! executor read, so what the user allows is exactly what runs -- the contract
//! the SQL path gets for free by showing the statement itself.
//!
//! The catastrophic shapes are refused outright rather than prompted: a
//! keyspace-wide `DEL`/`EXPIRE` is not something a rubber-stamped Allow should
//! be able to reach, and [`kv_command`](kv_allowed_command) exists only under a
//! hard verb allowlist because a general command tool would route around every
//! other gate in the seam.

use serde_json::Value as Json;

use super::super::gate::WriteAssessment;
use super::super::util::truncate_summary;
use super::format::resp_arg;
use super::json::kv_json_set_value;
use super::set::{KV_SET_PROMPT_CHARS, kv_set_plan};

/// The Redis mutating tools (KV backend): each rides the same per-call
/// approval gate as a SQL write.
pub(in crate::ai) const KV_WRITE_TOOLS: &[&str] = &[
    "kv_set",
    "kv_json_set",
    "kv_expire",
    "kv_delete",
    "kv_rename",
    "kv_copy_key",
    "kv_config_set",
    // Not a keyspace write, but a server-state one that rides the same gate.
    "kv_client_kill",
    // Read-only by allowlist, but classified as a write on purpose: this is the
    // one tool whose safety rests entirely on that allowlist, so the fail-closed
    // default (approval required, never advertised headlessly) protects it too.
    "kv_command",
];
/// The command verbs `kv_command` may run. Introspection only: nothing here
/// reads a key's value, writes anything, or reconfigures the server, so the tool
/// cannot be used to route around `kv_get_value`'s tier gate or any write gate.
const KV_COMMAND_ALLOWLIST: &[&str] = &[
    "INFO", "MEMORY", "OBJECT", "TYPE", "TTL", "PTTL", "EXISTS", "STRLEN", "LATENCY", "COMMAND",
    "DBSIZE", "LASTSAVE", "TIME", "ROLE",
];
/// The `argv` of a `kv_command` call: the non-empty string entries, in order.
pub(super) fn kv_command_argv(input: &Json) -> Vec<String> {
    input
        .get("argv")
        .and_then(Json::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Json::as_str)
                .map(str::trim)
                .filter(|a| !a.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}
/// Whether `argv` names a command `kv_command` may run. The verb must be on
/// [`KV_COMMAND_ALLOWLIST`], and there must be exactly one of them: Redis has no
/// in-command chaining, but refusing anything unexpected is the posture this
/// tool only exists under.
pub(super) fn kv_allowed_command(argv: &[String]) -> Result<(), String> {
    let Some(verb) = argv.first() else {
        return Err("kv_command needs a non-empty `argv`".into());
    };
    let upper = verb.to_ascii_uppercase();
    if !KV_COMMAND_ALLOWLIST.contains(&upper.as_str()) {
        return Err(format!(
            "`{verb}` is not on kv_command's allowlist ({}). Use the dedicated tool for what you \
             need; this one is introspection only.",
            KV_COMMAND_ALLOWLIST.join(", ")
        ));
    }
    Ok(())
}
pub(in crate::ai) fn is_kv_write_tool(name: &str) -> bool {
    KV_WRITE_TOOLS.contains(&name)
}
/// Whether a Redis glob is (near) keyspace-wide: it carries no literal anchoring
/// character, so it matches essentially every key. `*`, `**`, `?*`, and `[a-z]*`
/// all qualify; `user:*` does not (the `user:` literal anchors it). A destructive
/// write over such a pattern is refused even with approval, so the keyspace-wide
/// guard can't be evaded by an equivalent glob.
pub(super) fn matches_whole_keyspace(pattern: &str) -> bool {
    let mut chars = pattern.chars();
    let mut in_class = false;
    let mut has_literal = false;
    while let Some(c) = chars.next() {
        match c {
            // An escaped character is a literal anchor.
            '\\' => has_literal |= chars.next().is_some(),
            '[' if !in_class => in_class = true,
            ']' if in_class => in_class = false,
            // Class contents are a wildcard set, not an anchor.
            _ if in_class => {}
            // Wildcards provide no selectivity.
            '*' | '?' => {}
            _ => has_literal = true,
        }
    }
    !has_literal
}
/// Vet a Redis write tool for the approval gate: build the human-readable
/// operation shown in the Allow/Deny prompt, and hard-block the catastrophic
/// shapes (a keyspace-wide DELETE or EXPIRE) even with approval — mirroring the
/// SQL gate's refusal of an unqualified UPDATE/DELETE. Tier + read-only were
/// already checked by [`assess_write`](crate::ai::gate::assess_write).
/// The explicit key targets of a `kv_delete` input: `key` and `keys` combined,
/// in that order. The one accumulation both the approval prompt and the executor
/// use, so what the user approves is exactly what gets deleted.
pub(super) fn kv_delete_targets(input: &Json) -> Vec<String> {
    let mut targets: Vec<String> = Vec::new();
    if let Some(k) = input
        .get("key")
        .and_then(Json::as_str)
        .filter(|k| !k.is_empty())
    {
        targets.push(k.to_string());
    }
    if let Some(arr) = input.get("keys").and_then(Json::as_array) {
        targets.extend(arr.iter().filter_map(|v| v.as_str()).map(str::to_string));
    }
    targets
}
pub(in crate::ai) fn assess_kv_write(name: &str, input: &Json) -> WriteAssessment {
    let s = |k: &str| {
        input
            .get(k)
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };
    match name {
        "kv_expire" => {
            let seconds = input.get("seconds").and_then(Json::as_i64);
            let target = match (s("key"), s("pattern")) {
                (Some(k), _) => format!("key `{k}`"),
                (None, Some(p)) => {
                    if matches_whole_keyspace(p) && seconds.is_some_and(|sec| sec > 0) {
                        return WriteAssessment::Reject(format!(
                            "refusing to set a TTL on the entire keyspace (pattern `{p}` matches \
                             essentially every key): this would expire every key. Narrow the \
                             pattern."
                        ));
                    }
                    format!("all keys matching `{p}`")
                }
                (None, None) => {
                    return WriteAssessment::Reject("kv_expire needs `key` or `pattern`".into());
                }
            };
            let action = match seconds {
                Some(sec) if sec > 0 => format!("EXPIRE {target} in {sec}s"),
                _ => format!("PERSIST {target} (remove any expiry)"),
            };
            WriteAssessment::NeedsApproval { sql: action }
        }
        "kv_delete" => {
            // Built from the SAME accumulation the executor deletes
            // (`kv_delete_targets`): the input schema permits `key` and `keys`
            // simultaneously, so a prompt built from `key` alone would show one
            // key while everything in `keys` rides along unseen.
            let targets = kv_delete_targets(input);
            if let [one] = targets.as_slice() {
                WriteAssessment::NeedsApproval {
                    sql: format!("DELETE key `{one}`"),
                }
            } else if !targets.is_empty() {
                WriteAssessment::NeedsApproval {
                    sql: format!("DELETE {} key(s): {}", targets.len(), targets.join(", ")),
                }
            } else if let Some(p) = s("pattern") {
                if matches_whole_keyspace(p) {
                    return WriteAssessment::Reject(format!(
                        "refusing to DELETE the entire keyspace (pattern `{p}` matches essentially \
                         every key): use FLUSHDB by hand if that's really intended. Narrow the \
                         pattern."
                    ));
                }
                WriteAssessment::NeedsApproval {
                    sql: format!("DELETE all keys matching `{p}`"),
                }
            } else {
                WriteAssessment::Reject("kv_delete needs `key`, `keys`, or `pattern`".into())
            }
        }
        "kv_set" => match kv_set_plan(input) {
            // The prompt IS the command list: the user reads what will run, not a
            // paraphrase of it.
            Ok(plan) => WriteAssessment::NeedsApproval {
                sql: truncate_summary(&plan.commands().join("\n"), KV_SET_PROMPT_CHARS),
            },
            Err(why) => WriteAssessment::Reject(why),
        },
        "kv_json_set" => {
            let Some(key) = s("key") else {
                return WriteAssessment::Reject("kv_json_set needs `key`".into());
            };
            let Some(value) = kv_json_set_value(input) else {
                return WriteAssessment::Reject("kv_json_set needs `value`".into());
            };
            if let Err(e) = red_core::kv::validate_json(&value) {
                return WriteAssessment::Reject(format!("`value` is not valid JSON: {e}"));
            }
            let path = s("path").unwrap_or("$");
            // The prompt says whether the whole document is being replaced.
            // `JSON.SET key $` overwrites everything, including the parts of the
            // document nobody read, which is a different risk from writing a leaf.
            let scope = if path == "$" || path.is_empty() {
                "\n\u{26a0} Path `$` replaces the WHOLE document, not just one field."
            } else {
                ""
            };
            WriteAssessment::NeedsApproval {
                sql: format!(
                    "JSON.SET {} {path} {}{scope}",
                    resp_arg(key),
                    truncate_summary(&value, KV_SET_PROMPT_CHARS)
                ),
            }
        }
        "kv_copy_key" => match (s("from"), s("to")) {
            (Some(f), Some(t)) => {
                let replace = input
                    .get("replace")
                    .and_then(Json::as_bool)
                    .unwrap_or(false);
                let clobber = if replace {
                    format!(", OVERWRITING `{t}` if it already exists")
                } else {
                    format!(", refusing if `{t}` already exists")
                };
                WriteAssessment::NeedsApproval {
                    sql: format!("COPY key `{f}` to `{t}` (DUMP + RESTORE){clobber}"),
                }
            }
            _ => WriteAssessment::Reject("kv_copy_key needs `from` and `to`".into()),
        },
        "kv_command" => {
            let argv = kv_command_argv(input);
            match kv_allowed_command(&argv) {
                Ok(()) => WriteAssessment::NeedsApproval {
                    sql: argv
                        .iter()
                        .map(|a| resp_arg(a))
                        .collect::<Vec<_>>()
                        .join(" "),
                },
                Err(why) => WriteAssessment::Reject(why),
            }
        }
        "kv_client_kill" => match input.get("id").and_then(Json::as_i64) {
            Some(id) => {
                let mut op = format!("CLIENT KILL ID {id}");
                if let Some(addr) = s("addr") {
                    op.push_str(&format!(" ({addr})"));
                }
                match s("cmd") {
                    Some(cmd) => op.push_str(&format!("\nLast command: {cmd}")),
                    None => op.push_str(
                        "\nThe agent did not say what this client is doing; read kv_client_list \
                         before allowing.",
                    ),
                }
                WriteAssessment::NeedsApproval { sql: op }
            }
            None => WriteAssessment::Reject(
                "kv_client_kill needs the numeric `id` of a client from kv_client_list".into(),
            ),
        },
        "kv_rename" => match (s("from"), s("to")) {
            (Some(f), Some(t)) => WriteAssessment::NeedsApproval {
                sql: format!("RENAME `{f}` -> `{t}`"),
            },
            _ => WriteAssessment::Reject("kv_rename needs `from` and `to`".into()),
        },
        "kv_config_set" => match (s("parameter"), input.get("value").and_then(Json::as_str)) {
            // A CONFIG value may legitimately be empty (e.g. `save ""`), so `value`
            // isn't filtered for emptiness like the others.
            (Some(p), Some(v)) => {
                let mut op = format!("CONFIG SET {p} {v}");
                // Surface a danger note in the approval prompt for parameters
                // that relocate/toggle persistence or change auth: a single
                // Allow on `CONFIG SET dir` + `dbfilename` is the classic
                // RDB-write server-takeover, indistinguishable in the raw
                // command from a benign `maxmemory-policy` tweak.
                if is_dangerous_config_param(p) {
                    op.push_str(
                        "\n\u{26a0} This parameter can change where/how Redis persists data or \
                         its auth, and is a known server-takeover vector. Allow only if this \
                         exact value was intended.",
                    );
                }
                WriteAssessment::NeedsApproval { sql: op }
            }
            _ => WriteAssessment::Reject("kv_config_set needs `parameter` and `value`".into()),
        },
        other => WriteAssessment::Reject(format!("unknown KV write tool `{other}`")),
    }
}
/// CONFIG parameters that can relocate or toggle Redis persistence (the
/// `CONFIG SET dir` + `dbfilename` RDB-write takeover and its AOF equivalents)
/// or change authentication/exposure. A `CONFIG SET` of one of these gets a
/// danger note in the approval prompt so it can't be waved through as routine.
fn is_dangerous_config_param(param: &str) -> bool {
    const DANGEROUS: &[&str] = &[
        "dir",
        "dbfilename",
        "appendfilename",
        "appenddirname",
        "appendonly",
        "save",
        "requirepass",
        "masterauth",
        "masteruser",
        "aclfile",
        "unixsocket",
        "logfile",
        "pidfile",
        "bind",
        "protected-mode",
        "enable-debug-command",
        "enable-module-command",
    ];
    DANGEROUS.contains(&param.trim().to_ascii_lowercase().as_str())
}
/// CONFIG parameters whose *value* is a credential and must never be echoed back
/// to the model (a `CONFIG GET requirepass` exfiltration vector). Broader than an
/// exact list so a `*pass*`-shaped parameter can't slip a secret through.
pub(super) fn is_secret_config_param(param: &str) -> bool {
    let p = param.trim().to_ascii_lowercase();
    matches!(p.as_str(), "requirepass" | "masterauth" | "masteruser") || p.contains("pass")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::gate::{is_headless_tool, is_write_tool};
    use red_core::kv::KvModules;

    use crate::ai::kv::catalog::kv_tool_catalog;
    use crate::ai::testutil::assess_write;
    use crate::ai::turn::kv_subagent_catalog;
    use red_core::{AiPolicy, AiTier};
    use serde_json::json;

    #[test]
    fn whole_keyspace_glob_catches_wildcard_equivalents() {
        // No literal anchor: matches essentially every key, so refused.
        for p in ["*", "**", "?*", "*?", "[a-z]*", "?", "[^a]"] {
            assert!(matches_whole_keyspace(p), "{p} should be whole-keyspace");
        }
        // A literal anchor narrows it: allowed (rides the normal approval gate).
        for p in ["user:*", "a*", "*:cache", "[a-z]_role", "\\*", "session:?"] {
            assert!(
                !matches_whole_keyspace(p),
                "{p} should NOT be whole-keyspace"
            );
        }
    }

    #[test]
    fn kv_delete_star_equivalents_are_rejected_not_prompted() {
        // The literal `*` and its glob-equivalents are hard-refused, never a
        // NeedsApproval the user could wave through.
        for p in ["*", "?*", "[a-z]*"] {
            assert!(
                matches!(
                    assess_kv_write("kv_delete", &json!({ "pattern": p })),
                    WriteAssessment::Reject(_)
                ),
                "kv_delete pattern `{p}` must be rejected"
            );
        }
        // A scoped pattern still prompts.
        assert!(matches!(
            assess_kv_write("kv_delete", &json!({ "pattern": "user:*" })),
            WriteAssessment::NeedsApproval { .. }
        ));
    }

    /// The approval prompt must show the exact payload that runs, and must say
    /// when a write replaces the whole document rather than one field.
    #[test]
    fn kv_json_set_prompts_with_the_payload_and_flags_a_root_write() {
        assert!(is_write_tool("kv_json_set"));
        assert!(!AiTier::Read.allows_tool("kv_json_set"));
        assert!(AiTier::Write.allows_tool("kv_json_set"));

        let leaf = assess_kv_write(
            "kv_json_set",
            &json!({ "key": "cfg", "path": "$.status", "value": "ok" }),
        );
        let WriteAssessment::NeedsApproval { sql } = leaf else {
            panic!("a scoped JSON write should prompt");
        };
        assert!(sql.contains("$.status"), "got {sql}");
        // A bare string is written as a JSON string, and the prompt shows that.
        assert!(sql.contains("\"ok\""), "got {sql}");
        assert!(
            !sql.contains('\u{26a0}'),
            "a leaf write is not a whole-document one"
        );

        let WriteAssessment::NeedsApproval { sql } =
            assess_kv_write("kv_json_set", &json!({ "key": "cfg", "value": { "a": 1 } }))
        else {
            panic!("a root JSON write should prompt");
        };
        assert!(sql.contains("WHOLE document"), "got {sql}");

        // Malformed JSON is refused before the prompt, not at the server.
        assert!(matches!(
            assess_kv_write("kv_json_set", &json!({ "key": "cfg", "value": "{\"a\":}" })),
            WriteAssessment::Reject(_)
        ));
        assert!(matches!(
            assess_kv_write("kv_json_set", &json!({ "key": "cfg" })),
            WriteAssessment::Reject(_)
        ));
    }

    #[test]
    fn kv_set_is_a_gated_write_at_write_tier_only() {
        assert!(is_write_tool("kv_set"));
        assert!(!AiTier::Read.allows_tool("kv_set"));
        assert!(AiTier::Write.allows_tool("kv_set"));
        let names = |p: AiPolicy| {
            kv_tool_catalog(&p, &KvModules::NONE)
                .into_iter()
                .map(|t| t.name)
                .collect::<Vec<_>>()
        };
        assert!(names(AiPolicy::default()).iter().all(|n| n != "kv_set"));
        let write = AiPolicy {
            tier: AiTier::Write,
            ..AiPolicy::default()
        };
        assert!(names(write).iter().any(|n| n == "kv_set"));
        // A read-only connection withholds it, and a subagent never gets it.
        let write_ro = AiPolicy {
            tier: AiTier::Write,
            read_only: true,
            ..AiPolicy::default()
        };
        assert!(names(write_ro).iter().all(|n| n != "kv_set"));
        assert!(
            kv_subagent_catalog(
                &AiPolicy {
                    tier: AiTier::Write,
                    ..AiPolicy::default()
                },
                &KvModules::NONE,
            )
            .iter()
            .all(|t| t.name != "kv_set")
        );
        // Never advertised over the headless transport either (it is a write).
        assert!(!is_headless_tool("kv_set"));
    }

    #[test]
    fn config_get_secrets_are_redacted() {
        for p in ["requirepass", "masterauth", "masteruser", "primarypass"] {
            assert!(is_secret_config_param(p), "{p} should be secret");
        }
        for p in ["maxmemory", "dir", "save"] {
            assert!(!is_secret_config_param(p), "{p} should not be secret");
        }
    }

    #[test]
    fn kv_write_gate_prompts_shapes_and_refuses_keyspace_wide() {
        let write = AiPolicy {
            tier: AiTier::Write,
            ..AiPolicy::default()
        };
        // A single-key or scoped-pattern op prompts for approval.
        assert!(matches!(
            assess_write("kv_delete", &json!({ "key": "user:1" }), &write),
            WriteAssessment::NeedsApproval { .. }
        ));
        assert!(matches!(
            assess_write("kv_delete", &json!({ "pattern": "session:*" }), &write),
            WriteAssessment::NeedsApproval { .. }
        ));
        assert!(matches!(
            assess_write("kv_expire", &json!({ "key": "k", "seconds": 60 }), &write),
            WriteAssessment::NeedsApproval { .. }
        ));
        assert!(matches!(
            assess_write("kv_rename", &json!({ "from": "a", "to": "b" }), &write),
            WriteAssessment::NeedsApproval { .. }
        ));
        // `key` and `keys` passed together: the prompt must name every target
        // the executor will delete, not just `key` — the drift that let 50k
        // keys ride behind a one-key approval.
        match assess_write(
            "kv_delete",
            &json!({ "key": "scratch:1", "keys": ["a", "b", "c"] }),
            &write,
        ) {
            WriteAssessment::NeedsApproval { sql } => {
                for k in ["scratch:1", "a", "b", "c"] {
                    assert!(sql.contains(k), "prompt must name `{k}`: {sql}");
                }
                assert!(sql.contains("4 key(s)"), "{sql}");
            }
            WriteAssessment::Reject(why) => panic!("expected NeedsApproval, got Reject({why})"),
            WriteAssessment::NotWrite => panic!("expected NeedsApproval, got NotWrite"),
        }
        // Keyspace-wide delete/expire is refused outright, even at Write tier.
        assert!(matches!(
            assess_write("kv_delete", &json!({ "pattern": "*" }), &write),
            WriteAssessment::Reject(_)
        ));
        assert!(matches!(
            assess_write(
                "kv_expire",
                &json!({ "pattern": "*", "seconds": 60 }),
                &write
            ),
            WriteAssessment::Reject(_)
        ));
        // The write tools are rejected below Write tier and on a read-only conn.
        assert!(matches!(
            assess_write("kv_delete", &json!({ "key": "k" }), &AiPolicy::default()),
            WriteAssessment::Reject(_)
        ));
        let write_ro = AiPolicy {
            tier: AiTier::Write,
            read_only: true,
            ..AiPolicy::default()
        };
        assert!(matches!(
            assess_write("kv_delete", &json!({ "key": "k" }), &write_ro),
            WriteAssessment::Reject(_)
        ));
    }

    #[test]
    fn kv_command_runs_only_allowlisted_verbs() {
        let write = AiPolicy {
            tier: AiTier::Write,
            ..AiPolicy::default()
        };
        // An allowlisted verb prompts with the exact command.
        match assess_write(
            "kv_command",
            &json!({ "argv": ["MEMORY", "DOCTOR"] }),
            &write,
        ) {
            WriteAssessment::NeedsApproval { sql } => assert_eq!(sql, "MEMORY DOCTOR"),
            _ => panic!("an allowlisted verb must prompt"),
        }
        // Everything else is refused, including the reads it could otherwise use
        // to route around kv_get_value's tier gate and any write gate.
        for argv in [
            json!(["GET", "user:1"]),
            json!(["SET", "user:1", "x"]),
            json!(["FLUSHALL"]),
            json!(["CONFIG", "SET", "dir", "/tmp"]),
            json!(["EVAL", "return 1", "0"]),
            json!([]),
        ] {
            assert!(
                matches!(
                    assess_write("kv_command", &json!({ "argv": argv }), &write),
                    WriteAssessment::Reject(_)
                ),
                "kv_command {argv} must be refused"
            );
        }
    }
}
