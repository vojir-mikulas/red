//! The Claude Messages API provider: one streamed turn per `stream_turn` call,
//! parsed straight off the SSE wire. Request bodies put the (stable) system
//! prompt and tool catalog behind a `cache_control` breakpoint so long sessions
//! re-read them from cache instead of re-billing them every turn.

use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value as Json, json};
use tokio::sync::mpsc::UnboundedSender;

use crate::types::{
    AiError, ContentBlock, Delta, Message, Result, Role, StopReason, TurnOutcome, TurnRequest,
    Usage,
};
use crate::{AiProvider, CancelToken};

const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// Whether `base_url` is safe to send the API key to. Every request carries the
/// key in the `x-api-key` header, so a `base_url` override (a config field) must
/// not be allowed to retarget that credential to an arbitrary host over cleartext.
/// HTTPS is allowed anywhere; plain HTTP only to loopback (a local proxy / test).
pub fn is_safe_base_url(base_url: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(base_url) else {
        return false;
    };
    match url.scheme() {
        "https" => true,
        // url strips the brackets from an IPv6 host, so match the bare form too.
        "http" => matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "::1")),
        _ => false,
    }
}

/// Claude provider. Holds a reused `reqwest::Client` and the API key (never
/// logged). `base_url` is overridable so a test or a proxy can point elsewhere.
pub struct AnthropicProvider {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
}

impl AnthropicProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key: api_key.into(),
            base_url: DEFAULT_BASE_URL.to_string(),
        }
    }

    /// Override the endpoint (tests / OpenAI-compatible proxies that speak the
    /// Anthropic wire format).
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    fn build_body(&self, req: &TurnRequest) -> Json {
        let mut body = json!({
            "model": req.model,
            "max_tokens": req.max_tokens,
            "stream": true,
            "system": [{
                "type": "text",
                "text": req.system,
                "cache_control": { "type": "ephemeral" },
            }],
            "messages": req.messages.iter().map(message_to_wire).collect::<Vec<_>>(),
        });

        if req.show_thinking {
            body["thinking"] = json!({ "type": "adaptive", "display": "summarized" });
        }

        if !req.tools.is_empty() {
            let last = req.tools.len() - 1;
            let tools: Vec<Json> = req
                .tools
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    let mut tool = json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": t.input_schema,
                    });
                    // Cache the whole tool list (renders before `system`) by
                    // marking the last entry.
                    if i == last {
                        tool["cache_control"] = json!({ "type": "ephemeral" });
                    }
                    tool
                })
                .collect();
            body["tools"] = Json::Array(tools);
        }

        body
    }
}

#[async_trait]
impl AiProvider for AnthropicProvider {
    async fn stream_turn(
        &self,
        req: &TurnRequest,
        tx: &UnboundedSender<Delta>,
        cancel: &CancelToken,
    ) -> Result<TurnOutcome> {
        if self.api_key.is_empty() {
            return Err(AiError::MissingKey("anthropic".into()));
        }
        if cancel.is_cancelled() {
            return Err(AiError::Cancelled);
        }

        let body = self.build_body(req);
        let resp = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| AiError::Network(e.to_string()))?;

        let status = resp.status();
        if !status.is_success() {
            let detail = resp.text().await.unwrap_or_default();
            return Err(match status.as_u16() {
                401 | 403 => AiError::Auth,
                429 => AiError::RateLimited,
                _ => AiError::Provider(format!("{status}: {}", truncate(&detail, 400))),
            });
        }

        let mut acc = TurnAccumulator::default();
        // Accumulate raw bytes, not a String: a multibyte UTF-8 codepoint can be
        // split across two network chunks, so decoding each chunk on its own would
        // turn the boundary bytes into U+FFFD. We only decode whole SSE lines,
        // which always end on a codepoint boundary (the `\n`).
        let mut buf: Vec<u8> = Vec::new();
        let mut stream = resp.bytes_stream();

        while let Some(chunk) = stream.next().await {
            if cancel.is_cancelled() {
                return Err(AiError::Cancelled);
            }
            let chunk = chunk.map_err(|e| AiError::Network(e.to_string()))?;
            buf.extend_from_slice(&chunk);

            // SSE events are newline-delimited; keep the trailing partial line.
            while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
                let line_bytes: Vec<u8> = buf.drain(..=nl).collect();
                let line = String::from_utf8_lossy(&line_bytes);
                let line = line.trim_end_matches('\n').trim_end_matches('\r');
                let Some(data) = line.strip_prefix("data:") else {
                    continue; // skip `event:` lines and blanks; data carries `type`
                };
                let data = data.trim();
                if data.is_empty() {
                    continue;
                }
                let event: Json = match serde_json::from_str(data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                acc.handle_event(&event, tx);
            }
        }

        acc.finish()
    }
}

/// Accumulates streamed SSE events into a single assistant message.
#[derive(Default)]
struct TurnAccumulator {
    blocks: Vec<PartialBlock>,
    stop_reason: Option<StopReason>,
    usage: Usage,
    refused: bool,
}

/// A content block under construction, indexed by its SSE `index`.
enum PartialBlock {
    Text(String),
    Thinking {
        text: String,
        signature: String,
    },
    RedactedThinking(String),
    ToolUse {
        id: String,
        name: String,
        json: String,
    },
}

/// Upper bound on distinct SSE content-block indices we'll materialize. A real
/// turn has a handful of blocks; the cap turns a hostile/oversized `index` into a
/// dropped event instead of an unbounded allocation.
const MAX_CONTENT_BLOCKS: usize = 1024;

impl TurnAccumulator {
    fn handle_event(&mut self, event: &Json, tx: &UnboundedSender<Delta>) {
        match event.get("type").and_then(Json::as_str) {
            Some("message_start") => {
                if let Some(u) = event.pointer("/message/usage") {
                    self.usage.input_tokens = u_get(u, "input_tokens");
                    self.usage.cache_read_input_tokens = u_get(u, "cache_read_input_tokens");
                }
            }
            Some("content_block_start") => {
                let cb = event.get("content_block");
                let block = match cb.and_then(|c| c.get("type")).and_then(Json::as_str) {
                    Some("text") => PartialBlock::Text(String::new()),
                    Some("thinking") => PartialBlock::Thinking {
                        text: String::new(),
                        signature: String::new(),
                    },
                    Some("redacted_thinking") => PartialBlock::RedactedThinking(
                        cb.and_then(|c| c.get("data"))
                            .and_then(Json::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    ),
                    Some("tool_use") => {
                        let id = cb
                            .and_then(|c| c.get("id"))
                            .and_then(Json::as_str)
                            .unwrap_or_default()
                            .to_string();
                        let name = cb
                            .and_then(|c| c.get("name"))
                            .and_then(Json::as_str)
                            .unwrap_or_default()
                            .to_string();
                        let _ = tx.send(Delta::ToolUseStarted {
                            id: id.clone(),
                            name: name.clone(),
                        });
                        PartialBlock::ToolUse {
                            id,
                            name,
                            json: String::new(),
                        }
                    }
                    _ => PartialBlock::Text(String::new()),
                };
                let idx = event.get("index").and_then(Json::as_u64).unwrap_or(0) as usize;
                // `index` is off the SSE wire and the endpoint is a configurable /
                // proxied `base_url`; a hostile or buggy stream could send a huge
                // value and force a multi-GB `resize_with`. Real responses have a
                // handful of blocks, so drop an implausibly-indexed event rather
                // than allocate for it.
                if idx >= MAX_CONTENT_BLOCKS {
                    return;
                }
                if idx >= self.blocks.len() {
                    self.blocks
                        .resize_with(idx + 1, || PartialBlock::Text(String::new()));
                }
                self.blocks[idx] = block;
            }
            Some("content_block_delta") => {
                let idx = event.get("index").and_then(Json::as_u64).unwrap_or(0) as usize;
                let delta = event.get("delta");
                let dtype = delta.and_then(|d| d.get("type")).and_then(Json::as_str);
                let Some(block) = self.blocks.get_mut(idx) else {
                    return;
                };
                match dtype {
                    Some("text_delta") => {
                        if let (PartialBlock::Text(s), Some(t)) = (block, text_field(delta, "text"))
                        {
                            s.push_str(t);
                            let _ = tx.send(Delta::Text(t.to_string()));
                        }
                    }
                    Some("thinking_delta") => {
                        if let (PartialBlock::Thinking { text, .. }, Some(t)) =
                            (block, text_field(delta, "thinking"))
                        {
                            text.push_str(t);
                            let _ = tx.send(Delta::Thinking(t.to_string()));
                        }
                    }
                    Some("signature_delta") => {
                        if let (PartialBlock::Thinking { signature, .. }, Some(t)) =
                            (block, text_field(delta, "signature"))
                        {
                            signature.push_str(t);
                        }
                    }
                    Some("input_json_delta") => {
                        if let (PartialBlock::ToolUse { json, .. }, Some(t)) =
                            (block, text_field(delta, "partial_json"))
                        {
                            json.push_str(t);
                        }
                    }
                    _ => {}
                }
            }
            Some("message_delta") => {
                if let Some(sr) = event.pointer("/delta/stop_reason").and_then(Json::as_str) {
                    self.stop_reason = Some(map_stop(sr));
                    if sr == "refusal" {
                        self.refused = true;
                    }
                }
                if let Some(u) = event.get("usage") {
                    self.usage.output_tokens = u_get(u, "output_tokens");
                }
            }
            _ => {}
        }
    }

    fn finish(self) -> Result<TurnOutcome> {
        if self.refused {
            return Err(AiError::Refused);
        }
        let content: Vec<ContentBlock> = self
            .blocks
            .into_iter()
            .filter_map(|b| match b {
                PartialBlock::Text(t) if t.is_empty() => None,
                PartialBlock::Text(text) => Some(ContentBlock::Text { text }),
                PartialBlock::Thinking { text, signature } => {
                    Some(ContentBlock::Thinking { text, signature })
                }
                PartialBlock::RedactedThinking(data) => {
                    Some(ContentBlock::RedactedThinking { data })
                }
                PartialBlock::ToolUse { id, name, json } => {
                    let input = serde_json::from_str(&json).unwrap_or_else(|_| json!({}));
                    Some(ContentBlock::ToolUse { id, name, input })
                }
            })
            .collect();

        Ok(TurnOutcome {
            message: Message {
                role: Role::Assistant,
                content,
            },
            stop_reason: self.stop_reason.unwrap_or(StopReason::EndTurn),
            usage: self.usage,
        })
    }
}

fn map_stop(s: &str) -> StopReason {
    match s {
        "end_turn" | "stop_sequence" => StopReason::EndTurn,
        "tool_use" => StopReason::ToolUse,
        "max_tokens" => StopReason::MaxTokens,
        "refusal" => StopReason::Refusal,
        _ => StopReason::Other,
    }
}

fn u_get(usage: &Json, key: &str) -> u64 {
    usage.get(key).and_then(Json::as_u64).unwrap_or(0)
}

fn text_field<'a>(delta: Option<&'a Json>, key: &str) -> Option<&'a str> {
    delta.and_then(|d| d.get(key)).and_then(Json::as_str)
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

/// Map one of our [`Message`]s to the Anthropic wire shape.
fn message_to_wire(msg: &Message) -> Json {
    let role = match msg.role {
        Role::User => "user",
        Role::Assistant => "assistant",
    };
    let content: Vec<Json> = msg.content.iter().map(block_to_wire).collect();
    json!({ "role": role, "content": content })
}

fn block_to_wire(block: &ContentBlock) -> Json {
    match block {
        ContentBlock::Text { text } => json!({ "type": "text", "text": text }),
        ContentBlock::Thinking { text, signature } => {
            json!({ "type": "thinking", "thinking": text, "signature": signature })
        }
        ContentBlock::RedactedThinking { data } => {
            json!({ "type": "redacted_thinking", "data": data })
        }
        ContentBlock::ToolUse { id, name, input } => {
            json!({ "type": "tool_use", "id": id, "name": name, "input": input })
        }
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => json!({
            "type": "tool_result",
            "tool_use_id": tool_use_id,
            "content": content,
            "is_error": is_error,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ToolDef;

    fn req() -> TurnRequest {
        TurnRequest {
            model: "claude-opus-4-8".into(),
            max_tokens: 1024,
            show_thinking: true,
            system: "You are a SQL analyst.".into(),
            tools: vec![ToolDef {
                name: "list_schema".into(),
                description: "List tables.".into(),
                input_schema: json!({ "type": "object", "properties": {} }),
            }],
            messages: vec![Message::user_text("hi")],
        }
    }

    #[test]
    fn body_caches_system_and_tools_and_sets_thinking() {
        let p = AnthropicProvider::new("k");
        let body = p.build_body(&req());
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["tools"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["stream"], true);
    }

    #[test]
    fn accumulates_text_and_tool_use_from_sse() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut acc = TurnAccumulator::default();
        let events = [
            json!({"type":"message_start","message":{"usage":{"input_tokens":10,"cache_read_input_tokens":4}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"t1","name":"list_schema"}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"x\":1}"}}),
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":7}}),
        ];
        for e in events {
            acc.handle_event(&e, &tx);
        }
        let outcome = acc.finish().unwrap();
        assert_eq!(outcome.stop_reason, StopReason::ToolUse);
        assert_eq!(outcome.usage.input_tokens, 10);
        assert_eq!(outcome.usage.output_tokens, 7);
        assert_eq!(outcome.usage.cache_read_input_tokens, 4);
        assert_eq!(outcome.message.content.len(), 2);
        match &outcome.message.content[1] {
            ContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "t1");
                assert_eq!(name, "list_schema");
                assert_eq!(input["x"], 1);
            }
            other => panic!("expected tool_use, got {other:?}"),
        }
        // The first delta we streamed should be the tool-use start or text.
        let mut saw_text = false;
        while let Ok(d) = rx.try_recv() {
            if let Delta::Text(t) = d
                && t == "Hello"
            {
                saw_text = true;
            }
        }
        assert!(saw_text);
    }

    #[test]
    fn refusal_becomes_error() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut acc = TurnAccumulator::default();
        acc.handle_event(
            &json!({"type":"message_delta","delta":{"stop_reason":"refusal"}}),
            &tx,
        );
        assert!(matches!(acc.finish(), Err(AiError::Refused)));
    }
}
