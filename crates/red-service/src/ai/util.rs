//! Small helpers every seam's tools share: the caps that keep one tool result
//! from ballooning the model's context, the timeout that bounds one driver call,
//! and the two formatting primitives (`fmt_bytes`, `truncate_summary`) that
//! would otherwise be reimplemented three times.

use std::time::Duration;

use red_core::RedError;
use red_driver::AbortSignal;

use crate::protocol::AiContext;

/// Race a one-shot tool fetch against the policy's statement timeout. On expiry,
/// fire the fetch's [`AbortSignal`] so the engine stops, then surface
/// [`RedError::Timeout`]. A `0` timeout never fires. Mirrors the dispatch loop's
/// `with_timeout` so the AI path bounds queries the same way human paging does.
pub(in crate::ai) async fn guard_timeout<T>(
    timeout_ms: u64,
    abort: &AbortSignal,
    fut: impl std::future::Future<Output = red_core::Result<T>>,
) -> red_core::Result<T> {
    tokio::pin!(fut);
    let mut timed_out = false;
    let out = loop {
        tokio::select! {
            res = &mut fut => break res,
            _ = sleep_ms(timeout_ms), if !timed_out && timeout_ms != 0 => {
                timed_out = true;
                abort.abort();
            }
        }
    };
    match out {
        Err(RedError::Interrupted) if timed_out => Err(RedError::Timeout),
        other => other,
    }
}
/// Sleep `ms` milliseconds, or never (a `0` timeout means "no cap").
async fn sleep_ms(ms: u64) {
    if ms == 0 {
        std::future::pending::<()>().await
    } else {
        tokio::time::sleep(Duration::from_millis(ms)).await
    }
}
/// Cap one tool result at `max` bytes so a wide/long result can't balloon the
/// model's context. Truncates on a char boundary and appends a note. `0` disables.
pub(in crate::ai) fn cap_result_bytes(mut content: String, max: usize) -> String {
    if max == 0 || content.len() <= max {
        return content;
    }
    let mut cut = max;
    while cut > 0 && !content.is_char_boundary(cut) {
        cut -= 1;
    }
    content.truncate(cut);
    content.push_str("\n…(result truncated: it exceeded the size cap; narrow the query)");
    content
}
/// Truncate to `max` chars on a char boundary, appending an ellipsis when cut.
pub(in crate::ai) fn truncate_summary(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
}
/// Coarse human byte count for the agent's text output, shared by every seam's
/// formatter.
pub(in crate::ai) fn fmt_bytes(n: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    if n >= MB {
        format!("{:.1}MB", n as f64 / MB as f64)
    } else if n >= KB {
        format!("{:.1}KB", n as f64 / KB as f64)
    } else {
        format!("{n}B")
    }
}
/// Append the grounding footer every seam's system prompt shares: the live
/// connection line and, when set, the read-only notice. One place so the footer
/// can't drift per seam; the seam passes its already-built body and its own
/// read-only wording (SQL names the blocked ops; KV/doc keep it terse). SQL's
/// schema overview is appended by its caller afterward, since only SQL has one.
pub(in crate::ai) fn finish_system_prompt(
    mut body: String,
    ctx: &AiContext,
    read_only_note: &str,
) -> String {
    if !ctx.connection.is_empty() {
        body.push_str(&format!("\nConnected to: {}", ctx.connection));
    }
    if ctx.read_only {
        body.push('\n');
        body.push_str(read_only_note);
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_byte_cap_truncates_on_char_boundary() {
        // Under the cap: returned verbatim.
        assert_eq!(cap_result_bytes("hello".into(), 10), "hello");
        // `0` disables the cap.
        assert_eq!(cap_result_bytes("hello".into(), 0), "hello");
        // A multi-byte string capped mid-codepoint truncates at the boundary below
        // the cap (never splitting a char) and notes the truncation.
        let capped = cap_result_bytes("ééééé".into(), 5);
        assert!(capped.starts_with("éé")); // 4 bytes ≤ 5; the 3rd 'é' would cross it
        assert!(capped.contains("result truncated"));
    }

    #[test]
    fn summary_truncation_is_char_safe_and_marked() {
        let long = "x".repeat(200);
        let out = truncate_summary(&long, 80);
        assert_eq!(out.chars().count(), 80);
        assert!(out.ends_with('…'));
        // Multibyte input never splits a codepoint.
        let emoji = "😀".repeat(100);
        let out = truncate_summary(&emoji, 10);
        assert_eq!(out.chars().count(), 10);
    }
}
