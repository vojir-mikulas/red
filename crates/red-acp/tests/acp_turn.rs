//! Drives the real ACP client (`AcpConversation`) against the canned fake agent
//! (`src/bin/red-acp-fake-agent.rs`) over stdio — no subscription, no network.
//! Asserts the handshake + session come up, a turn streams text and finishes, and
//! cancel stops an in-flight turn.

use red_acp::{AcpConfig, AcpConversation, AcpDelta, AcpStop};
use tokio::sync::mpsc;

/// Point the conversation at the fake-agent binary Cargo built for this test.
fn fake_agent_config() -> AcpConfig {
    AcpConfig {
        command: env!("CARGO_BIN_EXE_red-acp-fake-agent").to_string(),
        cwd: std::env::temp_dir(),
        mcp: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn handshake_then_streamed_turn() {
    let conv = AcpConversation::start(fake_agent_config())
        .await
        .expect("agent comes up");

    let (sink, mut deltas) = mpsc::unbounded_channel();
    let done = conv.prompt("hello there".to_string(), sink);

    let mut text = String::new();
    let mut saw_thinking = false;
    while let Some(delta) = deltas.recv().await {
        match delta {
            AcpDelta::Text(t) => text.push_str(&t),
            AcpDelta::Thinking(_) => saw_thinking = true,
            _ => {}
        }
    }

    let result = done.await.expect("reply").expect("turn ok");
    assert_eq!(text, "Hello world");
    assert!(saw_thinking, "expected a streamed thought");
    assert_eq!(result.stop, AcpStop::EndTurn);
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_stops_an_inflight_turn() {
    let conv = AcpConversation::start(fake_agent_config())
        .await
        .expect("agent comes up");

    let (sink, mut deltas) = mpsc::unbounded_channel();
    // "HANG" makes the fake agent wait for session/cancel before finishing.
    let done = conv.prompt("please HANG".to_string(), sink);

    // Once any streamed delta lands, the turn is in flight — cancel it.
    let _ = deltas.recv().await;
    conv.cancel();

    while deltas.recv().await.is_some() {}
    let result = done.await.expect("reply").expect("turn ok");
    assert_eq!(result.stop, AcpStop::Cancelled);
}
