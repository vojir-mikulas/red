//! Drives the real ACP client (`AcpConversation`) against the canned fake agent
//! (`src/bin/red-acp-fake-agent.rs`) over stdio: no subscription, no network.
//! Asserts the handshake + session come up, a turn streams text and finishes, and
//! cancel stops an in-flight turn.

use red_acp::{AcpConfig, AcpConversation, AcpDelta, AcpPermission, AcpStop};
use tokio::sync::mpsc;

/// Point the conversation at the fake-agent binary Cargo built for this test.
fn fake_agent_config() -> AcpConfig {
    AcpConfig {
        command: env!("CARGO_BIN_EXE_red-acp-fake-agent").to_string(),
        cwd: std::env::temp_dir(),
        mcp: None,
        // The fake agent titles its permission tool `run_select`; auto-allow it.
        allow_tools: vec!["run_select".to_string()],
        permissions: None,
        commands: None,
        config: None,
    }
}

/// Drain the streamed answer text of a turn to completion.
async fn collect_text(deltas: &mut mpsc::UnboundedReceiver<AcpDelta>) -> String {
    let mut text = String::new();
    while let Some(delta) = deltas.recv().await {
        if let AcpDelta::Text(t) = delta {
            text.push_str(&t);
        }
    }
    text
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

    // Once any streamed delta lands, the turn is in flight; cancel it.
    let _ = deltas.recv().await;
    conv.cancel();

    while deltas.recv().await.is_some() {}
    let result = done.await.expect("reply").expect("turn ok");
    assert_eq!(result.stop, AcpStop::Cancelled);
}

#[tokio::test(flavor = "multi_thread")]
async fn auto_allows_a_known_readonly_tool() {
    // No permissions sink: an auto-allowed tool must still be granted silently.
    let conv = AcpConversation::start(fake_agent_config())
        .await
        .expect("agent comes up");

    let (sink, mut deltas) = mpsc::unbounded_channel();
    let done = conv.prompt("PERMIT please".to_string(), sink);

    let text = collect_text(&mut deltas).await;
    done.await.expect("reply").expect("turn ok");
    assert_eq!(text, "GRANTED");
}

#[tokio::test(flavor = "multi_thread")]
async fn prompts_the_user_for_an_unknown_tool() {
    let (perm_tx, mut perm_rx) = mpsc::unbounded_channel::<AcpPermission>();
    let config = AcpConfig {
        permissions: Some(perm_tx),
        ..fake_agent_config()
    };
    let conv = AcpConversation::start(config)
        .await
        .expect("agent comes up");

    // `UNKNOWN` makes the fake agent request an un-allowlisted tool → user decides.
    let (sink, mut deltas) = mpsc::unbounded_channel();
    let done = conv.prompt("PERMIT UNKNOWN".to_string(), sink);

    let perm = perm_rx.recv().await.expect("a permission to surface");
    assert_eq!(perm.title, "transmogrify");
    perm.decide.send(true).expect("decision delivered");

    let text = collect_text(&mut deltas).await;
    done.await.expect("reply").expect("turn ok");
    assert_eq!(text, "GRANTED");
}

#[tokio::test(flavor = "multi_thread")]
async fn detects_a_crashed_agent() {
    let conv = AcpConversation::start(fake_agent_config())
        .await
        .expect("agent comes up");
    assert!(conv.is_alive(), "a freshly started agent is alive");

    // "EXIT" makes the fake agent kill its process mid-turn (a crash). The turn
    // fails and the handle must report itself dead so the service restarts it.
    let (sink, mut deltas) = mpsc::unbounded_channel();
    let done = conv.prompt("please EXIT".to_string(), sink);
    while deltas.recv().await.is_some() {}
    let outcome = done.await;
    assert!(
        !matches!(outcome, Ok(Ok(_))),
        "a crashed turn must not report success: {outcome:?}"
    );

    // The child monitor reports the dead process; the connection task winds down.
    let dead = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while conv.is_alive() {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(dead.is_ok(), "a crashed agent is reported dead");
}

#[tokio::test(flavor = "multi_thread")]
async fn denies_an_unknown_tool_when_no_ui_is_wired() {
    // permissions: None → anything not auto-allowed is denied by default.
    let conv = AcpConversation::start(fake_agent_config())
        .await
        .expect("agent comes up");

    let (sink, mut deltas) = mpsc::unbounded_channel();
    let done = conv.prompt("PERMIT UNKNOWN".to_string(), sink);

    let text = collect_text(&mut deltas).await;
    done.await.expect("reply").expect("turn ok");
    assert_eq!(text, "DENIED");
}
