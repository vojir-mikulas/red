//! The `kv_set` plan: one parse, read by both the approval prompt and the
//! executor.
//!
//! This is where "what you approve is what runs" is actually enforced on the
//! Redis seam. [`kv_set_plan`] validates a call once into a [`KvSetPlan`];
//! [`KvSetPlan::commands`] renders that plan as the commands `redis-cli` would
//! echo, and [`kv_apply_set`] walks the same plan through the typed `KvDriver`
//! writers. Neither side can drift, because neither re-reads the raw input.
//!
//! The payload guards live here too: a multi-megabyte value in a tool call is a
//! context problem before it is a Redis one, so it is refused at the gate rather
//! than streamed to the server.

use std::sync::Arc;
use std::time::Duration;

use red_core::RedError;
use red_core::kv::StringTtl;
use red_driver::KvDriver;
use serde_json::Value as Json;

use super::format::resp_arg;
use super::write::matches_whole_keyspace;

/// Ceiling on one `kv_set` value (a string body, a hash/stream field value, a
/// set/list member). A multi-megabyte value in a tool call is a context problem
/// before it is a Redis one — it has already crossed the wire to the provider and
/// back — so refuse it here and tell the model to write it by hand.
const KV_SET_VALUE_MAX: usize = 64 * 1024;

/// Ceiling on all of one `kv_set` call's values combined.
const KV_SET_PAYLOAD_MAX: usize = 256 * 1024;

/// Ceiling on the elements (hash fields, set/zset members, list values, stream
/// fields) one `kv_set` call may write. The per-element writers are one round
/// trip each, so this bounds the call's cost as well as the prompt's length.
const KV_SET_ELEMS_MAX: usize = 1_000;

/// How much of a `kv_set` command list the approval prompt shows before it is
/// truncated. Long enough for a realistic collection write, short enough that a
/// thousand-member payload cannot push the key name off the top of the dialog.
pub(super) const KV_SET_PROMPT_CHARS: usize = 800;

/// The value shape a [`KvSetPlan`] writes, one variant per Redis type. Each maps
/// to the `KvDriver` writer for that type, so an unwritable combination (a score
/// on a plain set, a field on a list) cannot be represented.
enum KvSetBody {
    /// `SET`, whose own expiry option replaces a separate `PEXPIRE`.
    Str { value: String, ttl: StringTtl },
    /// `HSET` per field.
    Hash(Vec<(String, String)>),
    /// One `SADD` with every member.
    Set(Vec<String>),
    /// `ZADD` per `(member, score)`.
    ZSet(Vec<(String, f64)>),
    /// `RPUSH` per value.
    List(Vec<String>),
    /// One `XADD … *` with the entry's fields.
    Stream(Vec<(String, String)>),
}

/// One parsed, validated `kv_set` call: the typed `KvDriver` writes it will
/// perform, in order.
///
/// Built once by [`kv_set_plan`] and used by *both* the approval prompt (which
/// renders [`KvSetPlan::commands`]) and the executor, so what the user allows is
/// exactly what runs — the contract the SQL path gets for free by showing the
/// statement itself, and the one `kv_delete`'s shared target list restored here.
pub(super) struct KvSetPlan {
    key: String,
    /// `DEL` the key before writing. Redis has no "replace this whole
    /// collection" primitive, so `mode: "set"` over a hash/set/zset/list is a
    /// delete and a rebuild; it is rendered in the prompt rather than implied.
    clear_first: bool,
    body: KvSetBody,
    /// Applied after the body as a `PEXPIRE`. The string form carries its expiry
    /// inside `SET` instead, so this stays `None` there.
    expire: Option<Duration>,
}

impl KvSetPlan {
    /// The commands this plan runs, in order, as `redis-cli` would echo them.
    /// The approval prompt's body and the executor's script are the same list.
    pub(super) fn commands(&self) -> Vec<String> {
        let key = resp_arg(&self.key);
        let mut out = Vec::new();
        if self.clear_first {
            out.push(format!("DEL {key}"));
        }
        match &self.body {
            KvSetBody::Str { value, ttl } => {
                let mut cmd = format!("SET {key} {}", resp_arg(value));
                if let StringTtl::Set(d) = ttl {
                    cmd.push_str(&format!(" PX {}", kv_millis(*d)));
                }
                out.push(cmd);
            }
            KvSetBody::Hash(fields) => out.extend(
                fields
                    .iter()
                    .map(|(f, v)| format!("HSET {key} {} {}", resp_arg(f), resp_arg(v))),
            ),
            KvSetBody::Set(members) => {
                let args: Vec<String> = members.iter().map(|m| resp_arg(m)).collect();
                out.push(format!("SADD {key} {}", args.join(" ")));
            }
            KvSetBody::ZSet(members) => out.extend(
                members
                    .iter()
                    .map(|(m, score)| format!("ZADD {key} {score} {}", resp_arg(m))),
            ),
            KvSetBody::List(values) => out.extend(
                values
                    .iter()
                    .map(|v| format!("RPUSH {key} {}", resp_arg(v))),
            ),
            KvSetBody::Stream(fields) => {
                let args: Vec<String> = fields
                    .iter()
                    .flat_map(|(f, v)| [resp_arg(f), resp_arg(v)])
                    .collect();
                out.push(format!("XADD {key} * {}", args.join(" ")));
            }
        }
        if let Some(d) = self.expire {
            out.push(format!("PEXPIRE {key} {}", kv_millis(d)));
        }
        out
    }
}

/// A `Duration` as the whole milliseconds the `PX`/`PEXPIRE` writers send. Both
/// clamp to at least 1, since a zero-millisecond expiry deletes the key on
/// arrival, which is never what a `ttl_seconds` of a fraction meant.
fn kv_millis(d: Duration) -> u64 {
    (d.as_millis() as u64).max(1)
}

/// Coerce one JSON scalar to the string Redis stores. Numbers and booleans are
/// accepted (a model writing `{"value": 42}` means the string `"42"`); a nested
/// object or array is not, because silently serializing one to JSON would write
/// a shape the user never approved reading back as a document.
fn kv_scalar(v: &Json, what: &str) -> Result<String, String> {
    match v {
        Json::String(s) => Ok(s.clone()),
        Json::Number(n) => Ok(n.to_string()),
        Json::Bool(b) => Ok(b.to_string()),
        _ => Err(format!(
            "{what} must be a string, number, or boolean; Redis stores bytes, so encode a \
             document yourself if that is what you mean"
        )),
    }
}

/// Parse and validate a `kv_set` call into the plan both the prompt and the
/// executor use. Every refusal here is a `Reject` the model can act on, never a
/// prompt: an oversized payload or an unwritable shape is not something a user
/// should be asked to rubber-stamp.
pub(super) fn kv_set_plan(input: &Json) -> Result<KvSetPlan, String> {
    let key = input
        .get("key")
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .ok_or("kv_set needs a non-empty `key`")?
        .to_string();
    // A key with no literal character is a glob the model mistook for a key name.
    // Writing it would create a key literally called `*`, which is worse than the
    // error: it collides with every pattern the user later types.
    if matches_whole_keyspace(&key) {
        return Err(format!(
            "`{key}` is a glob pattern, not a key: kv_set writes one exact key. Scan first if you \
             meant to find keys."
        ));
    }
    let kind = input
        .get("type")
        .and_then(Json::as_str)
        .map(str::trim)
        .unwrap_or("");
    let value = input.get("value");
    let mode_replace = match input.get("mode").and_then(Json::as_str).unwrap_or("set") {
        "set" => true,
        "append" => false,
        other => {
            return Err(format!(
                "kv_set `mode` must be \"set\" or \"append\", not `{other}`"
            ));
        }
    };
    let ttl = match input.get("ttl_seconds").and_then(Json::as_i64) {
        Some(s) if s > 0 => Some(Duration::from_secs(s as u64)),
        _ => None,
    };

    // `{ field: value }` pairs from a JSON object, in the order the model wrote
    // them (serde_json preserves insertion order), so the prompt reads the way
    // the call did.
    let pairs = |what: &str| -> Result<Vec<(String, String)>, String> {
        let obj = value.and_then(Json::as_object).ok_or_else(|| {
            format!("kv_set on a {kind} needs `value` as a JSON object of {what}")
        })?;
        obj.iter()
            .map(|(k, v)| Ok((k.clone(), kv_scalar(v, "each value")?)))
            .collect()
    };
    // A list of scalars from a JSON array, or a lone scalar treated as a
    // one-element list (the shape a model reaches for when adding one member).
    let members = || -> Result<Vec<String>, String> {
        match value {
            Some(Json::Array(items)) => items
                .iter()
                .map(|v| kv_scalar(v, "each member"))
                .collect::<Result<Vec<_>, _>>(),
            Some(v) => Ok(vec![kv_scalar(v, "`value`")?]),
            None => Err(format!("kv_set on a {kind} needs `value`")),
        }
    };

    let (body, clear_first) = match kind {
        "string" => {
            let v = value.ok_or("kv_set on a string needs `value`")?;
            let body = KvSetBody::Str {
                value: kv_scalar(v, "`value`")?,
                // `SET` carries its own expiry; a plain `SET` clears any existing
                // one, which is Redis's own semantics and is what the rendered
                // command says.
                ttl: ttl.map_or(StringTtl::Clear, StringTtl::Set),
            };
            (body, false)
        }
        "hash" => match input
            .get("field")
            .and_then(Json::as_str)
            .filter(|f| !f.is_empty())
        {
            // The single-field form is an upsert of that one field, so it never
            // clears the rest of the hash regardless of `mode`.
            Some(field) => {
                let v = value.ok_or("kv_set with a `field` needs `value`")?;
                (
                    KvSetBody::Hash(vec![(field.to_string(), kv_scalar(v, "`value`")?)]),
                    false,
                )
            }
            None => (KvSetBody::Hash(pairs("field/value")?), mode_replace),
        },
        "set" => (KvSetBody::Set(members()?), mode_replace),
        "zset" => {
            let obj = value.and_then(Json::as_object).ok_or(
                "kv_set on a zset needs `value` as a JSON object of member/score, e.g. \
                 { \"ada\": 1.5 }",
            )?;
            let scored = obj
                .iter()
                .map(|(m, s)| {
                    s.as_f64()
                        .filter(|f| f.is_finite())
                        .map(|score| (m.clone(), score))
                        .ok_or_else(|| format!("zset score for `{m}` must be a finite number"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            (KvSetBody::ZSet(scored), mode_replace)
        }
        "list" => (KvSetBody::List(members()?), mode_replace),
        // A stream is an append-only log and `value` describes ONE entry, so
        // there is no "the stream is these entries" shape to replace with;
        // `mode` is deliberately inert here rather than silently wiping a log.
        "stream" => (KvSetBody::Stream(pairs("field/value")?), false),
        "" => return Err("kv_set needs `type` (string/hash/set/zset/list/stream)".into()),
        other => {
            return Err(format!(
                "kv_set `type` must be string/hash/set/zset/list/stream, not `{other}`"
            ));
        }
    };

    let size = kv_set_size(&body);
    if size.elems == 0 {
        return Err(format!("kv_set on a {kind} needs at least one value"));
    }
    if size.elems > KV_SET_ELEMS_MAX {
        return Err(format!(
            "kv_set is capped at {KV_SET_ELEMS_MAX} elements per call ({} given); split it",
            size.elems
        ));
    }
    if size.largest > KV_SET_VALUE_MAX {
        return Err(format!(
            "a single kv_set value is {} bytes, over the {KV_SET_VALUE_MAX}-byte cap; write a \
             value this large by hand",
            size.largest
        ));
    }
    if size.bytes > KV_SET_PAYLOAD_MAX {
        return Err(format!(
            "kv_set payload is {} bytes, over the {KV_SET_PAYLOAD_MAX}-byte cap; split it",
            size.bytes
        ));
    }
    Ok(KvSetPlan {
        key,
        clear_first,
        // The string form folds its expiry into `SET`; every other type needs the
        // separate `PEXPIRE`.
        expire: match body {
            KvSetBody::Str { .. } => None,
            _ => ttl,
        },
        body,
    })
}

/// What the payload guards measure: the values a `kv_set` call writes, not the
/// rendered command text. `largest` is kept alongside `bytes` because one 10 MB
/// string and ten thousand small members are different problems with different
/// advice.
struct KvSetSize {
    elems: usize,
    bytes: usize,
    largest: usize,
}

fn kv_set_size(body: &KvSetBody) -> KvSetSize {
    let measure = |lengths: &[usize]| KvSetSize {
        elems: lengths.len(),
        bytes: lengths.iter().sum(),
        largest: lengths.iter().copied().max().unwrap_or(0),
    };
    let pairs = |v: &[(String, String)]| {
        measure(&v.iter().map(|(f, s)| f.len() + s.len()).collect::<Vec<_>>())
    };
    match body {
        KvSetBody::Str { value, .. } => measure(&[value.len()]),
        KvSetBody::Hash(f) | KvSetBody::Stream(f) => pairs(f),
        KvSetBody::Set(m) | KvSetBody::List(m) => {
            measure(&m.iter().map(String::len).collect::<Vec<_>>())
        }
        KvSetBody::ZSet(m) => measure(&m.iter().map(|(s, _)| s.len()).collect::<Vec<_>>()),
    }
}

/// Run an approved [`KvSetPlan`] against the driver, one typed write per element.
/// A mid-plan failure reports how far it got: Redis has no transaction here, so
/// claiming nothing happened would be a lie.
pub(super) async fn kv_apply_set(driver: &Arc<dyn KvDriver>, plan: &KvSetPlan) -> (String, bool) {
    let key = plan.key.as_str();
    if plan.clear_first
        && let Err(e) = driver.delete_keys(std::slice::from_ref(&plan.key)).await
    {
        return (format!("error: clearing `{key}` failed: {e}"), false);
    }
    let mut done = 0usize;
    let failed =
        |n: usize, e: RedError| (format!("error: after {n} write(s) to `{key}`: {e}"), false);
    match &plan.body {
        KvSetBody::Str { value, ttl } => {
            if let Err(e) = driver.set_string(key, value.clone(), *ttl).await {
                return failed(0, e);
            }
            done = 1;
        }
        KvSetBody::Hash(fields) => {
            for (field, value) in fields {
                if let Err(e) = driver.set_field(key, field, value.clone()).await {
                    return failed(done, e);
                }
                done += 1;
            }
        }
        KvSetBody::Set(members) => match driver.set_add(key, members).await {
            Ok(_) => done = members.len(),
            Err(e) => return failed(0, e),
        },
        KvSetBody::ZSet(members) => {
            for (member, score) in members {
                if let Err(e) = driver.zset_add(key, member, *score).await {
                    return failed(done, e);
                }
                done += 1;
            }
        }
        KvSetBody::List(values) => {
            for value in values {
                if let Err(e) = driver.list_push(key, value.clone(), false).await {
                    return failed(done, e);
                }
                done += 1;
            }
        }
        KvSetBody::Stream(fields) => match driver.stream_add(key, fields).await {
            Ok(_) => done = fields.len(),
            Err(e) => return failed(0, e),
        },
    }
    if let Some(d) = plan.expire
        && let Err(e) = driver.set_ttl(key, Some(d)).await
    {
        return (
            format!("error: wrote `{key}` but setting its expiry failed: {e}"),
            false,
        );
    }
    (format!("Wrote `{key}` ({done} element(s))."), true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::gate::WriteAssessment;
    use crate::ai::kv::write::assess_kv_write;
    use serde_json::json;

    /// The approval prompt must be the command list itself, not a paraphrase:
    /// the user allows `HSET user:1 name "Ada"`, so that is what runs.
    #[test]
    fn kv_set_prompt_shows_the_literal_commands() {
        let detail = |input: Json| match assess_kv_write("kv_set", &input) {
            WriteAssessment::NeedsApproval { sql } => sql,
            other => panic!(
                "expected NeedsApproval, got {}",
                match other {
                    WriteAssessment::Reject(w) => format!("Reject({w})"),
                    _ => "NotWrite".into(),
                }
            ),
        };
        assert_eq!(
            detail(json!({ "key": "user:1:name", "type": "string", "value": "Ada" })),
            "SET user:1:name Ada"
        );
        // A value needing quoting gets it, and an explicit TTL rides inside SET as
        // the `PX` the driver actually sends.
        assert_eq!(
            detail(json!({
                "key": "greeting", "type": "string", "value": "hello world", "ttl_seconds": 60
            })),
            "SET greeting \"hello world\" PX 60000"
        );
        assert_eq!(
            detail(json!({ "key": "user:1", "type": "hash", "field": "name", "value": "Ada" })),
            "HSET user:1 name Ada"
        );
        // A collection replace is a DEL and a rebuild; the DEL is shown, never implied.
        let zset = detail(json!({
            "key": "board", "type": "zset", "value": { "ada": 1.5 }, "ttl_seconds": 30
        }));
        assert_eq!(zset, "DEL board\nZADD board 1.5 ada\nPEXPIRE board 30000");
        // `append` leaves what is there alone.
        assert_eq!(
            detail(json!({ "key": "q", "type": "list", "value": ["a"], "mode": "append" })),
            "RPUSH q a"
        );
        // A stream always appends: no DEL, whatever `mode` says.
        assert_eq!(
            detail(json!({
                "key": "events", "type": "stream", "value": { "kind": "signup" }, "mode": "set"
            })),
            "XADD events * kind signup"
        );
    }

    #[test]
    fn kv_set_refuses_bad_keys_and_oversized_payloads() {
        let rejected = |input: Json| {
            matches!(
                assess_kv_write("kv_set", &input),
                WriteAssessment::Reject(_)
            )
        };
        // An empty key, and a glob the model mistook for one.
        assert!(rejected(
            json!({ "key": "", "type": "string", "value": "x" })
        ));
        assert!(rejected(
            json!({ "key": "  ", "type": "string", "value": "x" })
        ));
        for pattern in ["*", "?*", "[a-z]*"] {
            assert!(
                rejected(json!({ "key": pattern, "type": "string", "value": "x" })),
                "key `{pattern}` must be refused"
            );
        }
        // Oversized: one huge value, and too many small ones. Both refused at the
        // gate, so neither reaches the driver.
        let huge = "x".repeat(KV_SET_VALUE_MAX + 1);
        assert!(rejected(
            json!({ "key": "k", "type": "string", "value": huge })
        ));
        let many: Vec<String> = (0..KV_SET_ELEMS_MAX + 1).map(|i| i.to_string()).collect();
        assert!(rejected(
            json!({ "key": "k", "type": "set", "value": many })
        ));
        // Shape errors are rejections too, never a prompt.
        assert!(rejected(
            json!({ "key": "k", "type": "trie", "value": "x" })
        ));
        assert!(rejected(
            json!({ "key": "k", "type": "zset", "value": { "a": "not-a-score" } })
        ));
        assert!(rejected(
            json!({ "key": "k", "type": "string", "value": { "nested": 1 } })
        ));
        assert!(rejected(json!({ "key": "k", "type": "set", "value": [] })));
    }
}
