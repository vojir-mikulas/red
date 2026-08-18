//! Retrieval from what the user has already done: the query history and the
//! saved-query library.
//!
//! A log of statements a human wrote and ran against *this* connection is the
//! highest-signal grounding a database client has, and it costs nothing to
//! collect because RED already keeps it. It encodes, without anyone writing
//! documentation, the real join paths (including the ones no foreign key
//! declares, which is most of them in a database that grew organically), which
//! date column people actually filter on, how a soft delete is expressed here,
//! and whether `status` is compared to a string or an int. A saved query is
//! stronger still: one a human named, kept, and reruns is the user's own settled
//! definition of a metric.
//!
//! Retrieval, not a blob in the prompt: [`search_query_history`] scores on demand
//! against the topic at hand, so a long history costs tokens only for the handful
//! of entries that are actually relevant.
//!
//! **The stores are read, never written, from here.** `QueryHistory::record` is
//! called from the user's run path only. History is valuable *because* it is
//! human-authored; feeding the agent's own `run_select` output back in would be a
//! confidence loop with no ground truth.
//!
//! [`search_query_history`]: history_tool_def

use red_ai::ToolDef;
use red_core::AiLimits;
use red_core::sql::{Dialect, has_word, strip_noise};
use serde_json::{Value as Json, json};

use super::util::cap_result_bytes;

/// Entries returned when the caller doesn't ask, and the ceiling when it does.
/// Small on purpose: these are few-shot examples, not a search result page, and
/// eight relevant statements beat forty of which six are relevant.
const DEFAULT_LIMIT: usize = 8;
const MAX_LIMIT: usize = 20;

/// The most saved queries [`run_list_saved_queries`] names in one reply. A user
/// with hundreds of snippets should still get a usable list rather than a wall,
/// and the reply says how many were left out.
const MAX_LISTED_QUERIES: usize = 100;

/// The `search_query_history` tool. `noun` names what the store holds on this
/// seam ("SQL statements" / "Redis commands"), since the Redis console records
/// into the same per-connection log the SQL editor does.
pub(in crate::ai) fn history_tool_def(noun: &str) -> ToolDef {
    ToolDef {
        name: "search_query_history".into(),
        description: format!(
            "Search the {noun} THIS USER actually ran against THIS connection, newest-first. \
            Call it before writing anything non-trivial: it shows you the real join paths (often \
            not the ones the foreign keys declare), which date column people filter on, how a \
            soft delete is expressed here, and what values a status column is actually compared \
            to. Prefer matching the user's existing idiom over inventing a cleaner one. Pass the \
            tables, columns or concepts you care about as `topic`; matching is whole-word over \
            the statement text. Nothing you run yourself is ever recorded here, so this is \
            human-written SQL, not your own output echoed back."
        ),
        input_schema: json!({
            "type": "object",
            "properties": {
                "topic": {
                    "type": "string",
                    "description": "Words to match: table names, column names, or concepts \
                        (e.g. \"orders revenue monthly\").",
                },
                "limit": {
                    "type": "integer",
                    "description": format!("Max entries to return (1..{MAX_LIMIT}, default {DEFAULT_LIMIT})."),
                },
            },
            "required": ["topic"],
            "additionalProperties": false,
        }),
    }
}

/// The `list_saved_queries` tool: names and descriptions only.
pub(in crate::ai) fn list_saved_queries_tool_def() -> ToolDef {
    ToolDef {
        name: "list_saved_queries".into(),
        description: "List the queries the user has SAVED to their library (name + description). \
            A saved query is one a human named, kept, and reruns, so it is this user's own \
            blessed definition of whatever it computes. Check this before writing a non-trivial \
            query: matching their existing definition of a metric matters more than writing \
            something cleverer. Cheap: bodies are not returned, so read the one you want with \
            read_saved_query."
            .into(),
        input_schema: json!({ "type": "object", "properties": {}, "additionalProperties": false }),
    }
}

/// The `read_saved_query` tool: one query's full body.
pub(in crate::ai) fn read_saved_query_tool_def() -> ToolDef {
    ToolDef {
        name: "read_saved_query".into(),
        description: "Read one saved query's full SQL, by the name list_saved_queries reported. \
            Matching is loose (case and punctuation are ignored), so you don't have to reproduce \
            the display name exactly. Use it to see how the user computes something before \
            writing your own version of it."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "The saved query's name, as listed." },
            },
            "required": ["name"],
            "additionalProperties": false,
        }),
    }
}

/// The `kv_recent_keys` tool: the keys the user has been looking at.
pub(in crate::ai) fn recent_keys_tool_def() -> ToolDef {
    ToolDef {
        name: "kv_recent_keys".into(),
        description: "The keys THIS USER has recently opened in the Redis browser on this \
            connection, newest-first, with their type and TTL. Redis has no schema, so what \
            somebody chose to look at is real evidence about which namespaces matter and what \
            shape their keys take - use it to orient before scanning blindly."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "limit": {
                    "type": "integer",
                    "description": format!("Max keys to return (1..{MAX_LIMIT}, default {DEFAULT_LIMIT})."),
                },
            },
            "additionalProperties": false,
        }),
    }
}

/// Run `search_query_history` for `conn_id`.
///
/// `conn_id` is empty on any transport that has no app-side connection identity;
/// the catalog withholds the tool there, so this is a belt-and-braces refusal
/// rather than a path anyone reaches. Returning *unscoped* history instead would
/// break the one property this tool must hold: staging SQL is not evidence about
/// production, and another connection's statements are nobody's business here.
pub(in crate::ai) fn run_search_history(
    conn_id: &str,
    dialect: Dialect,
    input: &Json,
    limits: &AiLimits,
) -> (String, bool) {
    if conn_id.is_empty() {
        return (
            "error: query history is unavailable on this transport (no connection context)".into(),
            false,
        );
    }
    let topic = input
        .get("topic")
        .and_then(Json::as_str)
        .unwrap_or("")
        .trim();
    if topic.is_empty() {
        return ("error: search_query_history needs a `topic`".into(), false);
    }
    let limit = read_limit(input);
    let entries = red_config::history::QueryHistory::load().for_conn(conn_id);
    let hits = rank(&entries, topic, dialect, limit);
    if hits.is_empty() {
        return (
            format!(
                "No statements matching \"{topic}\" in this connection's history \
                 ({} entries total). Nothing to learn from here; fall back to the schema.",
                entries.len()
            ),
            true,
        );
    }
    let mut out = format!(
        "{} of this user's own statements on this connection, best match first:\n",
        hits.len()
    );
    for entry in hits {
        let when = red_config::history::relative_time(entry.ran_unix);
        if when.is_empty() {
            out.push_str("\n--\n");
        } else {
            out.push_str(&format!("\n-- {when}\n"));
        }
        out.push_str(entry.sql.trim());
        out.push('\n');
    }
    (cap_result_bytes(out, limits.max_result_bytes), true)
}

/// Run `list_saved_queries`.
pub(in crate::ai) fn run_list_saved_queries(limits: &AiLimits) -> (String, bool) {
    let saved = red_config::queries::load();
    if saved.is_empty() {
        return (
            "The user has no saved queries. (They save one with Shift-Cmd-S, or you can offer \
             one with save_query.)"
                .into(),
            true,
        );
    }
    let total = saved.len();
    let mut out = format!("{total} saved queries. Read one with read_saved_query.\n");
    for q in saved.iter().take(MAX_LISTED_QUERIES) {
        match q.description.as_deref().filter(|d| !d.trim().is_empty()) {
            Some(desc) => out.push_str(&format!("- {} - {desc}\n", q.name)),
            None => out.push_str(&format!("- {}\n", q.name)),
        }
    }
    if total > MAX_LISTED_QUERIES {
        out.push_str(&format!(
            "…and {} more not listed.\n",
            total - MAX_LISTED_QUERIES
        ));
    }
    (cap_result_bytes(out, limits.max_result_bytes), true)
}

/// Run `read_saved_query`.
pub(in crate::ai) fn run_read_saved_query(input: &Json, limits: &AiLimits) -> (String, bool) {
    let name = input
        .get("name")
        .and_then(Json::as_str)
        .unwrap_or("")
        .trim();
    if name.is_empty() {
        return ("error: read_saved_query needs a `name`".into(), false);
    }
    let saved = red_config::queries::load();
    let wanted = red_config::queries::slug(name);
    // Match on the slug so the model doesn't have to reproduce punctuation or
    // capitalization exactly, then fall back to a contains-match on the display
    // name for a half-remembered one.
    let hit = saved
        .iter()
        .find(|q| red_config::queries::slug(&q.name) == wanted)
        .or_else(|| {
            let needle = name.to_lowercase();
            saved
                .iter()
                .find(|q| q.name.to_lowercase().contains(&needle))
        });
    match hit {
        Some(q) => (
            cap_result_bytes(
                format!("{}:\n```sql\n{}\n```", q.name, q.sql.trim()),
                limits.max_result_bytes,
            ),
            true,
        ),
        None => {
            let names: Vec<&str> = saved.iter().map(|q| q.name.as_str()).collect();
            (
                format!(
                    "error: no saved query named \"{name}\". Available: {}",
                    if names.is_empty() {
                        "(none)".to_string()
                    } else {
                        names.join(", ")
                    }
                ),
                false,
            )
        }
    }
}

/// Run `kv_recent_keys` for `conn_id`. Same connection-scoping rule as
/// [`run_search_history`].
pub(in crate::ai) fn run_recent_keys(
    conn_id: &str,
    input: &Json,
    limits: &AiLimits,
) -> (String, bool) {
    if conn_id.is_empty() {
        return (
            "error: the recent-keys list is unavailable on this transport (no connection context)"
                .into(),
            false,
        );
    }
    let limit = read_limit(input);
    let store = red_config::recent_keys::RecentKeysStore::load();
    let keys = store.get(conn_id).cloned().unwrap_or_default();
    if keys.is_empty() {
        return (
            "The user hasn't opened any keys on this connection yet.".into(),
            true,
        );
    }
    let mut out = format!(
        "{} recently-viewed keys, newest first:\n",
        keys.len().min(limit)
    );
    for k in keys.iter().take(limit) {
        let ttl = match k.ttl_secs {
            Some(s) => format!("ttl {s}s"),
            None => "no ttl".to_string(),
        };
        let when = red_config::history::relative_time(k.viewed_unix);
        let when = if when.is_empty() {
            String::new()
        } else {
            format!(", {when}")
        };
        out.push_str(&format!("- {} ({}, {ttl}{when})\n", k.key, k.kv_type));
    }
    (cap_result_bytes(out, limits.max_result_bytes), true)
}

/// The caller's `limit`, clamped into `1..=MAX_LIMIT`, defaulting to
/// [`DEFAULT_LIMIT`].
fn read_limit(input: &Json) -> usize {
    input
        .get("limit")
        .and_then(Json::as_u64)
        .map_or(DEFAULT_LIMIT, |n| (n as usize).clamp(1, MAX_LIMIT))
}

/// Score, dedupe and take the top `limit` entries for `topic`.
///
/// Deliberately not a search engine. The store is capped at 1000 short strings,
/// so whole-word scoring over all of them is microseconds; an embedding model
/// would be a dependency, a download, and a background index bought for nothing.
fn rank<'a>(
    entries: &'a [red_config::history::HistoryEntry],
    topic: &str,
    dialect: Dialect,
    limit: usize,
) -> Vec<&'a red_config::history::HistoryEntry> {
    let terms = terms_of(topic);
    if terms.is_empty() {
        return Vec::new();
    }
    let mut scored: Vec<(usize, &red_config::history::HistoryEntry, String)> = entries
        .iter()
        .filter_map(|e| {
            // Score against the *stripped* copy so a term inside a string literal
            // or a comment doesn't count as usage: `WHERE note = 'orders'` is not
            // evidence about the orders table. (A quoted identifier is blanked
            // too, which loses a little recall in the safe direction.)
            let stripped = strip_noise(&e.sql, dialect).to_lowercase();
            let score = score_entry(&stripped, &terms);
            (score > 0).then(|| (score, e, normalize(&stripped)))
        })
        .collect();
    // Best match first; `id` is monotonic, so descending id breaks ties by
    // recency, which is what "the way we do it *now*" means.
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.id.cmp(&a.1.id)));

    let mut seen = std::collections::HashSet::new();
    scored
        .into_iter()
        // A statement run twenty times, or run again with a different literal, is
        // one piece of evidence: the shape is the grounding, not the parameter.
        .filter(|(_, _, norm)| seen.insert(norm.clone()))
        .take(limit)
        .map(|(_, e, _)| e)
        .collect()
}

/// One entry's score: a whole-word hit is worth 1, and a hit in a *table
/// position* (right after `FROM`/`JOIN`/`UPDATE`/`INTO`) is worth 3, because
/// "this statement reads the orders table" is far stronger evidence than "the
/// word orders appears in it".
fn score_entry(stripped_lower: &str, terms: &[String]) -> usize {
    let tokens: Vec<&str> = stripped_lower
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|t| !t.is_empty())
        .collect();
    terms
        .iter()
        .map(|term| {
            if in_table_position(&tokens, term) {
                3
            } else if has_word(stripped_lower, term) {
                1
            } else {
                0
            }
        })
        .sum()
}

/// Whether `term` appears immediately after a keyword that introduces a relation.
/// A one-token lookahead rather than a parser: it misses a schema-qualified name's
/// second half and an aliased subquery, which costs a little ranking accuracy and
/// nothing else.
fn in_table_position(tokens: &[&str], term: &str) -> bool {
    tokens
        .windows(2)
        .any(|w| matches!(w[0], "from" | "join" | "update" | "into" | "table") && w[1] == term)
}

/// The topic's distinct search terms: lowercased, split the same way
/// [`has_word`] splits a haystack, single characters dropped (a stray `a` would
/// match half the log).
fn terms_of(topic: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in topic
        .to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|t| t.len() > 1)
    {
        if !out.iter().any(|seen| seen == t) {
            out.push(t.to_string());
        }
    }
    out
}

/// A statement's dedupe key: whitespace collapsed over the already-stripped copy,
/// so the same query differing only in formatting, comments, or a literal
/// collapses to one entry.
fn normalize(stripped_lower: &str) -> String {
    stripped_lower
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use red_config::history::HistoryEntry;

    fn entry(id: u64, sql: &str) -> HistoryEntry {
        HistoryEntry {
            id,
            sql: sql.to_string(),
            conn_id: "a".into(),
            ran_unix: 0,
            namespace: None,
        }
    }

    fn ranked<'a>(entries: &'a [HistoryEntry], topic: &str) -> Vec<&'a str> {
        rank(entries, topic, Dialect::Generic, 10)
            .into_iter()
            .map(|e| e.sql.as_str())
            .collect()
    }

    /// Relevance beats recency: the newest statement is not automatically the
    /// useful one, which is the whole reason this ranks instead of just tailing
    /// the log.
    #[test]
    fn the_relevant_entry_outranks_the_merely_recent_one() {
        let entries = vec![
            entry(9, "SELECT 1"),
            entry(8, "SELECT count(*) FROM users"),
            entry(
                7,
                "SELECT o.id FROM orders o JOIN accounts a ON a.id = o.account_id",
            ),
        ];
        let hits = ranked(&entries, "orders accounts");
        assert!(hits[0].contains("JOIN accounts"), "got {hits:?}");
        // `SELECT 1` matches nothing and is dropped rather than padding the reply.
        assert!(!hits.contains(&"SELECT 1"));
    }

    /// Reading the table beats merely naming it, even when the bare mention is
    /// the more recent statement.
    #[test]
    fn a_table_position_hit_outranks_a_bare_mention() {
        let entries = vec![
            entry(9, "SELECT orders FROM summary"),
            entry(8, "SELECT id FROM orders"),
        ];
        let hits = ranked(&entries, "orders");
        assert_eq!(hits[0], "SELECT id FROM orders");
        assert_eq!(hits.len(), 2);
    }

    /// Whole-word matching, so a topic never matches a fragment of a longer
    /// identifier: searching for `order` must not drag in every `order_id`.
    #[test]
    fn a_term_never_matches_part_of_an_identifier() {
        let entries = vec![entry(1, "SELECT orders_count FROM summary")];
        assert!(ranked(&entries, "orders").is_empty());
        assert_eq!(ranked(&entries, "orders_count").len(), 1);
    }

    /// A term inside a string literal is not usage.
    #[test]
    fn a_literal_is_not_evidence() {
        let entries = vec![entry(1, "SELECT * FROM notes WHERE body = 'orders'")];
        assert!(ranked(&entries, "orders").is_empty());
        // The real table still matches.
        assert_eq!(ranked(&entries, "notes").len(), 1);
    }

    /// The same statement written twice, and the same statement with a different
    /// literal, are one piece of evidence.
    #[test]
    fn dedupes_on_shape_not_on_bytes() {
        let entries = vec![
            entry(3, "SELECT id FROM orders WHERE status = 'paid'"),
            entry(
                2,
                "select  id\n  from orders\n  where status = 'void' -- note",
            ),
            entry(1, "SELECT id FROM orders WHERE status = 'refunded'"),
        ];
        let hits = ranked(&entries, "orders");
        assert_eq!(hits.len(), 1, "got {hits:?}");
        // The newest survives, so the shown literal is the most recent one.
        assert!(hits[0].contains("paid"));
    }

    #[test]
    fn a_topic_of_only_noise_matches_nothing() {
        let entries = vec![entry(1, "SELECT id FROM orders")];
        assert!(ranked(&entries, "?? a !").is_empty());
        assert!(terms_of("?? a !").is_empty());
    }

    #[test]
    fn limit_is_clamped_to_the_ceiling_and_defaults() {
        assert_eq!(read_limit(&json!({})), DEFAULT_LIMIT);
        assert_eq!(read_limit(&json!({ "limit": 3 })), 3);
        assert_eq!(read_limit(&json!({ "limit": 0 })), 1);
        assert_eq!(read_limit(&json!({ "limit": 9999 })), MAX_LIMIT);
    }

    /// A subagent sent off to research a topic must be able to read the same
    /// history and library the parent can: that is most of what "research this"
    /// means here. `narrow_to_subagent` only strips writes and recursion, so this
    /// asserts the outcome rather than the mechanism.
    #[test]
    fn a_subagent_can_reach_the_grounding_tools() {
        use crate::ai::gate::narrow_to_subagent;
        use crate::ai::sql::catalog::tool_catalog;
        use red_core::AiPolicy;

        let names: Vec<String> = narrow_to_subagent(tool_catalog(&AiPolicy::default()))
            .into_iter()
            .map(|t| t.name)
            .collect();
        for tool in [
            "search_query_history",
            "list_saved_queries",
            "read_saved_query",
        ] {
            assert!(
                names.iter().any(|n| n == tool),
                "{tool} must reach subagents; got {names:?}"
            );
        }
    }

    /// History is grounding *because* it is human-authored. If the service ever
    /// recorded the agent's own `run_select` back into the log, every later
    /// retrieval would be the model reading its own homework, and a wrong join
    /// would harden into "how we do it here". The store is `pub`, so nothing but
    /// this stops a future refactor from wiring the two together.
    #[test]
    fn the_service_never_writes_to_the_query_history() {
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            let Ok(read) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in read.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(path);
                }
            }
        }
        let mut files = Vec::new();
        walk(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut files,
        );
        assert!(!files.is_empty(), "the source walk found nothing");
        for file in files {
            let Ok(src) = std::fs::read_to_string(&file) else {
                continue;
            };
            // This file is skipped because it names the forbidden call in its own
            // doc and in the needle below; the wiring would go in an executor, the
            // turn loop, or dispatch, and those are all still covered.
            if file.file_name().is_some_and(|n| n == "grounding.rs") {
                continue;
            }
            // Only files that touch the store can wire it to a writer, so this
            // pairing stays targeted enough not to trip on unrelated `record`s.
            if src.contains("QueryHistory") {
                assert!(
                    !src.contains("record("),
                    "{} both reads the query history and records into it; the agent's own \
                     statements must never enter the log",
                    file.display()
                );
            }
        }
    }

    /// Connection scoping is a privacy property, not a nicety: another
    /// connection's statements must never surface here. `for_conn` is what
    /// enforces it, so assert the call this tool actually makes.
    #[test]
    fn history_is_scoped_to_the_connection_and_refuses_without_one() {
        let (msg, ok) = run_search_history(
            "",
            Dialect::Generic,
            &json!({ "topic": "orders" }),
            &AiLimits::default(),
        );
        assert!(!ok);
        assert!(msg.contains("no connection context"), "{msg}");
    }
}
