//! The Redis key export walk: scan a scope, read each key through the paths
//! that already page, and write it out in one of the three formats.
//!
//! The rule this file exists to keep is the same one the SQL export keeps:
//! **never hold the keyspace**. The scan runs a page at a time under the
//! existing [`ScanBudget`], each key's value is read through
//! `read_collection_page` / `read_list_window` / `read_stream_range` rather than
//! a `*GETALL`, and each chunk is written and dropped before the next is read.
//! A one-million-element set becomes repeated `SADD` lines and is never
//! assembled. The abort is honoured between keys *and* between collection pages,
//! because reading every value of every key is much heavier than the scan that
//! found them.

use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use red_core::kv::{
    CollectionKind, KV_DUMP_MAGIC, KV_EXPORT_CHUNK, KeyMeta, KvElement, KvExportFormat,
    KvExportOptions, KvExportScope, KvType, ScanBudget, ScanCursor, command_line, commands_header,
    dump_frame, expire_line, is_lossy_text, json_binary, json_string,
};
use red_core::{ExportOutcome, ExportShortfall, RedError, Result};
use red_driver::{AbortSignal, KvDriver};
use tokio::sync::mpsc::UnboundedSender;

/// The scan budget the export walks under. Wider than the browse's (nobody is
/// watching a grid repaint) but still bounded, so a cancel stays responsive.
const EXPORT_SCAN_BUDGET: ScanBudget = ScanBudget {
    count_hint: 500,
    wall_clock: Duration::from_millis(500),
    want: 500,
};

/// Elements read per collection page. The same window the inspector's sub-grid
/// pages through, so a huge collection costs the same here as it does there.
const COLLECTION_PAGE: usize = 1_000;

/// Keys between progress emits. Coarser than the SQL export's row throttle
/// because a key costs several round trips, not one row.
const PROGRESS_KEYS: u64 = 25;

/// Everything that decides what an export file holds, gathered so the walk takes
/// one request rather than seven positional arguments that are easy to transpose.
pub(crate) struct KvExportRequest {
    pub(crate) format: KvExportFormat,
    pub(crate) scope: KvExportScope,
    pub(crate) options: KvExportOptions,
    /// What the header comment names as the source of the keys.
    pub(crate) source: String,
    /// When the export was taken, as the header prints it.
    pub(crate) taken_at: String,
}

/// Run one export to completion. Returns what was written, including the keys a
/// text format could not carry.
pub(crate) async fn run_kv_export(
    driver: &Arc<dyn KvDriver>,
    path: &Path,
    req: KvExportRequest,
    cancel: &AtomicBool,
    progress: UnboundedSender<u64>,
) -> Result<ExportOutcome> {
    let file = std::fs::File::create(path).map_err(|e| RedError::Driver(e.to_string()))?;
    let mut out = BufWriter::new(file);
    let mut state = ExportState::default();

    let result = write_all(driver, &mut out, &req, cancel, &progress, &mut state).await;

    match result {
        Ok(()) => {
            out.flush().map_err(|e| RedError::Driver(e.to_string()))?;
            Ok(ExportOutcome {
                rows: state.written,
                shortfall: (state.skipped > 0)
                    .then_some(ExportShortfall::SkippedKeys(state.skipped)),
            })
        }
        Err(e) => {
            // A cancelled or failed export leaves nothing behind, the same
            // promise `CancelExport` already makes for the SQL path.
            drop(out);
            let _ = std::fs::remove_file(path);
            Err(e)
        }
    }
}

/// Running totals, kept apart from the walk so the error path can still report
/// them.
#[derive(Default)]
struct ExportState {
    written: u64,
    /// Keys a text format could not carry (see `is_lossy_text`).
    skipped: u64,
}

async fn write_all<W: Write>(
    driver: &Arc<dyn KvDriver>,
    out: &mut W,
    req: &KvExportRequest,
    cancel: &AtomicBool,
    progress: &UnboundedSender<u64>,
    state: &mut ExportState,
) -> Result<()> {
    let io = |e: std::io::Error| RedError::Driver(e.to_string());
    let format = req.format;
    match format {
        KvExportFormat::Commands => {
            out.write_all(
                commands_header(&req.source, &req.scope, &req.options, &req.taken_at).as_bytes(),
            )
            .map_err(io)?;
        }
        KvExportFormat::Json => out.write_all(b"[").map_err(io)?,
        KvExportFormat::Dump => out.write_all(KV_DUMP_MAGIC).map_err(io)?,
    }

    let abort = AbortSignal::new();
    let mut cursor = ScanCursor::START;
    let mut last_reported = 0u64;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(RedError::Interrupted);
        }
        let (keys, next, done) = next_page(driver, &req.scope, cursor, &abort).await?;
        for meta in keys {
            if cancel.load(Ordering::Relaxed) {
                return Err(RedError::Interrupted);
            }
            write_key(driver, out, format, &meta, req.options, cancel, state).await?;
            if state.written.saturating_sub(last_reported) >= PROGRESS_KEYS {
                let _ = progress.send(state.written);
                last_reported = state.written;
            }
        }
        cursor = next;
        if done {
            break;
        }
    }

    if format == KvExportFormat::Json {
        out.write_all(b"\n]\n").map_err(io)?;
    }
    Ok(())
}

/// One page of the scope's keys, and whether the walk is finished.
///
/// A `Selection` scope is not a scan at all: the keys are already named, so it
/// resolves their metadata directly and finishes in one page.
async fn next_page(
    driver: &Arc<dyn KvDriver>,
    scope: &KvExportScope,
    cursor: ScanCursor,
    abort: &AbortSignal,
) -> Result<(Vec<KeyMeta>, ScanCursor, bool)> {
    match scope {
        KvExportScope::Selection(keys) => {
            let mut out = Vec::with_capacity(keys.len());
            for key in keys {
                if let Some(meta) = driver.probe_key(key).await? {
                    out.push(meta);
                }
            }
            Ok((out, cursor, true))
        }
        KvExportScope::Matching {
            pattern,
            type_filter,
        } => {
            let page = driver
                .scan_keys(
                    cursor,
                    pattern.as_deref(),
                    type_filter.as_deref(),
                    None,
                    EXPORT_SCAN_BUDGET,
                    abort,
                )
                .await?;
            Ok((page.keys, page.next_cursor, page.exhausted))
        }
        KvExportScope::Database => {
            let page = driver
                .scan_keys(cursor, None, None, None, EXPORT_SCAN_BUDGET, abort)
                .await?;
            Ok((page.keys, page.next_cursor, page.exhausted))
        }
    }
}

/// Write one key in the chosen format, or count it as skipped.
async fn write_key<W: Write>(
    driver: &Arc<dyn KvDriver>,
    out: &mut W,
    format: KvExportFormat,
    meta: &KeyMeta,
    options: KvExportOptions,
    cancel: &AtomicBool,
    state: &mut ExportState,
) -> Result<()> {
    match format {
        KvExportFormat::Dump => write_dump_key(driver, out, meta, state).await,
        KvExportFormat::Commands => {
            write_commands_key(driver, out, meta, options, cancel, state).await
        }
        KvExportFormat::Json => write_json_key(driver, out, meta, cancel, state).await,
    }
}

/// The DUMP format: the server serializes, so this carries any value exactly and
/// never inspects one.
async fn write_dump_key<W: Write>(
    driver: &Arc<dyn KvDriver>,
    out: &mut W,
    meta: &KeyMeta,
    state: &mut ExportState,
) -> Result<()> {
    let Some((payload, ttl)) = driver.dump_key(&meta.key).await? else {
        return Ok(()); // vanished between the scan and here
    };
    let ttl_ms = ttl.map(|d| d.as_millis() as u64).unwrap_or(0);
    out.write_all(&dump_frame(&meta.key, ttl_ms, &payload))
        .map_err(|e| RedError::Driver(e.to_string()))?;
    state.written += 1;
    Ok(())
}

/// The Commands format: the exact inverse of the import that already ships.
async fn write_commands_key<W: Write>(
    driver: &Arc<dyn KvDriver>,
    out: &mut W,
    meta: &KeyMeta,
    options: KvExportOptions,
    cancel: &AtomicBool,
    state: &mut ExportState,
) -> Result<()> {
    let mut lines: Vec<String> = Vec::new();
    if options.del_first {
        lines.push(command_line(["DEL", meta.key.as_str()]));
    }
    if !collect_commands(driver, meta, cancel, &mut lines).await? {
        state.skipped += 1;
        return Ok(());
    }
    if options.ttls
        && let Some(ttl) = meta.ttl
    {
        // Absolute, so an import minutes later does not extend the countdown.
        // A server-clock skew is not worth a round trip here: the browse already
        // reports remaining TTL against the local clock.
        let at = now_unix_ms().saturating_add(ttl.as_millis() as i64);
        lines.push(expire_line(&meta.key, at));
    }
    for line in lines {
        writeln!(out, "{line}").map_err(|e| RedError::Driver(e.to_string()))?;
    }
    state.written += 1;
    Ok(())
}

/// Build the command lines that recreate `meta`'s value, chunked.
///
/// Returns `false` when the value is binary and this format cannot carry it, so
/// the caller counts the key as skipped rather than writing it mangled.
async fn collect_commands(
    driver: &Arc<dyn KvDriver>,
    meta: &KeyMeta,
    cancel: &AtomicBool,
    lines: &mut Vec<String>,
) -> Result<bool> {
    let key = meta.key.as_str();
    match meta.kv_type {
        KvType::String => {
            let Some(value) = driver.read_string_full(key).await? else {
                return Ok(true);
            };
            let Some(text) = exact_text(&value) else {
                return Ok(false);
            };
            lines.push(command_line(["SET", key, &text]));
        }
        KvType::Hash | KvType::Set | KvType::ZSet => {
            let kind = match meta.kv_type {
                KvType::Hash => CollectionKind::Hash,
                KvType::Set => CollectionKind::Set,
                _ => CollectionKind::ZSet,
            };
            let mut cursor = 0u64;
            let abort = AbortSignal::new();
            loop {
                if cancel.load(Ordering::Relaxed) {
                    return Err(RedError::Interrupted);
                }
                let page = driver
                    .read_collection_page(key, kind, cursor, collection_budget(), &abort)
                    .await?;
                for chunk in page.elements.chunks(KV_EXPORT_CHUNK) {
                    let Some(line) = collection_chunk(key, kind, chunk) else {
                        return Ok(false);
                    };
                    lines.push(line);
                }
                cursor = page.next_cursor;
                if page.exhausted {
                    break;
                }
            }
        }
        KvType::List => {
            // A list has no `LSCAN`; the head window is what the driver offers,
            // and it is read in order so `RPUSH` rebuilds the list as it was.
            let items = driver.read_list_window(key, true, LIST_WINDOW).await?;
            if items.iter().any(|v| is_lossy_text(v)) {
                return Ok(false);
            }
            for chunk in items.chunks(KV_EXPORT_CHUNK) {
                let argv = ["RPUSH", key]
                    .into_iter()
                    .chain(chunk.iter().map(String::as_str));
                lines.push(command_line(argv));
            }
        }
        KvType::Stream => {
            let mut before = None;
            let mut entries = Vec::new();
            loop {
                if cancel.load(Ordering::Relaxed) {
                    return Err(RedError::Interrupted);
                }
                let page = driver
                    .read_stream_range(key, before.as_deref(), COLLECTION_PAGE)
                    .await?;
                entries.extend(page.entries);
                before = page.next_before.clone();
                if page.exhausted || before.is_none() {
                    break;
                }
            }
            // `XREVRANGE` reads newest-first; a stream must be rebuilt oldest-
            // first or the ids go backwards and `XADD` refuses them.
            for entry in entries.iter().rev() {
                if is_lossy_text(&entry.id)
                    || entry
                        .fields
                        .iter()
                        .any(|(f, v)| is_lossy_text(f) || is_lossy_text(v))
                {
                    return Ok(false);
                }
                let argv = ["XADD", key, entry.id.as_str()].into_iter().chain(
                    entry
                        .fields
                        .iter()
                        .flat_map(|(f, v)| [f.as_str(), v.as_str()]),
                );
                lines.push(command_line(argv));
            }
        }
        KvType::Json => {
            let Some(text) = driver
                .json_get(key, &red_core::kv::JsonPath::root())
                .await?
            else {
                return Ok(true);
            };
            let Some(doc) = exact_text(&text) else {
                return Ok(false);
            };
            lines.push(command_line(["JSON.SET", key, "$", &doc]));
        }
        // A module type RED has no reader for: there is no command that would
        // recreate it, so it is skipped and counted like a binary value.
        KvType::Other(_) => return Ok(false),
    }
    Ok(true)
}

/// How many list elements an export reads. A list pages only from its ends, so
/// this is the honest bound: past it the export would need `LRANGE` at a deep
/// offset, whose cost grows with the offset.
const LIST_WINDOW: usize = 100_000;

fn collection_budget() -> ScanBudget {
    ScanBudget {
        count_hint: 500,
        wall_clock: Duration::from_millis(500),
        want: COLLECTION_PAGE,
    }
}

/// One `HSET`/`SADD`/`ZADD` line for a chunk of elements, or `None` when the
/// chunk holds a value this format cannot carry.
fn collection_chunk(key: &str, kind: CollectionKind, chunk: &[KvElement]) -> Option<String> {
    let mut argv: Vec<String> = Vec::with_capacity(chunk.len() * 2 + 2);
    argv.push(
        match kind {
            CollectionKind::Hash => "HSET",
            CollectionKind::Set => "SADD",
            CollectionKind::ZSet => "ZADD",
        }
        .to_string(),
    );
    argv.push(key.to_string());
    for element in chunk {
        match element {
            KvElement::Member(m) if is_lossy_text(m) => return None,
            KvElement::Member(m) => argv.push(m.clone()),
            KvElement::Field(f, v) if is_lossy_text(f) || is_lossy_text(v) => return None,
            KvElement::Field(f, v) => {
                argv.push(f.clone());
                argv.push(v.clone());
            }
            KvElement::Scored(m, _) if is_lossy_text(m) => return None,
            // `ZADD` takes score before member, the reverse of how the page
            // carries them.
            KvElement::Scored(m, score) => {
                argv.push(score.to_string());
                argv.push(m.clone());
            }
        }
    }
    Some(command_line(argv.iter().map(String::as_str)))
}

/// The JSON format: one object per key, streamed so a big collection is written
/// element by element rather than assembled.
async fn write_json_key<W: Write>(
    driver: &Arc<dyn KvDriver>,
    out: &mut W,
    meta: &KeyMeta,
    cancel: &AtomicBool,
    state: &mut ExportState,
) -> Result<()> {
    let io = |e: std::io::Error| RedError::Driver(e.to_string());
    if state.written > 0 {
        out.write_all(b",").map_err(io)?;
    }
    write!(
        out,
        "\n  {{\"key\":{},\"type\":{},\"ttl_ms\":{},\"value\":",
        json_string(&meta.key),
        json_string(meta.kv_type.label()),
        meta.ttl
            .map(|d| d.as_millis().to_string())
            .unwrap_or_else(|| "null".into()),
    )
    .map_err(io)?;
    write_json_value(driver, out, meta, cancel).await?;
    out.write_all(b"}").map_err(io)?;
    state.written += 1;
    Ok(())
}

async fn write_json_value<W: Write>(
    driver: &Arc<dyn KvDriver>,
    out: &mut W,
    meta: &KeyMeta,
    cancel: &AtomicBool,
) -> Result<()> {
    let io = |e: std::io::Error| RedError::Driver(e.to_string());
    let key = meta.key.as_str();
    match meta.kv_type {
        KvType::String => match driver.read_string_full(key).await? {
            // Binary is tagged rather than mangled: a reader can tell
            // `{"b64":…}` from the string `<12 bytes>`.
            Some(red_core::Value::Blob(bytes)) => {
                out.write_all(json_binary(&bytes).as_bytes()).map_err(io)?;
            }
            Some(value) => {
                let text = value.to_string();
                out.write_all(json_string(&text).as_bytes()).map_err(io)?;
            }
            None => out.write_all(b"null").map_err(io)?,
        },
        KvType::Hash | KvType::Set | KvType::ZSet => {
            let kind = match meta.kv_type {
                KvType::Hash => CollectionKind::Hash,
                KvType::Set => CollectionKind::Set,
                _ => CollectionKind::ZSet,
            };
            // A hash and a zset are maps; a set is a list.
            let (open, close) = if kind == CollectionKind::Set {
                ("[", "]")
            } else {
                ("{", "}")
            };
            out.write_all(open.as_bytes()).map_err(io)?;
            let mut cursor = 0u64;
            let mut first = true;
            let abort = AbortSignal::new();
            loop {
                if cancel.load(Ordering::Relaxed) {
                    return Err(RedError::Interrupted);
                }
                let page = driver
                    .read_collection_page(key, kind, cursor, collection_budget(), &abort)
                    .await?;
                for element in &page.elements {
                    if !first {
                        out.write_all(b",").map_err(io)?;
                    }
                    first = false;
                    let text = match element {
                        KvElement::Member(m) => json_string(m),
                        KvElement::Field(f, v) => format!("{}:{}", json_string(f), json_string(v)),
                        KvElement::Scored(m, s) => format!("{}:{s}", json_string(m)),
                    };
                    out.write_all(text.as_bytes()).map_err(io)?;
                }
                cursor = page.next_cursor;
                if page.exhausted {
                    break;
                }
            }
            out.write_all(close.as_bytes()).map_err(io)?;
        }
        KvType::List => {
            let items = driver.read_list_window(key, true, LIST_WINDOW).await?;
            out.write_all(b"[").map_err(io)?;
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.write_all(b",").map_err(io)?;
                }
                out.write_all(json_string(item).as_bytes()).map_err(io)?;
            }
            out.write_all(b"]").map_err(io)?;
        }
        KvType::Stream => {
            let mut before = None;
            let mut entries = Vec::new();
            loop {
                let page = driver
                    .read_stream_range(key, before.as_deref(), COLLECTION_PAGE)
                    .await?;
                entries.extend(page.entries);
                before = page.next_before.clone();
                if page.exhausted || before.is_none() {
                    break;
                }
            }
            out.write_all(b"[").map_err(io)?;
            for (i, entry) in entries.iter().rev().enumerate() {
                if i > 0 {
                    out.write_all(b",").map_err(io)?;
                }
                write!(out, "{{\"id\":{},\"fields\":{{", json_string(&entry.id)).map_err(io)?;
                for (j, (f, v)) in entry.fields.iter().enumerate() {
                    if j > 0 {
                        out.write_all(b",").map_err(io)?;
                    }
                    write!(out, "{}:{}", json_string(f), json_string(v)).map_err(io)?;
                }
                out.write_all(b"}}").map_err(io)?;
            }
            out.write_all(b"]").map_err(io)?;
        }
        KvType::Json => {
            // Already JSON: embed the document rather than quoting it as a
            // string, so the export is one document, not a document in a string.
            match driver
                .json_get(key, &red_core::kv::JsonPath::root())
                .await?
            {
                Some(value) => {
                    let text = value.to_string();
                    out.write_all(text.as_bytes()).map_err(io)?;
                }
                None => out.write_all(b"null").map_err(io)?,
            }
        }
        KvType::Other(ref name) => {
            out.write_all(json_string(&format!("<unsupported type {name}>")).as_bytes())
                .map_err(io)?;
        }
    }
    Ok(())
}

/// A value's text only when it is exactly that: a capped or binary body is not,
/// and a `SET` written from one would replace the key with its own prefix.
fn exact_text(value: &red_core::Value) -> Option<String> {
    match value {
        red_core::Value::Text(s) => Some(s.to_string()),
        red_core::Value::Integer(_) | red_core::Value::Real(_) => Some(value.to_string()),
        red_core::Value::Null => Some(String::new()),
        red_core::Value::Blob(_) | red_core::Value::Capped(_) => None,
    }
}

/// The local wall clock in Unix milliseconds, for absolute expiries.
fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use red_core::kv::{KvType, StreamEntry, tokenize_command};

    use super::*;

    fn meta(key: &str, kv_type: KvType) -> KeyMeta {
        KeyMeta {
            key: key.to_string(),
            kv_type,
            ttl: None,
            encoding: String::new(),
            approx_bytes: 0,
        }
    }

    /// A chunk of collection elements becomes one command whose argv, tokenized
    /// back, is exactly what would recreate those elements. `ZADD` is the one
    /// that reverses order (score before member), so it earns its own case.
    #[test]
    fn collection_chunks_round_trip_through_the_tokenizer() {
        let hash = collection_chunk(
            "h",
            CollectionKind::Hash,
            &[
                KvElement::Field("f 1".into(), "v1".into()),
                KvElement::Field("f2".into(), "v\t2".into()),
            ],
        )
        .unwrap();
        assert_eq!(
            tokenize_command(&hash),
            vec!["HSET", "h", "f 1", "v1", "f2", "v\t2"]
        );

        let set = collection_chunk(
            "s",
            CollectionKind::Set,
            &[
                KvElement::Member("m1".into()),
                KvElement::Member("m 2".into()),
            ],
        )
        .unwrap();
        assert_eq!(tokenize_command(&set), vec!["SADD", "s", "m1", "m 2"]);

        // ZADD takes the score first, the reverse of how the page carries it.
        let zset = collection_chunk(
            "z",
            CollectionKind::ZSet,
            &[KvElement::Scored("member one".into(), 1.5)],
        )
        .unwrap();
        assert_eq!(
            tokenize_command(&zset),
            vec!["ZADD", "z", "1.5", "member one"]
        );
    }

    /// A value the text formats cannot carry stops the key rather than writing
    /// it mangled.
    #[test]
    fn a_binary_element_refuses_the_chunk() {
        assert!(
            collection_chunk(
                "s",
                CollectionKind::Set,
                &[KvElement::Member("bad\u{FFFD}bytes".into())]
            )
            .is_none()
        );
        assert!(
            collection_chunk(
                "h",
                CollectionKind::Hash,
                &[KvElement::Field("f".into(), "bad\u{FFFD}".into())]
            )
            .is_none()
        );
    }

    #[test]
    fn only_an_exact_body_can_become_a_set_command() {
        use red_core::{CappedCell, Value};
        assert_eq!(exact_text(&Value::Text("hi".into())).as_deref(), Some("hi"));
        assert_eq!(exact_text(&Value::Integer(7)).as_deref(), Some("7"));
        // A capped body would write the key back as its own prefix; a blob is
        // not text at all. Both refuse.
        assert!(exact_text(&Value::Blob(vec![0xFF])).is_none());
        assert!(
            exact_text(&Value::Capped(Box::new(CappedCell {
                head: "abc".into(),
                len: 9_000,
                blob: false,
            })))
            .is_none()
        );
    }

    /// A stream must be rebuilt oldest-first: `XREVRANGE` reads newest-first and
    /// `XADD` refuses an id that goes backwards.
    #[test]
    fn stream_entries_are_emitted_in_ascending_id_order() {
        let entries = [
            StreamEntry {
                id: "3-0".into(),
                fields: vec![("c".into(), "3".into())],
            },
            StreamEntry {
                id: "1-0".into(),
                fields: vec![("a".into(), "1".into())],
            },
        ];
        let ids: Vec<&str> = entries.iter().rev().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["1-0", "3-0"], "the walk reverses before writing");
    }

    // Live round-trip tests against a real server, provided via
    // `RED_TEST_REDIS_URL` (a spare logical database, since they FLUSHDB), so a
    // run without one skips cleanly. The JSON cases additionally need the
    // RedisJSON module and skip without it.
    //
    //   docker run --rm -d -p 6398:6379 redis/redis-stack-server:latest
    //   export RED_TEST_REDIS_URL='redis://127.0.0.1:6398/9'

    fn live_url() -> Option<String> {
        std::env::var("RED_TEST_REDIS_URL").ok()
    }

    async fn live_driver() -> Option<Arc<dyn KvDriver>> {
        let url = live_url()?;
        match red_driver::RedisDriver::connect(&url, false).await {
            Ok(d) => Some(Arc::new(d) as Arc<dyn KvDriver>),
            Err(e) => panic!("RED_TEST_REDIS_URL is set but unusable: {e}"),
        }
    }

    fn scratch_file(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("red_kvexport_{tag}_{}", std::process::id()))
    }

    fn request(format: KvExportFormat, options: KvExportOptions) -> KvExportRequest {
        KvExportRequest {
            format,
            scope: KvExportScope::Database,
            options,
            source: "test".into(),
            taken_at: "test".into(),
        }
    }

    /// Seed a mixed keyspace and return a snapshot of it for comparison.
    async fn seed(driver: &Arc<dyn KvDriver>) -> Vec<(String, String)> {
        for argv in [
            vec!["FLUSHDB"],
            vec!["SET", "plain", "hello world"],
            vec!["SET", "weird key", "a \"quoted\" and\ttabbed value"],
            vec!["HSET", "h", "f1", "v1", "f 2", "v 2"],
            vec!["RPUSH", "l", "a", "b", "c"],
            vec!["SADD", "s", "m1", "m 2"],
            vec!["ZADD", "z", "1.5", "member one"],
            vec!["XADD", "st", "1-1", "f", "v"],
            vec!["XADD", "st", "2-1", "g", "w"],
        ] {
            let argv: Vec<String> = argv.into_iter().map(str::to_string).collect();
            driver.command(&argv).await.unwrap();
        }
        driver
            .command(&["EXPIRE".into(), "plain".into(), "3600".into()])
            .await
            .unwrap();
        list_keys(driver).await
    }

    /// Every key with its type label, sorted, as the comparison the round-trip
    /// test makes.
    async fn list_keys(driver: &Arc<dyn KvDriver>) -> Vec<(String, String)> {
        let abort = AbortSignal::new();
        let mut cursor = ScanCursor::START;
        let mut out = Vec::new();
        loop {
            let page = driver
                .scan_keys(cursor, None, None, None, EXPORT_SCAN_BUDGET, &abort)
                .await
                .unwrap();
            out.extend(
                page.keys
                    .into_iter()
                    .map(|k| (k.key, k.kv_type.label().to_string())),
            );
            cursor = page.next_cursor;
            if page.exhausted {
                break;
            }
        }
        out.sort();
        out
    }

    /// The exit criterion: export a mixed keyspace to a `.redis` file, flush,
    /// import that file back, and find the keyspace unchanged.
    #[tokio::test]
    async fn commands_export_reimports_to_the_same_keyspace() {
        let Some(driver) = live_driver().await else {
            eprintln!("SKIP {}: RED_TEST_REDIS_URL not set", module_path!());
            return;
        };
        let before = seed(&driver).await;
        let path = scratch_file("cmds.redis");
        let cancel = AtomicBool::new(false);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let outcome = run_kv_export(
            &driver,
            &path,
            request(KvExportFormat::Commands, KvExportOptions::default()),
            &cancel,
            tx,
        )
        .await
        .unwrap();
        assert_eq!(outcome.rows as usize, before.len(), "every key written");
        assert_eq!(outcome.shortfall, None, "nothing skipped");

        // Wipe, then replay the file exactly as the import path does.
        driver.command(&["FLUSHDB".to_string()]).await.unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        for line in text.lines().filter(|l| !l.starts_with('#')) {
            let argv = red_core::kv::tokenize_command(line);
            if argv.is_empty() {
                continue;
            }
            driver
                .command(&argv)
                .await
                .unwrap_or_else(|e| panic!("replaying `{line}` failed: {e}"));
        }

        assert_eq!(
            list_keys(&driver).await,
            before,
            "the keyspace round-tripped"
        );
        // Values survive too, including the one carrying a quote and a tab.
        let weird = driver.read_string_full("weird key").await.unwrap().unwrap();
        assert_eq!(weird.to_string(), "a \"quoted\" and\ttabbed value");
        // An absolute PEXPIREAT means the TTL came back roughly as it was, not
        // extended by however long the round trip took.
        let ttl = driver.probe_key("plain").await.unwrap().unwrap().ttl;
        let secs = ttl.expect("plain still expires").as_secs();
        assert!((3500..=3600).contains(&secs), "ttl was {secs}s");

        std::fs::remove_file(&path).ok();
    }

    /// A value the text format cannot represent is skipped and counted, never
    /// written mangled.
    ///
    /// Seeded as a hash field carrying U+FFFD rather than as raw binary, because
    /// raw binary is not reachable from here: `KvDriver::command` takes `String`
    /// argv, which is exactly the limitation that makes the Commands format
    /// refuse these keys in the first place (a widened byte argv is the eventual
    /// fix, and is not this feature).
    #[tokio::test]
    async fn an_unrepresentable_value_is_skipped_and_reported() {
        let Some(driver) = live_driver().await else {
            eprintln!("SKIP {}: RED_TEST_REDIS_URL not set", module_path!());
            return;
        };
        driver.command(&["FLUSHDB".to_string()]).await.unwrap();
        driver
            .command(&["SET".into(), "ok".into(), "text".into()])
            .await
            .unwrap();
        driver
            .command(&[
                "HSET".into(),
                "lossy".into(),
                "f".into(),
                "bad\u{FFFD}bytes".into(),
            ])
            .await
            .unwrap();

        let path = scratch_file("bin.redis");
        let cancel = AtomicBool::new(false);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let outcome = run_kv_export(
            &driver,
            &path,
            request(KvExportFormat::Commands, KvExportOptions::default()),
            &cancel,
            tx,
        )
        .await
        .unwrap();
        assert_eq!(outcome.rows, 1, "only the representable key was written");
        assert_eq!(
            outcome.shortfall,
            Some(ExportShortfall::SkippedKeys(1)),
            "the skipped key is reported, not silently dropped"
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("SET ok text"));
        assert!(!text.contains("lossy"), "the skipped key is absent: {text}");
        // The shortfall reads as a sentence that names the way out.
        assert!(
            ExportShortfall::SkippedKeys(1).note().contains("DUMP"),
            "the note points at the format that would carry it"
        );
        std::fs::remove_file(&path).ok();
    }

    /// The DUMP format carries exactly what the text formats cannot, and RED
    /// restores it.
    #[tokio::test]
    async fn dump_export_restores_binary_byte_for_byte() {
        let Some(driver) = live_driver().await else {
            eprintln!("SKIP {}: RED_TEST_REDIS_URL not set", module_path!());
            return;
        };
        driver.command(&["FLUSHDB".to_string()]).await.unwrap();
        driver
            .command(&[
                "SET".into(),
                "bin".into(),
                String::from_utf8_lossy(&[0xFF, 0xFE]).into_owned(),
            ])
            .await
            .unwrap();
        let expected = driver.read_string_full("bin").await.unwrap().unwrap();

        let path = scratch_file("bin.rdbdump");
        let cancel = AtomicBool::new(false);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let outcome = run_kv_export(
            &driver,
            &path,
            request(KvExportFormat::Dump, KvExportOptions::default()),
            &cancel,
            tx,
        )
        .await
        .unwrap();
        assert_eq!(outcome.rows, 1);
        assert_eq!(outcome.shortfall, None, "DUMP carries binary");

        driver.command(&["FLUSHDB".to_string()]).await.unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert!(bytes.starts_with(KV_DUMP_MAGIC));
        let mut at = KV_DUMP_MAGIC.len();
        let mut restored = 0;
        while let Some((entry, next)) = red_core::kv::read_dump_frame(&bytes, at) {
            at = next;
            let ttl = (entry.ttl_ms > 0).then(|| Duration::from_millis(entry.ttl_ms));
            driver
                .restore_key(&entry.key, ttl, &entry.payload, true)
                .await
                .unwrap();
            restored += 1;
        }
        assert_eq!(restored, 1);
        assert_eq!(
            driver.read_string_full("bin").await.unwrap().unwrap(),
            expected,
            "the value came back byte for byte"
        );
        std::fs::remove_file(&path).ok();
    }

    /// A cancelled export leaves no partial file, the promise the toast makes.
    #[tokio::test]
    async fn a_cancelled_export_removes_its_file() {
        let Some(driver) = live_driver().await else {
            eprintln!("SKIP {}: RED_TEST_REDIS_URL not set", module_path!());
            return;
        };
        seed(&driver).await;
        let path = scratch_file("cancel.redis");
        let cancel = AtomicBool::new(true); // already cancelled
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let err = run_kv_export(
            &driver,
            &path,
            request(KvExportFormat::Commands, KvExportOptions::default()),
            &cancel,
            tx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, RedError::Interrupted));
        assert!(!path.exists(), "the partial file is removed");
    }

    /// The JSON format is one well-formed document, whatever the key types.
    #[tokio::test]
    async fn json_export_is_parseable() {
        let Some(driver) = live_driver().await else {
            eprintln!("SKIP {}: RED_TEST_REDIS_URL not set", module_path!());
            return;
        };
        seed(&driver).await;
        let path = scratch_file("keys.json");
        let cancel = AtomicBool::new(false);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        run_kv_export(
            &driver,
            &path,
            request(KvExportFormat::Json, KvExportOptions::default()),
            &cancel,
            tx,
        )
        .await
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value =
            serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}\n{text}"));
        let rows = parsed.as_array().expect("a JSON array");
        assert!(!rows.is_empty());
        for row in rows {
            assert!(row.get("key").is_some(), "every row names its key");
            assert!(row.get("type").is_some());
            assert!(row.get("value").is_some());
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_module_type_with_no_reader_is_skipped_not_guessed_at() {
        // `collect_commands` needs a driver, so this pins the classification the
        // arm rests on: an unknown module type has no recreating command.
        let m = meta("ts:1", KvType::Other("TSDB-TYPE".into()));
        assert!(matches!(m.kv_type, KvType::Other(_)));
    }
}
