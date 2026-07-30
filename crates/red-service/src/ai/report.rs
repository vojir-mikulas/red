//! The `generate_report` tool and the HTML shell it writes into.
//!
//! Engine-agnostic: the model authors the body from whatever it read on any
//! seam, and this turns it into a standalone document. The security posture is
//! the point of the file -- the report is sandboxed by a per-report CSP nonce,
//! the model's HTML is stripped of scripts and remote resources, and the only
//! code allowed to run is the trusted bundled chart renderer, which is handed
//! *data* and never model-supplied JavaScript.

use std::path::Path;

use red_ai::ToolDef;
use serde_json::{Value as Json, json};

use super::state::ReportSink;
use crate::protocol::ReportTheme;

/// Cap on the report payload a `generate_report` call may embed (body HTML plus the
/// serialized charts/data/filters). The model assembles `data` from already-capped
/// query results, but nothing else bounds what it can echo, and the renderer
/// builds one DOM node per row with no virtualization, so an oversized payload makes
/// a multi-MB document that's slow (or hostile) to open in the browser. Past this we
/// refuse and tell the model to narrow the report rather than write the file.
const MAX_REPORT_BYTES: usize = 4 * 1024 * 1024;
/// The `generate_report` tool definition, shared by the SQL and KV catalogs (the
/// report pipeline is engine-agnostic — the model authors HTML from whatever it
/// read).
pub(in crate::ai) fn report_tool_def() -> ToolDef {
    ToolDef {
        name: "generate_report".into(),
        description: "Write a custom HTML report for the user. It appears as a card in the \
            chat with an \"Open\" button; the user opens it in their browser when they choose \
            (it is NOT opened automatically). \
            YOU author the report: first read the data (with the read tools), then call this with \
            `html` set to the report's body: headings, prose/summary, one or more <table>s, \
            even an inline <svg> chart. Use semantic HTML and inline `style=\"…\"` for any \
            styling; a base stylesheet (light/dark) is already applied. Scripts and remote/\
            external resources (other domains, <script>, remote <img>/CSS) are stripped or \
            blocked for safety, so keep everything self-contained (data URIs for images). \
            For INTERACTIVE charts (hover tooltips, legends), pass `charts` (an array of \
            Chart.js v4 config objects) and reference each one from the body with an empty \
            <div data-red-chart=\"INDEX\"></div> placeholder (INDEX is the chart's position \
            in the array). The charts are rendered by a trusted built-in Chart.js; you supply \
            DATA only (no JavaScript/function callbacks; they are ignored). \
            For INTERACTIVE TABLES the user can search/sort/filter, pass `data` (named \
            datasets of {columns, rows}) and drop a <div data-red-table=\"NAME\"></div> \
            placeholder; the user gets a live filter box, click-to-sort headers, and per-column \
            filters. A chart can BIND to a dataset instead of carrying inline data: give it \
            {\"dataset\":\"NAME\",\"type\":\"bar\",\"x\":\"colName\",\"y\":[\"colA\"]}, and it \
            re-draws automatically when the user filters that dataset's table. \
            For DASHBOARD-style controls (like Grafana variables) that drive EVERY table and \
            bound chart at once, pass `filters`, e.g. a multi-select to show only chosen \
            regions: {\"column\":\"Region\",\"type\":\"multiselect\"}. They render as a control \
            bar at the top of the report. Prefer this (data + bound charts + a table + \
            filters) when the user wants to explore/slice the data; prefer inline-data charts \
            for a fixed visual. \
            Use this when the user asks for a report."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "html": { "type": "string", "description": "The report BODY as self-contained HTML (no <html>/<head>/<body> wrapper; that's added). Reference charts with <div data-red-chart=\"INDEX\"></div> and interactive tables with <div data-red-table=\"NAME\"></div> placeholders." },
                "title": { "type": "string", "description": "Report title (browser tab + heading)." },
                "charts": {
                    "type": "array",
                    "description": "Optional interactive charts. Each item is EITHER a full Chart.js v4 config with inline data, e.g. {\"type\":\"bar\",\"data\":{\"labels\":[…],\"datasets\":[{\"label\":\"Revenue\",\"data\":[…]}]},\"options\":{…}}, OR a dataset binding {\"dataset\":\"NAME\",\"type\":\"bar\",\"x\":\"colName\",\"y\":[\"col1\",\"col2\"],\"aggregate\":\"sum\",\"options\":{…}} that derives its data from a named `data` dataset and follows that table's filters. type is one of bar, line, pie, doughnut, radar, polarArea, scatter, bubble. aggregate (sum/avg/min/max/count/none, default none) groups rows sharing an x value. Data only; no functions/callbacks. Place a <div data-red-chart=\"INDEX\"></div> in the body for each.",
                    "items": { "type": "object" },
                },
                "data": {
                    "type": "object",
                    "description": "Optional named datasets for interactive tables and filter-linked charts, e.g. {\"sales\":{\"columns\":[\"Month\",\"Region\",\"Revenue\"],\"rows\":[[\"Jan\",\"NA\",120],[\"Feb\",\"EU\",90]]}}. Each value is {columns:[string], rows:[[cell,…]]} (cells are strings/numbers/null). Reference a dataset with <div data-red-table=\"sales\"></div> for a searchable/sortable table, and/or bind charts to it via {\"dataset\":\"sales\",…}.",
                    "additionalProperties": { "type": "object" },
                },
                "filters": {
                    "type": "array",
                    "description": "Optional report-wide filter controls (Grafana-style variables) that filter EVERY table and bound chart. Each is {\"column\":\"Region\",\"type\":\"multiselect\",\"label\":\"Region\",\"dataset\":\"sales\",\"default\":[…]}. type: multiselect (checkbox dropdown: pick which values to show; this is the 'show only selected regions' control), select (single value), range (numeric min/max), or search (substring). column must exist in the dataset(s); omit `dataset` to apply to all datasets that have that column. `default` pre-selects values (multiselect/select). They appear in a bar at the top; no body placeholder needed (optionally place <div data-red-filters></div> to position it).",
                    "items": { "type": "object" },
                },
            },
            "required": ["html"],
            "additionalProperties": false,
        }),
    }
}
/// The `generate_report` tool: wrap the model-authored HTML (+ optional
/// charts/data/filters) in a sandboxed, themed shell, size-check it, write it to
/// the report dir, and announce it as a chat card. Engine-agnostic — the report
/// pipeline is identical for SQL and Redis — so both `run_tool` and `kv_run_tool`
/// call it.
pub(in crate::ai) fn run_generate_report(input: &Json, report: &ReportSink) -> (String, bool) {
    let body = input
        .get("html")
        .and_then(Json::as_str)
        .unwrap_or("")
        .trim();
    if body.is_empty() {
        return (
            "error: generate_report needs `html` (the report body you authored)".into(),
            false,
        );
    }
    let title = input.get("title").and_then(Json::as_str);
    // Optional interactive charts: keep only well-formed Chart.js spec objects.
    // They are embedded as inert data and rendered by the trusted bundle (see
    // `wrap_report_html`); anything that isn't an object is dropped rather than
    // smuggled into the document.
    let charts: Vec<Json> = input
        .get("charts")
        .and_then(Json::as_array)
        .map(|items| items.iter().filter(|c| c.is_object()).cloned().collect())
        .unwrap_or_default();
    // Optional named datasets for interactive (filterable/sortable) tables and
    // filter-linked charts. Kept only if it's an object map.
    let data = input.get("data").filter(|v| v.is_object());
    // Optional report-wide filter controls (Grafana-style variables). Objects only.
    let filters: Vec<Json> = input
        .get("filters")
        .and_then(Json::as_array)
        .map(|items| items.iter().filter(|c| c.is_object()).cloned().collect())
        .unwrap_or_default();
    let html = wrap_report_html(title, body, &charts, data, &filters, report.theme());
    // Refuse an oversized report by measuring the FINAL document, discounting the
    // fixed chart bundle so the cap measures the model's contribution.
    let report_bytes = html.len().saturating_sub(REPORT_CHARTS_JS.len());
    if report_bytes > MAX_REPORT_BYTES {
        return (
            format!(
                "error: the report is too large ({} KiB; the cap is {} KiB). Summarize or \
                 aggregate the data, or narrow it, then try again.",
                report_bytes / 1024,
                MAX_REPORT_BYTES / 1024,
            ),
            false,
        );
    }
    let path = report
        .output_dir()
        .join(format!("red-report-{}.html", uuid::Uuid::new_v4().simple()));
    match write_report_file(&path, &html) {
        Ok(()) => {
            let clean_title = title.map(str::trim).filter(|t| !t.is_empty());
            report.announce(&path, clean_title);
            let label = clean_title.map(|t| format!(" “{t}”")).unwrap_or_default();
            (
                format!(
                    "Generated the report{label}. It's now available as a card in the chat for \
                     the user to open."
                ),
                true,
            )
        }
        Err(e) => (
            format!("error: could not write the report file: {e}"),
            false,
        ),
    }
}
/// The report shell's inline stylesheet: a neutral, light/dark base the model's
/// `style="…"` can build on. No external fonts/assets (the CSP forbids them).
const REPORT_STYLE: &str = concat!(
    "<style>",
    ":root{color-scheme:light dark}",
    "*{box-sizing:border-box}",
    "body{margin:0;padding:32px 24px;max-width:1100px;margin-inline:auto;",
    "font:15px/1.6 -apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,sans-serif;",
    "background:#fff;color:#1a1a1a}",
    "h1{font-size:22px}h2{font-size:17px;margin-top:1.6em}",
    "table{border-collapse:collapse;width:100%;margin:12px 0;font-variant-numeric:tabular-nums}",
    "th,td{padding:7px 12px;text-align:left;border-bottom:1px solid #e5e7eb}",
    "th{background:#f6f7f9;font-weight:600}",
    "tbody tr:nth-child(even){background:#fafbfc}",
    "code,pre{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;background:#f3f4f6;border-radius:4px}",
    "code{padding:1px 5px}pre{padding:12px;overflow:auto}",
    "@media(prefers-color-scheme:dark){",
    "body{background:#0f1115;color:#e6e6e6}",
    "th,td{border-bottom-color:#262a31}th{background:#161a20}",
    "tbody tr:nth-child(even){background:#13161b}",
    "code,pre{background:#1b2028}}",
    "</style>",
);
/// The report's base document style. With a `theme` (the active RED palette) the
/// page, tables and code blocks are painted in RED's colors and pinned to its
/// light/dark; without one, fall back to [`REPORT_STYLE`] (built-in, OS-driven).
fn report_style(theme: Option<&ReportTheme>) -> String {
    let Some(th) = theme else {
        return REPORT_STYLE.to_string();
    };
    let scheme = if th.is_dark { "dark" } else { "light" };
    format!(
        "<style>:root{{color-scheme:{scheme}}}*{{box-sizing:border-box}}\
         body{{margin:0;padding:32px 24px;max-width:1100px;margin-inline:auto;\
         font:15px/1.6 -apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,sans-serif;\
         background:{bg};color:{fg}}}\
         h1{{font-size:22px}}h2{{font-size:17px;margin-top:1.6em}}a{{color:{accent}}}\
         table{{border-collapse:collapse;width:100%;margin:12px 0;font-variant-numeric:tabular-nums}}\
         th,td{{padding:7px 12px;text-align:left;border-bottom:1px solid {border}}}\
         th{{background:{surface};font-weight:600}}\
         tbody tr:nth-child(even){{background:{hover}}}\
         code,pre{{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;background:{surface};border-radius:4px}}\
         code{{padding:1px 5px}}pre{{padding:12px;overflow:auto}}</style>",
        bg = th.bg,
        fg = th.fg,
        accent = th.accent,
        border = th.border,
        surface = th.surface,
        hover = th.hover,
    )
}
/// Serialize the theme into the report's inert data payload so the chart/table/
/// filter renderer paints in the same colors. Built by hand (rather than deriving
/// `Serialize`) to keep `ReportTheme` a plain data type and the key names explicit.
fn report_theme_json(theme: Option<&ReportTheme>) -> Json {
    match theme {
        None => Json::Null,
        Some(th) => json!({
            "is_dark": th.is_dark,
            "bg": th.bg,
            "surface": th.surface,
            "fg": th.fg,
            "muted": th.muted,
            "border": th.border,
            "grid": th.grid,
            "hover": th.hover,
            "accent": th.accent,
            "ring": th.ring,
            "palette": th.palette,
        }),
    }
}
/// Write a finished report to `path`, owner-readable only (`0600` on Unix). A
/// report can carry real query data, and on a shared temp dir (Linux `/tmp`) a
/// world-readable file would let another local user read it, so restrict it at
/// creation rather than writing world-readable and tightening after.
fn write_report_file(path: &Path, html: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path)?;
    file.write_all(html.as_bytes())
}
/// The trusted in-report chart bundle: Chart.js v4 (UMD, minified) + our renderer
/// (`assets/report-renderer.js`). This is the ONLY code allowed to run in a report;
/// it is injected behind a per-report CSP nonce, so the model's HTML and the
/// chart specs (which never carry the nonce) cannot execute. See `assets/README.md`
/// to regenerate after a Chart.js bump.
const REPORT_CHARTS_JS: &str = include_str!("../../assets/report-charts.js");
/// Wrap an AI-authored report body in a sandboxed, themed HTML document (Feature C).
/// The safety boundary is a strict Content-Security-Policy: `default-src 'none'`
/// blocks ALL scripts (inline and remote), remote fetches, and remote
/// images/CSS/fonts/frames; `style-src 'unsafe-inline'` allows the model's inline
/// styling; `img-src data:` allows inline (data-URI) images and SVG. So even if the
/// body (or a value injected from the data) smuggles a `<script>` or a remote URL,
/// the browser neither runs nor loads it. `<script>` blocks are also stripped
/// defensively, belt-and-suspenders.
///
/// When the model supplies `charts` or `data`, the report gains interactivity:
/// the specs/datasets/filters are embedded as inert `application/json` DATA the
/// model authors, and our trusted bundle (the only thing carrying the CSP `nonce`)
/// renders interactive charts (Chart.js), filterable/sortable tables over the
/// embedded `data`, and a report-wide filter bar (`filters`) that slices every
/// table and bound chart at once. The CSP keeps the hole tight: scripts run only with the nonce
/// (so the model cannot inject runnable code), and `connect-src 'none'` denies all
/// network egress (so even the trusted bundle cannot exfiltrate the data, and all
/// filtering happens client-side over what's already embedded, never a callback
/// to the database). The payload is pure data; the bundle never evals it and
/// writes every table cell via `textContent`.
fn wrap_report_html(
    title: Option<&str>,
    body: &str,
    charts: &[Json],
    data: Option<&Json>,
    filters: &[Json],
    theme: Option<&ReportTheme>,
) -> String {
    let title = title
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or("RED — report");
    let t = red_driver::html_escape(title);
    let safe_body = strip_scripts(body);
    // The base document style: RED's active theme if the UI supplied one, else
    // the built-in light/dark (follows the OS).
    let style = report_style(theme);

    let has_data = data
        .and_then(Json::as_object)
        .is_some_and(|o| !o.is_empty());
    if charts.is_empty() && !has_data {
        return format!(
            "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
             <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
             <meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; \
             style-src 'unsafe-inline'; img-src data:\">\
             <title>{t}</title>{style}</head><body>{safe_body}</body></html>\n"
        );
    }

    // Unguessable per-report nonce: only our bundle carries it, so a `<script>`
    // smuggled through the body or a spec value has no valid nonce and won't run.
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let payload = json!({
        "charts": charts,
        "data": data.cloned().unwrap_or(Json::Null),
        "filters": filters,
        "theme": report_theme_json(theme),
    })
    .to_string();
    // Neutralize `</script>` breakout from the inert data block; `<` parses
    // back to `<` under JSON.parse, so the data round-trips intact.
    let data = payload.replace('<', "\\u003c");
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; \
         script-src 'nonce-{nonce}'; style-src 'unsafe-inline'; img-src data:; \
         connect-src 'none'\">\
         <title>{t}</title>{style}</head><body>{safe_body}\
         <script id=\"red-report-data\" type=\"application/json\">{data}</script>\
         <script nonce=\"{nonce}\">{REPORT_CHARTS_JS}</script></body></html>\n"
    )
}
/// Remove `<script>…</script>` blocks (case-insensitive) from `html`. Defensive
/// only (the report's CSP already forbids script execution); this just keeps the
/// rendered document clean. An unterminated `<script` drops the remainder.
fn strip_scripts(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len());
    let mut i = 0;
    while i < html.len() {
        if lower[i..].starts_with("<script") {
            match lower[i..].find("</script>") {
                Some(rel) => {
                    i += rel + "</script>".len();
                    continue;
                }
                None => break,
            }
        }
        // `i` advances only by whole chars (`ch.len_utf8()` below) or past a
        // matched ASCII `</script>`, so it always sits on a UTF-8 boundary inside
        // the `i < html.len()` guard — there is always a next char.
        #[allow(
            clippy::expect_used,
            reason = "i is maintained on a char boundary; see comment"
        )]
        let ch = html[i..]
            .chars()
            .next()
            .expect("i sits on a char boundary within bounds");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Event;
    use crate::ai::sql::run_tool;
    use crate::protocol::ConversationId;
    use red_ai::CancelToken;
    use red_core::AiPolicy;
    use red_core::sql::Dialect;
    use red_driver::DatabaseDriver;
    use serde_json::json;
    use std::sync::Arc;

    #[tokio::test]
    async fn generate_report_wraps_ai_html_and_announces_it() {
        use futures::StreamExt;

        // generate_report renders model-authored HTML (no DB call). A no-op driver is
        // enough (the tool never touches it).
        let db = std::env::temp_dir().join(format!("red-gr-{}.db", uuid::Uuid::new_v4().simple()));
        let driver: Arc<dyn DatabaseDriver> = Arc::new(red_driver::SqliteDriver::new(db, true));
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let sink = ReportSink::new(tx, None, ConversationId::new(7), None, None);

        let (content, ok) = run_tool(
            &driver,
            Dialect::Sqlite,
            "generate_report",
            &json!({
                "title": "Widgets",
                "html": "<h1>Top widgets</h1><p>alpha leads beta.</p>\
                         <script>fetch('http://evil')</script>",
            }),
            &AiPolicy::default(),
            &CancelToken::new(),
            &sink,
        )
        .await;
        assert!(ok, "expected success, got: {content}");
        assert!(content.contains("Generated the report"));

        let (_session, event) = rx.next().await.expect("an AiReportReady event");
        let Event::AiReportReady {
            conversation_id,
            path,
            ..
        } = event
        else {
            panic!("expected AiReportReady");
        };
        assert_eq!(conversation_id.get(), 7);
        let html = std::fs::read_to_string(&path).unwrap();
        assert!(html.starts_with("<!doctype html>"));
        // The model's body is present and the title is carried through.
        assert!(html.contains("<h1>Top widgets</h1>"));
        assert!(html.contains("Widgets"));
        // Sandboxed: a strict CSP is set and the smuggled <script> is stripped.
        assert!(html.contains("Content-Security-Policy"));
        assert!(!html.contains("<script>"));
        assert!(!html.contains("evil"));

        // An empty body is refused, and nothing is announced.
        let (_content, ok) = run_tool(
            &driver,
            Dialect::Sqlite,
            "generate_report",
            &json!({ "html": "   " }),
            &AiPolicy::default(),
            &CancelToken::new(),
            &sink,
        )
        .await;
        assert!(!ok);
        // Nothing announced: the channel is empty but still open (Err), not an item.
        assert!(rx.try_recv().is_err(), "a refused report must not announce");
    }

    #[tokio::test]
    async fn generate_report_writes_to_the_configured_folder() {
        use futures::StreamExt;

        let db =
            std::env::temp_dir().join(format!("red-grd2-{}.db", uuid::Uuid::new_v4().simple()));
        let driver: Arc<dyn DatabaseDriver> = Arc::new(red_driver::SqliteDriver::new(db, true));
        // A folder that doesn't exist yet: `output_dir` must create it on demand rather
        // than dropping the report into the temp dir.
        let out =
            std::env::temp_dir().join(format!("red-reports-{}", uuid::Uuid::new_v4().simple()));
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let sink = ReportSink::new(tx, None, ConversationId::new(21), None, Some(out.clone()));

        let (_content, ok) = run_tool(
            &driver,
            Dialect::Sqlite,
            "generate_report",
            &json!({ "title": "Here", "html": "<h1>Here</h1>" }),
            &AiPolicy::default(),
            &CancelToken::new(),
            &sink,
        )
        .await;
        assert!(ok, "expected the report to be generated");

        let (_session, event) = rx.next().await.expect("an AiReportReady event");
        let Event::AiReportReady { path, .. } = event else {
            panic!("expected AiReportReady");
        };
        assert!(
            std::path::Path::new(&path).starts_with(&out),
            "report {path} should live under the configured folder {}",
            out.display()
        );
        assert!(
            out.is_dir(),
            "the configured folder should be created on demand"
        );
        let _ = std::fs::remove_dir_all(&out);
    }

    #[tokio::test]
    async fn generate_report_with_charts_is_nonce_gated_and_egress_free() {
        use futures::StreamExt;

        let db = std::env::temp_dir().join(format!("red-grc-{}.db", uuid::Uuid::new_v4().simple()));
        let driver: Arc<dyn DatabaseDriver> = Arc::new(red_driver::SqliteDriver::new(db, true));
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let sink = ReportSink::new(tx, None, ConversationId::new(11), None, None);

        let (content, ok) = run_tool(
            &driver,
            Dialect::Sqlite,
            "generate_report",
            &json!({
                "title": "Sales",
                "html": "<h1>Sales</h1><div data-red-chart=\"0\"></div>",
                "charts": [
                    {
                        "type": "bar",
                        // A label that tries to break out of the data block.
                        "data": { "labels": ["</script><script>alert(1)</script>"],
                                  "datasets": [{ "label": "Q1", "data": [3] }] },
                    },
                    // Non-object entries are dropped, not embedded.
                    "not-a-chart",
                ],
            }),
            &AiPolicy::default(),
            &CancelToken::new(),
            &sink,
        )
        .await;
        assert!(ok, "expected success, got: {content}");

        let (_session, event) = rx.next().await.expect("an AiReportReady event");
        let Event::AiReportReady { path, .. } = event else {
            panic!("expected AiReportReady");
        };
        let html = std::fs::read_to_string(&path).unwrap();

        // The chart hole is tight: scripts run only with the nonce, and there is
        // zero network egress so the bundle cannot leak the data it charts.
        assert!(html.contains("script-src 'nonce-"));
        assert!(html.contains("connect-src 'none'"));
        // The trusted bundle is injected behind the nonce; the inert data block is not.
        assert!(html.contains("<script nonce="));
        assert!(html.contains("Chart.js v4"));
        assert!(html.contains("id=\"red-report-data\" type=\"application/json\""));
        // The breakout attempt is neutralized: no stray executable <script> from
        // the data, and the `<` is escaped to its JSON unicode form.
        assert!(!html.contains("<script>alert(1)</script>"));
        assert!(html.contains("\\u003c/script>"));
        // Non-object chart entries are filtered out of the embedded payload.
        assert!(!html.contains("not-a-chart"));
    }

    #[tokio::test]
    async fn generate_report_with_data_embeds_datasets_for_interactive_tables() {
        use futures::StreamExt;

        let db = std::env::temp_dir().join(format!("red-grd-{}.db", uuid::Uuid::new_v4().simple()));
        let driver: Arc<dyn DatabaseDriver> = Arc::new(red_driver::SqliteDriver::new(db, true));
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let sink = ReportSink::new(tx, None, ConversationId::new(13), None, None);

        let (content, ok) = run_tool(
            &driver,
            Dialect::Sqlite,
            "generate_report",
            &json!({
                "title": "Sales",
                "html": "<h1>Sales</h1><div data-red-table=\"sales\"></div>",
                "data": {
                    "sales": {
                        "columns": ["Month", "Region", "Revenue"],
                        "rows": [["Jan", "NA", 120], ["Feb", "EU", 90]],
                    },
                },
            }),
            &AiPolicy::default(),
            &CancelToken::new(),
            &sink,
        )
        .await;
        assert!(ok, "expected success, got: {content}");

        let (_session, event) = rx.next().await.expect("an AiReportReady event");
        let Event::AiReportReady { path, .. } = event else {
            panic!("expected AiReportReady");
        };
        let html = std::fs::read_to_string(&path).unwrap();

        // `data` alone (no charts) still triggers the interactive, no-egress shell.
        assert!(html.contains("script-src 'nonce-"));
        assert!(html.contains("connect-src 'none'"));
        assert!(html.contains("<script nonce="));
        // The dataset is embedded as inert data for client-side filtering.
        assert!(html.contains("id=\"red-report-data\" type=\"application/json\""));
        assert!(html.contains("\"sales\""));
        assert!(html.contains("Revenue"));
    }

    #[tokio::test]
    async fn generate_report_embeds_report_wide_filters() {
        use futures::StreamExt;

        let db = std::env::temp_dir().join(format!("red-grf-{}.db", uuid::Uuid::new_v4().simple()));
        let driver: Arc<dyn DatabaseDriver> = Arc::new(red_driver::SqliteDriver::new(db, true));
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let sink = ReportSink::new(tx, None, ConversationId::new(17), None, None);

        let (content, ok) = run_tool(
            &driver,
            Dialect::Sqlite,
            "generate_report",
            &json!({
                "title": "Sales",
                "html": "<h1>Sales</h1><div data-red-table=\"sales\"></div>",
                "data": {
                    "sales": {
                        "columns": ["Month", "Region", "Revenue"],
                        "rows": [["Jan", "NA", 120], ["Feb", "EU", 90]],
                    },
                },
                "filters": [
                    { "column": "Region", "type": "multiselect" },
                    "not-an-object",
                ],
            }),
            &AiPolicy::default(),
            &CancelToken::new(),
            &sink,
        )
        .await;
        assert!(ok, "expected success, got: {content}");

        let (_session, event) = rx.next().await.expect("an AiReportReady event");
        let Event::AiReportReady { path, .. } = event else {
            panic!("expected AiReportReady");
        };
        let html = std::fs::read_to_string(&path).unwrap();
        // The filter definition rides in the inert payload (non-object dropped).
        assert!(html.contains("\"filters\""));
        assert!(html.contains("multiselect"));
        assert!(!html.contains("not-an-object"));
        assert!(html.contains("connect-src 'none'"));
    }
}
