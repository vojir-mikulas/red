//! What the Redis agent is told it can do: the tier-filtered `kv_*` catalog and
//! the system prompt that introduces it.
//!
//! The bounds every walk in this seam runs under also live here, so a reader can
//! see in one place how much work any Redis tool may do before it stops and says
//! it stopped.

use red_ai::ToolDef;
use red_core::{AiPolicy, AiTier};
use serde_json::json;

use super::super::export::export_tool_def;
use super::super::gate::gate_catalog;
use super::super::report::report_tool_def;
use super::super::turn::spawn_subagent_tool_def;
use super::super::util::finish_system_prompt;
use crate::protocol::AiContext;

// --- Redis (KV) agent backend ---

/// Round-trip cap on a bounded keyspace walk, so a `kv_scan_keys`/sample never
/// loops unbounded on a huge keyspace.
pub(super) const KV_SCAN_ROUNDS_CAP: usize = 400;
/// Keys sampled for `kv_analyze` / `kv_biggest_keys` (bounded, like the UI's own
/// biggest-keys/analysis samplers).
pub(super) const KV_SAMPLE_MAX: usize = 20_000;
/// How many biggest keys `kv_biggest_keys` reports by default.
pub(super) const KV_BIGGEST_TOP: usize = 30;
/// How many elements of a collection `kv_get_value` previews.
pub(super) const KV_VALUE_ELEMS: usize = 50;
/// Max keys a single bulk write (kv_delete/kv_expire by pattern) touches per call;
/// past this it reports the bound was hit so the agent can run again.
pub(super) const KV_BULK_MAX: usize = 50_000;
/// Ceiling on the pending entries `kv_stream_groups` lists for one group. The
/// PEL of a stuck consumer can be enormous; the oldest few show the pattern.
pub(super) const KV_PENDING_MAX: usize = 100;
/// How many key templates `kv_key_schema` reports. A real keyspace has a handful
/// of shapes; past this the rollup has stopped rolling anything up.
pub(super) const KV_TEMPLATE_TOP: usize = 40;
/// The Redis agent's read-only tool catalog, gated by tier via
/// [`AiTier::allows_tool`] exactly like the SQL [`tool_catalog`](crate::ai::sql::catalog::tool_catalog). Redis writes
/// aren't wired yet, so every tool here is read-only.
pub(in crate::ai) fn kv_tool_catalog(policy: &AiPolicy) -> Vec<ToolDef> {
    let all = [
        ToolDef {
            name: "kv_server_info".into(),
            description: "Summarize the server: TOPOLOGY (standalone/sentinel/cluster), total key \
                count, version, memory (used/max/fragmentation), connected clients, ops/sec, \
                keyspace hit rate, evictions/expirations, uptime, and per-database key counts. \
                CALL THIS FIRST — a SCAN means something different on a cluster (it fans out \
                across slots), so the topology frames every other answer."
                .into(),
            input_schema: json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        },
        ToolDef {
            name: "kv_scan_keys".into(),
            description: "Find keys by glob pattern (e.g. `user:*`, `session:??`) and return each \
                key's type, TTL, and approximate memory. Bounded — use a selective pattern; this \
                is how you discover what's in the keyspace."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Glob MATCH pattern (default `*`, all keys)." },
                    "limit": { "type": "integer", "description": "Max keys to return." },
                },
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "kv_key_schema".into(),
            description: "Infer the keyspace's STRUCTURE: sample keys, segment each on `:`, and \
                report the key templates behind them (`user:*:sessions`, `cache:v2:product:*`) \
                with each one's key count, type, average size, and TTL coverage. Redis has no \
                schema, so the key template IS the schema — CALL THIS BEFORE REASONING ABOUT WHAT \
                THE KEYSPACE HOLDS, rather than guessing patterns and scanning for them."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Optional glob to restrict the sample (e.g. `cache:*`)." },
                },
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "kv_key_info".into(),
            description: "One key's type, TTL, OBJECT ENCODING, and approximate memory (no value). \
                Use before reading a value to see what shape it is."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": { "key": { "type": "string", "description": "The exact key name." } },
                "required": ["key"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "kv_get_value".into(),
            description: "Read a key's value (capped): a string's contents, or a preview of a \
                hash/set/zset/list/stream's elements. Large collections report their length and a \
                head window rather than materializing whole."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": { "key": { "type": "string", "description": "The exact key name." } },
                "required": ["key"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "kv_read_collection".into(),
            description: "Page DEEP into one big key's contents, past the preview kv_get_value \
                stops at: hash fields, set/zset members (cursor-paged), list elements (a head or \
                tail window), or stream entries (newest-first by ID range). Use this when the \
                preview says the collection is larger than what it showed. Echo the `next_cursor` \
                / `next_before` from the previous page to continue."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "key": { "type": "string", "description": "The exact key name." },
                    "cursor": { "type": "string", "description": "Hash/set/zset: the previous page's next_cursor (omit to start)." },
                    "before": { "type": "string", "description": "Stream: the previous page's next_before, to walk older (omit to start at the newest)." },
                    "from_tail": { "type": "boolean", "description": "List: read the tail rather than the head. Default false." },
                    "limit": { "type": "integer", "description": "Max elements to return." },
                },
                "required": ["key"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "kv_stream_groups".into(),
            description: "A stream's CONSUMER GROUPS: per group, its consumer count, pending \
                (delivered-but-unacked) entries, lag behind the tip, and last-delivered id. Pass \
                `group` to drill into that group's consumers (each with its pending count and \
                idle time) and its oldest pending entries. This is the answer to \"why is my \
                consumer lagging\", \"who owns these pending messages\", and \"is anything \
                stuck\" — a high delivery count with a large idle time is a stuck entry."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "key": { "type": "string", "description": "The stream key." },
                    "group": { "type": "string", "description": "Drill into this group's consumers and pending entries." },
                },
                "required": ["key"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "kv_keyspace_notifications".into(),
            description: "Read the server's `notify-keyspace-events` setting. An empty value means \
                keyspace notifications are OFF and no watcher will ever see anything — the first \
                thing to check when a subscriber reports silence."
                .into(),
            input_schema: json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        },
        ToolDef {
            name: "kv_command".into(),
            description: "Run one INTROSPECTION command verbatim. Deliberately restricted to a \
                hard allowlist of read-only verbs — INFO, MEMORY, OBJECT, TYPE, TTL, PTTL, \
                EXISTS, STRLEN, LATENCY, COMMAND, DBSIZE, LASTSAVE, TIME, ROLE — because a \
                general command tool would route around every other gate in this catalog. \
                Anything else is refused; use the dedicated tool instead. Requires the user's \
                approval, and the approval shows the exact command."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "argv": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "The command and its arguments, e.g. [\"MEMORY\", \"DOCTOR\"].",
                        "minItems": 1,
                    },
                },
                "required": ["argv"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "kv_client_list".into(),
            description: "The clients connected to the server (CLIENT LIST): id, address, name, \
                selected database, age, idle time, flags, and last command. The Redis analogue of \
                a SQL session list. Under a cluster this reports the seed node only."
                .into(),
            input_schema: json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        },
        ToolDef {
            name: "kv_biggest_keys".into(),
            description: "Sample the keyspace and return the largest keys by approximate memory \
                (redis-cli --bigkeys style). Bounded walk; the result says if it was truncated. Use \
                to find what's eating memory."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Optional glob to restrict the sample." },
                    "top": { "type": "integer", "description": "How many biggest keys to return." },
                },
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "kv_analyze".into(),
            description: "Roll a bounded keyspace sample up into a report: total memory, a per-type \
                breakdown, the top key-name namespaces (prefix up to the first `:`) by memory, and \
                a TTL-coverage summary (how many keys never expire vs. expire soon). Use for \
                'what's in here / why is memory high / what lacks a TTL' questions."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Optional glob to restrict the sample." },
                },
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "kv_slowlog".into(),
            description: "The server's SLOWLOG: recent commands that exceeded the slow threshold, \
                with their execution time and arguments. Use to diagnose slowness."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": { "count": { "type": "integer", "description": "How many entries (default 32)." } },
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "kv_config_get".into(),
            description: "Read one or more CONFIG parameters (glob allowed, e.g. `maxmemory*`). \
                Read-only; never sets."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": { "parameter": { "type": "string", "description": "CONFIG parameter or glob (e.g. `maxmemory-policy`)." } },
                "required": ["parameter"],
                "additionalProperties": false,
            }),
        },
        export_tool_def(
            "Write the keys matching a glob, with their values, to a file for the user (CSV or \
             JSON) and hand it over as a card in the chat they can open. Bounded: it walks a large \
             but finite number of keys and says so if it stopped early. Use it when the user asks \
             for an export/dump rather than an answer.",
            json!({
                "pattern": { "type": "string", "description": "Glob MATCH pattern (default `*`, every key)." },
                "format": {
                    "type": "string",
                    "enum": ["csv", "json"],
                    "description": "Output format (default \"json\"; CSV writes key,type,ttl,value).",
                },
                "name": { "type": "string", "description": "A short name for the file, e.g. \"session-keys\"." },
            }),
            &[],
        ),
        report_tool_def(),
        spawn_subagent_tool_def(),
        // --- gated writes (Write tier, writable connection only) ---
        ToolDef {
            name: "kv_set".into(),
            description: "Write a key's value. This is how you CREATE or UPDATE data: pick the \
                Redis `type` and pass `value` in that type's shape — a string/number for \
                `string`, a { field: value } object for `hash` and `stream`, an array for `set` \
                and `list`, a { member: score } object for `zset`. For one hash field, pass \
                `field` plus a scalar `value`. `ttl_seconds` sets an expiry. `mode` is \"set\" \
                (default: the key ends up holding exactly this, so a hash/set/zset/list is \
                cleared first) or \"append\" (add to what is already there); a stream always \
                appends. Requires the user's explicit approval, which shows the exact commands \
                that will run."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "key": { "type": "string", "description": "The exact key to write (not a glob)." },
                    "type": {
                        "type": "string",
                        "enum": ["string", "hash", "set", "zset", "list", "stream"],
                        "description": "The Redis type to write. Check kv_key_info first if the key may already exist as another type.",
                    },
                    "value": { "description": "The value, in the shape this `type` takes (see the tool description)." },
                    "field": { "type": "string", "description": "Hash only: write this single field, leaving the rest of the hash alone." },
                    "ttl_seconds": { "type": "integer", "description": "Expiry in seconds; omit for no expiry." },
                    "mode": {
                        "type": "string",
                        "enum": ["set", "append"],
                        "description": "\"set\" (default) replaces the key's contents; \"append\" adds to them.",
                    },
                },
                "required": ["key", "type", "value"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "kv_expire".into(),
            description: "Set or remove a key's expiry (EXPIRE / PERSIST). Targets one `key`, or \
                every key matching a `pattern` (bulk). Requires the user's explicit approval; a \
                keyspace-wide TTL (pattern `*`) is refused. Read/scan first to know what you'll \
                affect."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "key": { "type": "string", "description": "A single key to expire/persist." },
                    "pattern": { "type": "string", "description": "Glob to bulk-expire all matching keys (mutually exclusive with `key`)." },
                    "seconds": { "type": "integer", "description": "TTL in seconds; omit or 0 to PERSIST (remove expiry)." },
                },
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "kv_delete".into(),
            description: "Delete keys (DEL): one `key`, an explicit list of `keys`, or every key \
                matching a `pattern` (bulk). Requires explicit approval; deleting the whole \
                keyspace (pattern `*`) is refused. Scan/count first and tell the user how many \
                keys will go."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "key": { "type": "string", "description": "A single key to delete." },
                    "keys": { "type": "array", "items": { "type": "string" }, "description": "An explicit list of keys to delete." },
                    "pattern": { "type": "string", "description": "Glob to bulk-delete all matching keys." },
                },
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "kv_rename".into(),
            description: "Rename a key (RENAME `from` `to`); overwrites `to` if it exists. Requires \
                explicit approval."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "from": { "type": "string", "description": "Existing key name." },
                    "to": { "type": "string", "description": "New key name." },
                },
                "required": ["from", "to"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "kv_copy_key".into(),
            description: "Copy a key with its value and remaining expiry to a new name (DUMP then \
                RESTORE), leaving the original alone. The serialized value never passes through \
                this conversation — the server copies it — so this works for a key of any size or \
                type. Requires explicit approval; it refuses to overwrite an existing key unless \
                `replace` is set."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "from": { "type": "string", "description": "The key to copy." },
                    "to": { "type": "string", "description": "The new key name." },
                    "replace": { "type": "boolean", "description": "Overwrite `to` if it already exists. Default false." },
                    "keep_ttl": { "type": "boolean", "description": "Carry the source's remaining expiry over. Default true." },
                },
                "required": ["from", "to"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "kv_client_kill".into(),
            description: "Disconnect a client by its connection id (CLIENT KILL ID). Call \
                kv_client_list first for the `id`, and copy that client's `addr` and `cmd` into \
                this call so the user can see what they are disconnecting — the target is \
                re-checked against the live server first and refused if the id has been reused. \
                Requires explicit approval."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "integer", "description": "The client's connection id, from kv_client_list." },
                    "addr": { "type": "string", "description": "The client's address, copied from kv_client_list; verified before the kill." },
                    "cmd": { "type": "string", "description": "The client's last command, copied from kv_client_list, so the approval shows what is being cut off." },
                },
                "required": ["id"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "kv_config_set".into(),
            description: "Set a server CONFIG parameter (CONFIG SET). Powerful — can change memory \
                limits, persistence, eviction. Requires explicit approval; read the current value \
                with kv_config_get first."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "parameter": { "type": "string", "description": "CONFIG parameter (e.g. `maxmemory-policy`)." },
                    "value": { "type": "string", "description": "New value." },
                },
                "required": ["parameter", "value"],
                "additionalProperties": false,
            }),
        },
    ];
    gate_catalog(all, policy)
}
/// The Redis agent's system prompt (the KV analogue of [`system_prompt`](crate::ai::sql::catalog::system_prompt)): the
/// same shape, but describing the `kv_*` tools and Redis idioms instead of SQL.
/// Grounding is lazy — the model calls `kv_server_info`/`kv_scan_keys` rather
/// than being handed a pre-built summary — so no per-turn keyspace context is
/// needed.
pub(in crate::ai) fn kv_system_prompt(ctx: &AiContext, policy: &AiPolicy) -> String {
    let tools_line = match policy.tier {
        AiTier::Off => {
            "You have NO Redis tools available; answer from the conversation alone and tell the \
             user you cannot read the live server."
        }
        AiTier::Schema => {
            "You have metadata-only Redis tools: kv_server_info, kv_scan_keys, kv_key_schema, and \
             kv_key_info. You can see the server's stats, the keyspace's key templates, and keys' \
             types/TTLs/sizes, but you CANNOT read a key's value."
        }
        AiTier::Read => {
            "You have read-only Redis tools: kv_server_info (INFO summary, topology and size), \
             kv_key_schema (the keyspace's inferred key templates), kv_scan_keys (find keys by \
             glob pattern), kv_key_info (a key's type/TTL/encoding/size), kv_get_value (a key's \
             value or a collection preview), kv_read_collection (page deep into a big \
             collection/list/stream), kv_stream_groups (consumer groups, pending and lag), \
             kv_biggest_keys (sample for the largest keys by memory), kv_analyze (a keyspace \
             rollup: memory by type and namespace, TTL coverage), kv_slowlog (recent slow \
             commands), kv_client_list (connected clients), kv_config_get (read a CONFIG \
             parameter), export_result (write keys to a file for the user), and generate_report \
             (author an HTML report from what you've read, with optional Chart.js charts; it \
             appears as a card the user can open — use it when the user asks for a report). Ground \
             every answer in the live server with these tools rather than guessing."
        }
        AiTier::Write => {
            "You have the read-only Redis tools (kv_server_info, kv_scan_keys, kv_key_info, \
             kv_get_value, kv_biggest_keys, kv_analyze, kv_slowlog, kv_config_get, generate_report) \
             AND gated tools: kv_set (write a key of any type — this is how you create or update \
             data), kv_expire (set/remove a key's TTL), kv_delete (delete keys), kv_rename, \
             kv_copy_key, kv_client_kill, kv_config_set, and kv_command (introspection verbs only). \
             Every one requires the user's explicit Allow on the exact operation; assume it may be \
             denied. Before a bulk kv_delete/kv_expire by pattern, scan first (kv_scan_keys) and \
             tell the user how many keys will be affected — a keyspace-wide delete or expire \
             (pattern `*`) is refused outright. Only write when the user has asked you to change \
             data."
        }
    };
    finish_system_prompt(
        format!(
            "You are RED's Redis agent, embedded in a native database explorer. You help the user \
             explore and understand the Redis server they are connected to.\n\n\
             {tools_line}\n\n\
             Call kv_server_info first — it tells you the topology, and a SCAN means something \
             different on a cluster. Call kv_key_schema before reasoning about what the keyspace \
             holds; the key template is the schema.\n\n\
             Redis keys are addressed by glob patterns (e.g. `user:*`), not SQL — there are no \
             tables or joins. Be concise: lead with the answer, then the supporting detail. When \
             you show a command, put it in a fenced ```sh block (e.g. `redis-cli GET foo`).\n",
        ),
        ctx,
        "This connection is READ-ONLY.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use red_core::AiPolicy;

    #[test]
    fn kv_catalog_offers_writes_only_at_write_tier_and_not_read_only() {
        let names = |p: AiPolicy| {
            kv_tool_catalog(&p)
                .into_iter()
                .map(|t| t.name)
                .collect::<Vec<_>>()
        };
        // Read tier: reads only, no write tools.
        let read = names(AiPolicy::default());
        assert!(read.iter().any(|n| n == "kv_scan_keys"));
        assert!(read.iter().all(|n| n != "kv_delete"));
        // Write tier offers the write tools…
        let write = names(AiPolicy {
            tier: AiTier::Write,
            ..AiPolicy::default()
        });
        assert!(write.iter().any(|n| n == "kv_delete"));
        assert!(write.iter().any(|n| n == "kv_config_set"));
        // …but withholds them on a read-only connection.
        let write_ro = names(AiPolicy {
            tier: AiTier::Write,
            read_only: true,
            ..AiPolicy::default()
        });
        assert!(write_ro.iter().all(|n| n != "kv_delete"));
        assert!(write_ro.iter().any(|n| n == "kv_scan_keys"));
    }
}
