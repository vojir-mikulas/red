//! Phase 0 spike — de-risk the ACP 0.14 surface against a real agent.
//!
//! Spawns Claude Code in ACP mode as a subprocess, advertises **restricted**
//! client capabilities (no filesystem, no terminal — so the agent is corralled to
//! whatever MCP tools we provide and can't roam the disk or run shell commands),
//! runs the subscription auth flow if the agent asks for it, opens a session, and
//! streams one prompt — mapping the streamed `session/update` notifications onto
//! the delta vocabulary Red's panel already speaks.
//!
//! This is the one path that needs a human + a real Claude subscription: the
//! `authenticate` call makes the agent pop its own browser `/login`. Everything
//! the spike settles (the exact 0.14 API, the capability shape, how updates
//! encode text/thinking/tool-calls) is what the real `red-acp` provider is built
//! on.
//!
//! Run it:
//! ```bash
//! cargo run -p red-acp --example spike -- "What is 2 + 2?"
//! # override the agent command (defaults to the npx Claude Code ACP package):
//! cargo run -p red-acp --example spike -- --command "claude --acp" "List my tables"
//! ```

use std::str::FromStr;

use agent_client_protocol::schema::{
    ClientCapabilities, ContentBlock, FileSystemCapabilities, Implementation, InitializeRequest,
    NewSessionRequest, PromptRequest, ProtocolVersion, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SelectedPermissionOutcome,
    SessionNotification, SessionUpdate, TextContent,
};
use agent_client_protocol::{AcpAgent, Agent, ConnectionTo};

/// The default agent: Claude Code running in ACP mode, fetched on demand via npx.
/// The agent owns the subscription `/login` and billing — Red never sees the tokens.
const DEFAULT_AGENT_COMMAND: &str = "npx -y @agentclientprotocol/claude-agent-acp";

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber_init();

    let (command, prompt) = parse_args();
    eprintln!("🚀 spawning agent: {command}");
    let agent = AcpAgent::from_str(&command)?;

    agent_client_protocol::Client
        .builder()
        .name("red")
        // Stream the agent's incremental updates and map them onto Red's delta
        // vocabulary (text / thinking / tool activity / usage).
        .on_receive_notification(
            async move |notification: SessionNotification, _cx| {
                print_update(&notification.update);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        // The agent asks the client to approve tool calls. The spike auto-approves
        // (YOLO) just to observe the shape; the real provider will auto-allow
        // read-only DB tools and prompt for anything else (M-S2).
        .on_receive_request(
            async move |request: RequestPermissionRequest, responder, _cx| {
                let title = request
                    .tool_call
                    .fields
                    .title
                    .as_deref()
                    .unwrap_or("(tool)");
                eprintln!("🔐 permission request: {title}");
                match request.options.first().map(|o| o.option_id.clone()) {
                    Some(id) => responder.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(id)),
                    )),
                    None => responder.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Cancelled,
                    )),
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(agent, |conn: ConnectionTo<Agent>| async move {
            // 1. Initialize — advertise RESTRICTED capabilities: no fs, no terminal.
            eprintln!("🤝 initialize…");
            let init = conn
                .send_request(
                    InitializeRequest::new(ProtocolVersion::V1)
                        .client_capabilities(restricted_capabilities())
                        .client_info(Implementation::new("red", env!("CARGO_PKG_VERSION"))),
                )
                .block_task()
                .await?;
            eprintln!("✓ agent: {:?}", init.agent_info);
            eprintln!("  capabilities: {:?}", init.agent_capabilities);

            // 2. Auth — if the agent advertises auth methods, run the first one.
            //    For Claude Code's subscription method this triggers its own
            //    browser `/login`; Red never handles the resulting tokens.
            if let Some(method) = init.auth_methods.first() {
                eprintln!(
                    "🔑 authenticating via '{}' ({}) — a browser login may open…",
                    method.name(),
                    method.id().0
                );
                conn.send_request(agent_client_protocol::schema::AuthenticateRequest::new(
                    method.id().clone(),
                ))
                .block_task()
                .await?;
                eprintln!("✓ authenticated");
            } else {
                eprintln!("🔑 no auth methods advertised — agent is already logged in");
            }

            // 3. New session. `mcp_servers` is where Red's read-only DB MCP server
            //    will be attached in M-S1; the spike leaves it empty.
            eprintln!("📝 session/new…");
            let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"));
            let session = conn
                .send_request(NewSessionRequest::new(cwd))
                .block_task()
                .await?;
            let session_id = session.session_id;
            eprintln!("✓ session created");

            // 4. Prompt — updates stream through the notification handler above.
            eprintln!("💬 prompt: {prompt:?}\n");
            let resp = conn
                .send_request(PromptRequest::new(
                    session_id,
                    vec![ContentBlock::Text(TextContent::new(prompt.clone()))],
                ))
                .block_task()
                .await?;
            eprintln!("\n✅ done — stop reason: {:?}", resp.stop_reason);
            Ok(())
        })
        .await?;

    Ok(())
}

/// No filesystem, no terminal — the agent is restricted to the MCP tools we hand
/// it. This is the capability lockdown the plan calls for (M-S2 hardens it).
fn restricted_capabilities() -> ClientCapabilities {
    ClientCapabilities::default()
        .fs(FileSystemCapabilities::new()
            .read_text_file(false)
            .write_text_file(false))
        .terminal(false)
}

/// Map one streamed `session/update` onto the same delta categories Red's panel
/// renders (text / thinking / tool activity). This mapping is the load-bearing
/// part the real provider reuses.
fn print_update(update: &SessionUpdate) {
    match update {
        SessionUpdate::AgentMessageChunk(chunk) => print!("{}", text_of(&chunk.content)),
        SessionUpdate::AgentThoughtChunk(chunk) => {
            eprint!("\x1b[2m[thinking] {}\x1b[0m", text_of(&chunk.content));
        }
        SessionUpdate::ToolCall(call) => {
            eprintln!("\n🔧 tool call: {} [{:?}]", call.title, call.status);
        }
        SessionUpdate::ToolCallUpdate(update) => {
            eprintln!("   tool update: {:?}", update.fields.status);
        }
        SessionUpdate::UsageUpdate(usage) => eprintln!("   usage: {usage:?}"),
        other => eprintln!("   (update: {other:?})"),
    }
    use std::io::Write;
    let _ = std::io::stdout().flush();
}

/// Extract the plain text of a content block (the only kind we stream as prose).
fn text_of(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text(t) => t.text.clone(),
        other => format!("[{other:?}]"),
    }
}

/// `[--command "<cmd>"] <prompt…>` — no clap, to keep the crate dep-free.
fn parse_args() -> (String, String) {
    let mut args = std::env::args().skip(1).peekable();
    let mut command = DEFAULT_AGENT_COMMAND.to_string();
    let mut prompt_parts = Vec::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--command" | "-c" => {
                if let Some(c) = args.next() {
                    command = c;
                }
            }
            _ => prompt_parts.push(arg),
        }
    }
    let prompt = if prompt_parts.is_empty() {
        "Say hello in one short sentence.".to_string()
    } else {
        prompt_parts.join(" ")
    };
    (command, prompt)
}

fn tracing_subscriber_init() {
    // Best-effort; the spike runs fine without it.
}
