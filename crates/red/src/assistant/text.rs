//! Pure domain helpers for the assistant panel: slash-command matching, config
//! defaulting, the `/report` shortcut, title/transcript derivation, the bubble
//! element key, SQL extraction, schema summarising, and the report-theme snapshot.
//! These carry no `AppState` or rendering knowledge (just data in, data out), so
//! they're unit-tested in isolation here.

use gpui::SharedString;

/// Cap on schema objects folded into the grounding summary, so a database with
/// thousands of tables doesn't blow the context window. The model pulls full
/// detail on demand via `describe_table`, so a names-only overview is enough.
const SCHEMA_SUMMARY_CAP: usize = 200;

/// Cap on a derived chat title's length (characters), so a long first message
/// makes a sensible name rather than a wall of text in the picker.
const TITLE_CAP: usize = 60;

/// Cap on the prior-transcript digest folded back into a reopened chat's first
/// turn (M-S5), so resuming a long conversation doesn't blow the context window.
/// Keeps the most recent turns (the tail), which is what a follow-up references.
const SEED_CAP: usize = 6_000;

/// Candidate slash-command names for the composer's completion popup, or empty when
/// the word under the cursor isn't a slash command. A slash command is a `/` at the
/// start of the input (or after whitespace) followed by the in-progress name; the
/// returned candidate is the bare name (the editor keeps the typed `/`). The word
/// boundary matches the editor's own (alphanumeric + `_`), so the accepted candidate
/// replaces exactly the typed name.
pub(super) fn slash_candidates(
    commands: &[red_service::AiCommand],
    text: &str,
    cursor: usize,
) -> Vec<SharedString> {
    if commands.is_empty() {
        return Vec::new();
    }
    let bytes = text.as_bytes();
    let cursor = cursor.min(bytes.len());
    // Walk back over the in-progress command name.
    let mut start = cursor;
    while start > 0 && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_') {
        start -= 1;
    }
    // The char before the name must be `/`, and that `/` must open the input or
    // follow whitespace, so "and/or" or a file path never triggers the picker.
    if start == 0 || bytes[start - 1] != b'/' {
        return Vec::new();
    }
    let slash = start - 1;
    if slash > 0 && !bytes[slash - 1].is_ascii_whitespace() {
        return Vec::new();
    }
    let prefix = text[start..cursor].to_ascii_lowercase();
    commands
        .iter()
        .filter(|c| c.name.to_ascii_lowercase().starts_with(&prefix))
        .map(|c| SharedString::from(c.name.clone()))
        .collect()
}

/// Map a `red_service::AiConfigCategory` to/from the lowercase string persisted in
/// `state.json`, so a future category doesn't break older files (unknown → `Other`).
fn category_to_str(cat: red_service::AiConfigCategory) -> &'static str {
    match cat {
        red_service::AiConfigCategory::Model => "model",
        red_service::AiConfigCategory::Reasoning => "reasoning",
        red_service::AiConfigCategory::Mode => "mode",
        red_service::AiConfigCategory::Other => "other",
    }
}

fn category_from_str(s: &str) -> red_service::AiConfigCategory {
    match s {
        "model" => red_service::AiConfigCategory::Model,
        "reasoning" => red_service::AiConfigCategory::Reasoning,
        "mode" => red_service::AiConfigCategory::Mode,
        _ => red_service::AiConfigCategory::Other,
    }
}

/// A live config-option set as the serde-friendly shape persisted per agent, so the
/// composer can redraw the model/reasoning dropdowns on the next launch before a
/// chat opens its session.
pub(super) fn to_stored(
    options: &[red_service::AiConfigOption],
) -> Vec<crate::local_state::StoredConfigOption> {
    options
        .iter()
        .map(|o| crate::local_state::StoredConfigOption {
            id: o.id.clone(),
            name: o.name.clone(),
            category: category_to_str(o.category).to_string(),
            current_value: o.current_value.clone(),
            choices: o
                .choices
                .iter()
                .map(|c| crate::local_state::StoredConfigChoice {
                    value: c.value.clone(),
                    name: c.name.clone(),
                    description: c.description.clone(),
                })
                .collect(),
        })
        .collect()
}

/// The inverse of [`to_stored`]: a persisted config-option set back into the live
/// `red_service` shape the composer renders.
pub(super) fn from_stored(
    options: &[crate::local_state::StoredConfigOption],
) -> Vec<red_service::AiConfigOption> {
    options
        .iter()
        .map(|o| red_service::AiConfigOption {
            id: o.id.clone(),
            name: o.name.clone(),
            category: category_from_str(&o.category),
            current_value: o.current_value.clone(),
            choices: o
                .choices
                .iter()
                .map(|c| red_service::AiConfigChoice {
                    value: c.value.clone(),
                    name: c.name.clone(),
                    description: c.description.clone(),
                })
                .collect(),
        })
        .collect()
}

/// Which config selectors a fresh session should switch to honor the central
/// defaults: for each Model/Reasoning option whose stored default is non-empty, an
/// advertised choice, and not already current, the `(config_id, value)` to apply.
/// Options without a stored default (or already on it) are left as the agent set them.
pub(super) fn default_config_changes(
    options: &[red_service::AiConfigOption],
    model_default: &str,
    reasoning_default: &str,
    mode_default: &str,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for opt in options {
        let default = match opt.category {
            red_service::AiConfigCategory::Model => model_default,
            red_service::AiConfigCategory::Reasoning => reasoning_default,
            red_service::AiConfigCategory::Mode => mode_default,
            _ => continue,
        };
        if default.is_empty() || default == opt.current_value {
            continue;
        }
        if opt.choices.iter().any(|c| c.value == default) {
            out.push((opt.id.clone(), default.to_string()));
        }
    }
    out
}

/// An `Hsla` as a CSS color string (`hsl(h s% l%)`, with `/ a` when translucent),
/// so the active theme's tokens can be dropped straight into a report's CSS.
fn css_color(c: gpui::Hsla) -> String {
    let (h, s, l) = (c.h * 360.0, c.s * 100.0, c.l * 100.0);
    if c.a >= 0.999 {
        format!("hsl({h:.1} {s:.1}% {l:.1}%)")
    } else {
        format!("hsl({h:.1} {s:.1}% {l:.1}% / {:.3})", c.a)
    }
}

/// Snapshot the active Flint theme into a [`red_service::ReportTheme`] so an
/// AI-generated report is painted in Red's current palette (page, tables, chart
/// cards, filter bar) instead of a generic light/dark document. The categorical
/// chart palette is pulled from the theme's semantic colors, led by the accent.
pub(super) fn report_theme(t: &flint::Theme) -> red_service::ReportTheme {
    red_service::ReportTheme {
        is_dark: t.bg_app.l < 0.5,
        bg: css_color(t.bg_app),
        surface: css_color(t.bg_elevated),
        fg: css_color(t.text),
        muted: css_color(t.text_muted),
        border: css_color(t.border),
        grid: css_color(t.border_soft),
        hover: css_color(t.bg_hover),
        accent: css_color(t.accent),
        ring: css_color(t.accent_ghost),
        palette: vec![
            css_color(t.accent),
            css_color(t.blue),
            css_color(t.green),
            css_color(t.orange),
            css_color(t.purple),
            css_color(t.cyan),
            css_color(t.yellow),
        ],
    }
}

/// Expand a `/report …` composer shortcut into an explicit instruction so the agent
/// reads the data and calls `generate_report`. Returns `None` for a non-`/report`
/// message (sent verbatim). A bare `/report` still asks for a report of whatever's
/// in context; `/reporting` (no separator) is not matched.
pub(super) fn expand_slash_report(message: &str) -> Option<String> {
    let rest = message.strip_prefix("/report")?;
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let topic = rest.trim();
    let ask = if topic.is_empty() {
        "Create an HTML report for me.".to_string()
    } else {
        format!("Create an HTML report about: {topic}")
    };
    Some(format!(
        "{ask}\n\nRead the data you need with run_select, then call the generate_report tool with \
         the report written as HTML: a heading and a short summary. Where a visual helps, add \
         interactive charts via the `charts` argument and reference them with \
         <div data-red-chart=\"INDEX\"></div> placeholders. If the user would benefit from \
         exploring the rows, put the data in the `data` argument as a named dataset and drop a \
         <div data-red-table=\"NAME\"></div> placeholder for a searchable/sortable table (and bind \
         charts to that dataset so filters update them). Open it for me."
    ))
}

/// A one-line title from a chat's first user message: the first non-empty line,
/// whitespace-collapsed and capped. Used as the saved file's display name.
pub(super) fn derive_title(message: &str) -> String {
    let line = message
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let collapsed = line.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() > TITLE_CAP {
        let truncated: String = collapsed.chars().take(TITLE_CAP).collect();
        format!("{}…", truncated.trim_end())
    } else if collapsed.is_empty() {
        "Untitled chat".to_string()
    } else {
        collapsed
    }
}

/// Render a saved transcript as a compact `You:` / `Assistant:` digest to seed a
/// reopened chat's next turn (M-S5). Returns `None` for an empty transcript. The
/// digest is capped to its tail ([`SEED_CAP`]), the recent turns a follow-up
/// actually depends on, so resuming a long chat stays within budget.
pub(super) fn render_transcript(
    messages: &[crate::conversations::StoredMessage],
) -> Option<String> {
    let mut out = String::new();
    for m in messages {
        let text = m.text.trim();
        if text.is_empty() {
            continue;
        }
        let who = if m.role == "assistant" {
            "Agent"
        } else {
            "You"
        };
        out.push_str(who);
        out.push_str(": ");
        out.push_str(text);
        out.push_str("\n\n");
    }
    let trimmed = out.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Keep the tail if it's over budget, on a turn-ish boundary where possible.
    if trimmed.len() > SEED_CAP {
        // Step the start forward to a UTF-8 char boundary so slicing can't panic.
        let mut start = trimmed.len() - SEED_CAP;
        while start < trimmed.len() && !trimmed.is_char_boundary(start) {
            start += 1;
        }
        let slice = &trimmed[start..];
        let cut = slice.find("\n\n").map(|i| i + 2).unwrap_or(0);
        return Some(format!("…(earlier turns omitted)\n\n{}", &slice[cut..]));
    }
    Some(trimmed.to_string())
}

/// A stable element key for a bubble: its index in the transcript. Bubbles are
/// rendered in order and the panel rebuilds each frame, so the index is unique
/// among the currently-shown bubbles; and, unlike a content-length hash, it never
/// collides for equal-length messages (which would break per-bubble chip routing).
pub(super) fn bubble_key(index: usize) -> usize {
    index
}

/// Pull the first fenced ```sql block out of an assistant message, if any.
pub(super) fn extract_sql(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    let start = lower.find("```sql")?;
    let after = start + "```sql".len();
    let rest = &text[after..];
    let body_start = rest.find('\n')? + 1;
    let body = &rest[body_start..];
    let end = body.find("```")?;
    let sql = body[..end].trim();
    if sql.is_empty() {
        None
    } else {
        Some(sql.to_string())
    }
}

/// A compact `schema.table` overview for the system prompt, capped so a huge
/// database stays within budget. Full per-table detail is fetched on demand by
/// the model's `describe_table` tool.
pub(super) fn summarize_schema(schemas: &[red_core::SchemaMeta]) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let mut shown = 0usize;
    let mut total = 0usize;
    for sch in schemas {
        for obj in &sch.objects {
            total += 1;
            if shown < SCHEMA_SUMMARY_CAP {
                let _ = writeln!(out, "{}.{}", sch.name, obj.name);
                shown += 1;
            }
        }
    }
    if total > shown {
        let _ = write!(out, "… and {} more objects", total - shown);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistant::{ChatSession, QuickAction};

    #[test]
    fn central_defaults_apply_only_when_valid_and_different() {
        let choice = |v: &str| red_service::AiConfigChoice {
            value: v.into(),
            name: v.into(),
            description: None,
        };
        let model = red_service::AiConfigOption {
            id: "model".into(),
            name: "Model".into(),
            category: red_service::AiConfigCategory::Model,
            current_value: "auto".into(),
            choices: vec![choice("auto"), choice("opus"), choice("haiku")],
        };
        let reasoning = red_service::AiConfigOption {
            id: "reasoning".into(),
            name: "Reasoning".into(),
            category: red_service::AiConfigCategory::Reasoning,
            current_value: "default".into(),
            choices: vec![choice("default"), choice("hard")],
        };
        let mode = red_service::AiConfigOption {
            id: "mode".into(),
            name: "Mode".into(),
            category: red_service::AiConfigCategory::Mode,
            current_value: "default".into(),
            choices: vec![choice("default"), choice("acceptEdits"), choice("bypass")],
        };
        let opts = vec![model, reasoning, mode];

        // A valid, different model default applies; empty reasoning/mode defaults are left.
        assert_eq!(
            default_config_changes(&opts, "opus", "", ""),
            vec![("model".to_string(), "opus".to_string())]
        );
        // All three apply when each differs.
        assert_eq!(
            default_config_changes(&opts, "haiku", "hard", "acceptEdits"),
            vec![
                ("model".to_string(), "haiku".to_string()),
                ("reasoning".to_string(), "hard".to_string()),
                ("mode".to_string(), "acceptEdits".to_string()),
            ]
        );
        // A default equal to the current pick is a no-op; an unknown value is ignored.
        assert!(default_config_changes(&opts, "auto", "nonexistent", "default").is_empty());
        // No stored defaults → nothing to apply.
        assert!(default_config_changes(&opts, "", "", "").is_empty());
    }

    #[test]
    fn slash_picker_triggers_only_in_command_position() {
        let cmds = vec![
            red_service::AiCommand {
                name: "login".into(),
                description: "Sign in".into(),
            },
            red_service::AiCommand {
                name: "logout".into(),
                description: "Sign out".into(),
            },
            red_service::AiCommand {
                name: "clear".into(),
                description: "Reset".into(),
            },
        ];
        let names = |t: &str, c: usize| -> Vec<String> {
            slash_candidates(&cmds, t, c)
                .into_iter()
                .map(|s| s.to_string())
                .collect()
        };
        // A bare `/` offers everything; the prefix filters.
        assert_eq!(names("/", 1), vec!["login", "logout", "clear"]);
        assert_eq!(names("/lo", 3), vec!["login", "logout"]);
        assert_eq!(names("/cle", 4), vec!["clear"]);
        // After whitespace mid-message still counts as command position.
        assert_eq!(names("hi /lo", 6), vec!["login", "logout"]);
        // A slash glued to a preceding word (path, and/or) does not trigger.
        assert!(names("and/lo", 6).is_empty());
        // No match, and no commands → empty.
        assert!(names("/xyz", 4).is_empty());
        assert!(slash_candidates(&[], "/lo", 3).is_empty());
    }

    #[test]
    fn extracts_first_sql_fence() {
        let md = "Here you go:\n```sql\nSELECT 1;\n```\nDone.";
        assert_eq!(extract_sql(md).as_deref(), Some("SELECT 1;"));
        assert_eq!(extract_sql("no code here"), None);
        assert_eq!(extract_sql("```sql\n\n```"), None);
    }

    #[test]
    fn title_is_first_line_collapsed_and_capped() {
        assert_eq!(derive_title("How many users?"), "How many users?");
        // Leading blank lines skipped; whitespace collapsed.
        assert_eq!(
            derive_title("\n\n  list   the  tables \n"),
            "list the tables"
        );
        // Over-long titles are truncated with an ellipsis.
        let long = "a ".repeat(80);
        let title = derive_title(&long);
        assert!(title.ends_with('…'));
        assert!(title.chars().count() <= TITLE_CAP + 1);
        // Empty input has a sensible fallback.
        assert_eq!(derive_title("   \n  "), "Untitled chat");
    }

    #[test]
    fn transcript_digest_renders_roles_and_skips_empties() {
        let msgs = vec![
            crate::conversations::StoredMessage {
                role: "user".into(),
                text: "hi".into(),
                thinking: String::new(),
                ..Default::default()
            },
            crate::conversations::StoredMessage {
                role: "assistant".into(),
                text: "hello".into(),
                thinking: "ignored".into(),
                ..Default::default()
            },
            crate::conversations::StoredMessage {
                role: "assistant".into(),
                text: "   ".into(),
                thinking: String::new(),
                ..Default::default()
            },
        ];
        let seed = render_transcript(&msgs).expect("non-empty");
        assert!(seed.contains("You: hi"));
        assert!(seed.contains("Agent: hello"));
        // Empty-text turns are skipped; thinking isn't seeded.
        assert!(!seed.contains("ignored"));
        // An all-empty transcript yields nothing to seed.
        assert!(render_transcript(&[]).is_none());
    }

    #[test]
    fn quick_action_prompt_is_nonempty() {
        assert!(!QuickAction::ExplainError.prompt().trim().is_empty());
    }

    #[test]
    fn chat_carries_its_agent_binding() {
        // The chat stores the agent id verbatim; a turn carries it to the backend,
        // which resolves the kind (the panel no longer maps it). Any id round-trips.
        for id in ["anthropic", "subscription", "codex", "local"] {
            let chat = ChatSession::new(0, id.to_string());
            assert_eq!(chat.provider, id);
        }
    }
}
