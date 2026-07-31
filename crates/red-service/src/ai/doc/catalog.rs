//! What the MongoDB agent is told it can do: the tier-filtered catalog and the
//! system prompt that introduces it.
//!
//! The prompt leans harder on ordering than the other two seams', because a
//! document store has no declared schema to fall back on: orient first
//! (`doc_server_info`, `list_collections`, `describe_collection`,
//! `doc_reference_map`), then read.

use red_ai::ToolDef;
use red_core::{AiPolicy, AiTier};
use serde_json::{Value as Json, json};

use super::super::export::export_tool_def;
use super::super::gate::gate_catalog;
use super::super::knowledge::knowledge_tool_def;
use super::super::report::report_tool_def;
use super::super::turn::spawn_subagent_tool_def;
use super::super::util::finish_system_prompt;
use crate::protocol::AiContext;

/// The tier-filtered MongoDB tool catalog. Same shape as `kv_tool_catalog`: an
/// array of every def, then a tier + read-only filter.
pub(in crate::ai) fn doc_tool_catalog(policy: &AiPolicy) -> Vec<ToolDef> {
    let coll_args = |extra: Json| {
        // `{ db, coll }` plus tool-specific properties merged in.
        let mut props = serde_json::Map::new();
        props.insert(
            "db".into(),
            json!({ "type": "string", "description": "Database name." }),
        );
        props.insert(
            "coll".into(),
            json!({ "type": "string", "description": "Collection name." }),
        );
        if let Json::Object(m) = extra {
            props.extend(m);
        }
        json!({
            "type": "object",
            "properties": props,
            "required": ["db", "coll"],
            "additionalProperties": false,
        })
    };
    let all = [
        ToolDef {
            name: "doc_server_info".into(),
            description: "Summarize the deployment: server version, topology \
                (standalone/replica-set/sharded), and the databases with their sizes. Call this \
                first to understand what you're connected to."
                .into(),
            input_schema: json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        },
        ToolDef {
            name: "list_collections".into(),
            description: "The catalog: collections in a database (or every database when `db` is \
                omitted), with estimated document counts and view/time-series/capped kind."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": { "db": { "type": "string", "description": "Database to list; omit for all." } },
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "describe_collection".into(),
            description: "One collection's DISCOVERED schema (sampled field paths with per-type \
                frequency and present-ratio) plus its indexes. The schema is inferred from a \
                sample, not declared — a field can legitimately be several types."
                .into(),
            input_schema: coll_args(json!({})),
        },
        ToolDef {
            name: "doc_reference_map".into(),
            description: "Discover which fields REFERENCE other collections, and how well they \
                resolve. MongoDB has no foreign keys, so a field named `user_id` may point at \
                `users._id`, at something else, or at nothing: this samples each candidate \
                field's values, probes the target collection's `_id`, and reports the HIT RATE \
                (\"198/200 resolve\" is a usable join; \"0/200\" is a name collision). CALL THIS \
                BEFORE WRITING AN AGGREGATION THAT $lookups ACROSS COLLECTIONS. Bounded: a few \
                collections, a couple of hundred sampled values per field."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "db": { "type": "string", "description": "Database to map; omit for every non-system database." },
                    "collections": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Restrict to these collections; omit for all in the database.",
                    },
                },
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "profile_collection".into(),
            description: "The signature data-quality tool: sample the collection and report, per \
                field path, its type distribution and how often it is present — surfacing schema \
                drift (a field that's string here and int there) and optional fields. Never \
                returns raw documents."
                .into(),
            input_schema: coll_args(
                json!({ "sample": { "type": "integer", "description": "Documents to sample (default 200)." } }),
            ),
        },
        ToolDef {
            name: "get_document".into(),
            description: "Fetch ONE document by its `_id`. Cheaper and less error-prone than a \
                find with an _id filter. Pass an ObjectId as { \"$oid\": \"…\" } and a plain id \
                as itself."
                .into(),
            input_schema: coll_args(json!({
                "id": { "description": "The _id to fetch, in extended JSON ({ \"$oid\": \"…\" }) or as a plain scalar." },
            })),
        },
        ToolDef {
            name: "sample_documents".into(),
            description: "Return N random documents ($sample) so you can see the real shape before \
                writing a filter — the cheap 'show me what this looks like' a schemaless store needs."
                .into(),
            input_schema: coll_args(
                json!({ "n": { "type": "integer", "description": "How many to sample (default 5)." } }),
            ),
        },
        ToolDef {
            name: "find".into(),
            description: "Run a read-only find. `filter`/`projection`/`sort` are JSON documents \
                (extended JSON, e.g. { \"status\": \"active\" }); rows are capped. The only way to \
                read actual documents."
                .into(),
            input_schema: coll_args(json!({
                "filter": { "type": "object", "description": "Match document (empty = all)." },
                "projection": { "type": "object", "description": "Fields to include/exclude." },
                "sort": { "type": "object", "description": "Sort spec, e.g. { \"age\": -1 }." },
                "limit": { "type": "integer", "description": "Max documents to return." },
            })),
        },
        ToolDef {
            name: "aggregate".into(),
            description: "Run a read-only aggregation pipeline (a JSON array of stages). Write \
                stages ($out/$merge) are rejected. This is Mongo's analytical engine — group, \
                bucket, lookup, facet — well past what a plain find can express."
                .into(),
            input_schema: coll_args(
                json!({ "pipeline": { "type": "array", "description": "Array of aggregation stage documents." } }),
            ),
        },
        ToolDef {
            name: "count".into(),
            description: "Count documents matching an optional filter — cheap cardinality without \
                pulling documents."
                .into(),
            input_schema: coll_args(json!({ "filter": { "type": "object", "description": "Match document (empty = all)." } })),
        },
        ToolDef {
            name: "distinct".into(),
            description: "The distinct values of one field over documents matching an optional filter."
                .into(),
            input_schema: coll_args(json!({
                "field": { "type": "string", "description": "Field path." },
                "filter": { "type": "object", "description": "Match document (empty = all)." },
            })),
        },
        ToolDef {
            name: "explain_query".into(),
            description: "Explain a find: the winning plan, the index used, ACTUAL docs-examined \
                vs returned (it runs with executionStats, so these are measurements rather than \
                estimates), and an explicit COLLSCAN flag. Examined far exceeding returned is the \
                missing-index signature."
                .into(),
            input_schema: coll_args(json!({ "filter": { "type": "object", "description": "The find filter to explain." } })),
        },
        ToolDef {
            name: "index_advice".into(),
            description: "Given a find filter, is it index-covered? If it's a collection scan, \
                suggest the index key to add. Does NOT create it — that's a gated write."
                .into(),
            input_schema: coll_args(json!({ "filter": { "type": "object", "description": "The find filter to advise on." } })),
        },
        ToolDef {
            name: "doc_current_op".into(),
            description: "What the deployment is running RIGHT NOW ($currentOp), longest-running \
                first: opid, operation kind, namespace, elapsed time, client, the command itself, \
                and whether it is blocked waiting for a lock. The \"why is it slow right now\" \
                answer, as opposed to audit_collection's structural one. Idle connections are \
                excluded."
                .into(),
            input_schema: json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        },
        ToolDef {
            name: "audit_collection".into(),
            description: "Roll a sample into a health report: schema drift (mixed-type fields), \
                optional/sparse fields, and index coverage. The 'what's wrong in here' answer."
                .into(),
            input_schema: coll_args(json!({})),
        },
        export_tool_def(
            "Write the documents matching a filter to a JSON file for the user (an array of \
             extended-JSON documents) and hand it over as a card in the chat they can open. \
             Bounded: it pages through a large but finite number of documents and says so if it \
             stopped early. Use it when the user asks for an export/dump rather than an answer.",
            json!({
                "db": { "type": "string", "description": "Database name." },
                "coll": { "type": "string", "description": "Collection name." },
                "filter": { "type": "object", "description": "Match document (empty = the whole collection)." },
                "name": { "type": "string", "description": "A short name for the file, e.g. \"active-users\"." },
            }),
            &["db", "coll"],
        ),
        report_tool_def(),
        knowledge_tool_def(),
        spawn_subagent_tool_def(),
        // --- gated writes (Write tier, writable connection only) ---
        ToolDef {
            name: "propose_doc_write".into(),
            description: "Propose ONE write (insert/update/replace/delete) for the user to approve. \
                `update`/`delete` require a non-empty `filter`; `many:true` (affect all matches) is \
                shown explicitly in the approval. Read/find first to know what you'll affect."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "op": { "type": "string", "enum": ["insert", "update", "replace", "delete"] },
                    "db": { "type": "string" },
                    "coll": { "type": "string" },
                    "filter": { "type": "object", "description": "Match document (required for update/replace/delete)." },
                    "document": { "type": "object", "description": "The document to insert, or the replacement (insert/replace)." },
                    "update": { "type": "object", "description": "The $set-style patch fields (update)." },
                    "many": { "type": "boolean", "description": "Affect all matches, not just one (update/delete)." },
                },
                "required": ["op", "db", "coll"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "propose_index".into(),
            description: "Propose creating an index for the user to approve. Building an index \
                loads the server; the user approves."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "db": { "type": "string" },
                    "coll": { "type": "string" },
                    "keys": {
                        "type": "object",
                        "description": "Index key spec, e.g. { \"email\": 1, \"createdAt\": -1 }.",
                    },
                    "unique": { "type": "boolean" },
                },
                "required": ["db", "coll", "keys"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "doc_kill_op".into(),
            description: "Stop one running operation by its opid (killOp). Call doc_current_op \
                first for the `opid`, and copy that operation's `namespace` and `command` into \
                this call so the user can see what they are stopping — the target is re-checked \
                against the live server first and refused if the opid now belongs to something \
                else. Note that Mongo does NOT roll back an interrupted multi-document write: a \
                killed updateMany leaves what it already changed. Requires explicit approval."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "opid": { "type": "integer", "description": "The operation id from doc_current_op." },
                    "namespace": { "type": "string", "description": "The operation's db.collection, copied from doc_current_op; verified before the kill." },
                    "command": { "type": "string", "description": "The operation's command, copied from doc_current_op, so the approval shows what is being stopped." },
                },
                "required": ["opid"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "propose_collection_op".into(),
            description: "Propose creating or dropping a collection for the user to approve. \
                Dropping is destructive and always requires explicit confirmation."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "op": { "type": "string", "enum": ["create", "drop"] },
                    "db": { "type": "string" },
                    "coll": { "type": "string" },
                },
                "required": ["op", "db", "coll"],
                "additionalProperties": false,
            }),
        },
    ];
    gate_catalog(all, policy)
}
/// The MongoDB agent's grounding prompt (the doc analogue of `kv_system_prompt`).
pub(in crate::ai) fn doc_system_prompt(ctx: &AiContext, policy: &AiPolicy) -> String {
    let tools_line = match policy.tier {
        AiTier::Off => {
            "You have NO MongoDB tools available; the assistant is limited to general conversation."
        }
        AiTier::Schema => {
            "You have metadata-only MongoDB tools: doc_server_info (which lists the databases), \
             list_collections, and describe_collection. You can see the catalog and each \
             collection's inferred schema, indexes and validator, but cannot read documents."
        }
        AiTier::Read => {
            "You have read-only MongoDB tools: doc_server_info (deployment, topology and the \
             database list), list_collections, describe_collection (inferred schema, indexes and \
             any declared validator), doc_reference_map (which fields reference which collections, and \
             how well they resolve), profile_collection (per-field type/drift stats), \
             sample_documents, get_document (one document by _id), find, aggregate ($out/$merge \
             rejected), count, distinct, explain_query (optionally with execution stats), \
             index_advice, audit_collection, doc_current_op (what is running now), export_result, \
             generate_report, and save_knowledge (draft this connection's knowledge file for the \
             user to review). Ground every answer in the live deployment."
        }
        AiTier::Write => {
            "You have the read-only MongoDB tools (doc_server_info, list_collections, \
             describe_collection, profile_collection, sample_documents, find, aggregate, count, \
             distinct, explain_query, index_advice, audit_collection, doc_current_op, \
             export_result, save_knowledge) AND gated write tools: propose_doc_write, propose_index, \
             propose_collection_op, and doc_kill_op. Every write requires the user's explicit \
             Allow; update/delete require a non-empty filter, and dropping a collection always \
             confirms."
        }
    };
    finish_system_prompt(
        format!(
            "You are RED's MongoDB agent, embedded in a native database explorer. You help the \
             user explore and understand the MongoDB deployment they are connected to.\n\n\
             {tools_line}\n\n\
             MongoDB is SCHEMALESS: a collection has no declared columns, and a field can be \
             several types across documents. So ORIENT before you act — doc_server_info to see the \
             deployment, list_collections for the catalog, describe_collection/profile_collection \
             to learn the discovered schema, sample_documents to see real shape — THEN \
             find/aggregate to read, and explain_query/index_advice/audit_collection to reason \
             about performance and health. Before writing an aggregation that joins collections \
             with $lookup, call doc_reference_map: Mongo has no foreign keys, so a field named \
             `user_id` may not resolve, and the map tells you the hit rate.\n\n\
             Each tool result is labelled `[source N]`. When you state a figure or a fact you \
             read from a tool, append that marker to the claim - \"revenue was $4.2M [3]\". One \
             marker per claim, never the same source twice in a sentence, and never a marker on \
             something you reasoned out rather than read: a citation says where a number came \
             from, not that it is right.\n\n\
             Queries are filter documents and aggregation pipelines (extended JSON), never SQL. Be \
             concise: lead with the answer, then the detail.\n",
        ),
        ctx,
        "This connection is READ-ONLY.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doc_catalog_offers_writes_only_at_write_tier_and_not_read_only() {
        let names = |p: AiPolicy| {
            doc_tool_catalog(&p)
                .into_iter()
                .map(|t| t.name)
                .collect::<Vec<_>>()
        };
        // Read tier: the reads (incl. the signature tools), no write tools.
        let read = names(AiPolicy::default());
        assert!(read.iter().any(|n| n == "find"));
        assert!(read.iter().any(|n| n == "profile_collection"));
        assert!(read.iter().all(|n| n != "propose_doc_write"));
        // Write tier offers the gated writes…
        let write = names(AiPolicy {
            tier: AiTier::Write,
            ..AiPolicy::default()
        });
        assert!(write.iter().any(|n| n == "propose_doc_write"));
        assert!(write.iter().any(|n| n == "propose_collection_op"));
        // …but withholds them on a read-only connection.
        let write_ro = names(AiPolicy {
            tier: AiTier::Write,
            read_only: true,
            ..AiPolicy::default()
        });
        assert!(write_ro.iter().all(|n| n != "propose_doc_write"));
        assert!(write_ro.iter().any(|n| n == "find"));
    }
}
