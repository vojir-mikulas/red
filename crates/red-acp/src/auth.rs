//! Subscription **sign-in** and "who is logged in" for the Claude Agent ACP agent.
//!
//! The agent (`@agentclientprotocol/claude-agent-acp`) never advertises an auth
//! method to Red — it only offers terminal-login methods when the *client* asks
//! for the `auth.terminal` capability, and even then its `authenticate` RPC isn't
//! implemented for them. So the old "spawn a probe handshake to pop `/login`"
//! approach was a silent no-op once signed in. Instead the agent ships a CLI: run
//! it with `--cli auth <sub>` and it shells out to the bundled `claude` binary:
//!
//! - `auth status` → JSON ([`AuthStatus`]): login state, email, subscription.
//! - `auth login --claudeai` → a **paste-code** OAuth flow: it opens the browser
//!   to an authorize URL, then waits on **stdin** for the code the browser shows.
//!   There is no localhost auto-callback variant, so Red drives it: read the URL
//!   off stdout ([`run_login`] emits [`LoginEvent::Url`]), let the user authorize,
//!   then feed the code back over stdin (the `code` receiver).
//! - `auth logout` → clears the stored credential.
//!
//! Red never sees the OAuth tokens — the bundled CLI owns them.

use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

use crate::types::AcpError;

/// How long to wait for the bundled `auth status`/`logout` command to finish. The
/// first `npx` run may fetch the package; later runs are cached and quick.
const STATUS_TIMEOUT: Duration = Duration::from_secs(45);

/// How long to wait for `auth login` to print its authorize URL before giving up.
const LOGIN_URL_TIMEOUT: Duration = Duration::from_secs(60);

/// How long to wait, after the code is submitted, for the CLI to exchange it and
/// exit. A wrong/expired code makes the CLI fail fast; this only bounds a hang.
const LOGIN_FINISH_TIMEOUT: Duration = Duration::from_secs(120);

/// Cap on captured stderr used to explain a failed sign-in, so a chatty agent
/// can't grow the buffer without bound.
const STDERR_CAP: usize = 4096;

/// The agent's `auth status --json` payload — who (if anyone) is signed in. Unknown
/// fields are ignored; every field defaults so a `{"loggedIn": false}` still parses.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct AuthStatus {
    #[serde(rename = "loggedIn", default)]
    pub logged_in: bool,
    #[serde(default)]
    pub email: Option<String>,
    /// e.g. `"max"`, `"pro"` — the Claude subscription tier (claude.ai auth only).
    #[serde(rename = "subscriptionType", default)]
    pub subscription_type: Option<String>,
    /// e.g. `"claude.ai"` or `"console"`.
    #[serde(rename = "authMethod", default)]
    pub auth_method: Option<String>,
    #[serde(rename = "orgName", default)]
    pub org_name: Option<String>,
}

/// One step of the interactive [`run_login`] flow, relayed to the UI.
#[derive(Debug)]
pub enum LoginEvent {
    /// The authorize URL the CLI opened (and printed). Red surfaces it so the user
    /// can open it manually if the auto-open didn't land, then authorizes there.
    Url(String),
    /// The flow finished: `Ok` on a stored credential, `Err(message)` otherwise
    /// (cancelled, wrong code, spawn failure, timeout).
    Done(Result<(), String>),
}

/// Query the agent's bundled CLI for the current sign-in. Runs `<command> --cli
/// auth status` and parses its JSON. Errors if the command can't be parsed/spawned,
/// times out, or doesn't speak this CLI (e.g. a non-Claude agent).
pub async fn auth_status(command: &str) -> Result<AuthStatus, AcpError> {
    let (program, mut args) = split_command(command)?;
    args.extend(["--cli", "auth", "status"].into_iter().map(String::from));
    let output = timeout(
        STATUS_TIMEOUT,
        Command::new(&program)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| AcpError::Protocol("auth status timed out".into()))?
    .map_err(|e| AcpError::Spawn(format!("could not run {program}: {e}")))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json = extract_json_object(&stdout).ok_or_else(|| {
        let stderr = String::from_utf8_lossy(&output.stderr);
        AcpError::Protocol(format!(
            "could not read sign-in status (exit {:?}): {}",
            output.status.code(),
            stderr.trim()
        ))
    })?;
    serde_json::from_str(json)
        .map_err(|e| AcpError::Protocol(format!("could not parse sign-in status: {e}")))
}

/// Sign out of the agent's subscription: `<command> --cli auth logout`. Best-effort;
/// surfaces a non-zero exit as an error.
pub async fn logout(command: &str) -> Result<(), AcpError> {
    let (program, mut args) = split_command(command)?;
    args.extend(["--cli", "auth", "logout"].into_iter().map(String::from));
    let output = timeout(
        STATUS_TIMEOUT,
        Command::new(&program)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| AcpError::Protocol("logout timed out".into()))?
    .map_err(|e| AcpError::Spawn(format!("could not run {program}: {e}")))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(AcpError::Protocol(format!(
            "logout failed: {}",
            stderr.trim()
        )))
    }
}

/// Drive the paste-code sign-in to completion. Spawns `<command> --cli auth login
/// --claudeai`, emits the authorize URL via `events`, then waits for the code on
/// `code` (the UI sends it once the user has authorized in the browser), feeds it to
/// the CLI's stdin, and reports the outcome via [`LoginEvent::Done`]. Dropping the
/// `code` sender before a code arrives cancels the flow (the child is killed).
pub async fn run_login(
    command: String,
    events: mpsc::UnboundedSender<LoginEvent>,
    code: oneshot::Receiver<String>,
) {
    let result = run_login_inner(&command, &events, code).await;
    let _ = events.send(LoginEvent::Done(result));
}

async fn run_login_inner(
    command: &str,
    events: &mpsc::UnboundedSender<LoginEvent>,
    code: oneshot::Receiver<String>,
) -> Result<(), String> {
    let (program, mut args) = split_command(command).map_err(|e| e.to_string())?;
    args.extend(
        ["--cli", "auth", "login", "--claudeai"]
            .into_iter()
            .map(String::from),
    );
    let mut child = Command::new(&program)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("could not start sign-in ({program}): {e}"))?;

    let stdout = child.stdout.take().expect("piped stdout");
    let mut stdin = child.stdin.take().expect("piped stdin");
    let stderr = child.stderr.take().expect("piped stderr");

    // Drain stderr into a capped buffer so a failed exchange can be explained.
    let errbuf = Arc::new(Mutex::new(String::new()));
    {
        let errbuf = errbuf.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let mut buf = errbuf.lock().unwrap_or_else(|p| p.into_inner());
                if buf.len() < STDERR_CAP {
                    buf.push_str(&line);
                    buf.push('\n');
                }
            }
        });
    }

    let mut lines = BufReader::new(stdout).lines();
    // Phase 1: read the authorize URL off stdout (the CLI also opens the browser).
    let url = read_until_url(&mut lines, LOGIN_URL_TIMEOUT).await?;
    let _ = events.send(LoginEvent::Url(url));
    // Keep draining stdout so the pipe never stalls while the user authorizes.
    let drain = tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });

    // Phase 2: wait for the pasted code (or cancellation via a dropped sender).
    let code = code.await.map_err(|_| "sign-in cancelled".to_string())?;
    stdin
        .write_all(format!("{}\n", code.trim()).as_bytes())
        .await
        .map_err(|e| format!("could not submit the code: {e}"))?;
    stdin.flush().await.ok();
    // Close stdin so the CLI stops waiting for more input and proceeds to exchange.
    drop(stdin);

    // Phase 3: wait for the exchange to finish.
    let status = match timeout(LOGIN_FINISH_TIMEOUT, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(e)) => return Err(format!("sign-in process error: {e}")),
        Err(_) => {
            let _ = child.start_kill();
            return Err("sign-in timed out".into());
        }
    };
    let _ = drain.await;

    if status.success() {
        Ok(())
    } else {
        let stderr = errbuf
            .lock()
            .map(|b| b.trim().to_string())
            .unwrap_or_default();
        Err(if stderr.is_empty() {
            "sign-in failed — the code may be wrong or expired".into()
        } else {
            stderr
        })
    }
}

/// Read stdout lines until one carries an `https://` URL, or the stream ends / the
/// deadline passes. Returns the bare URL.
async fn read_until_url<R: tokio::io::AsyncBufRead + Unpin>(
    lines: &mut tokio::io::Lines<R>,
    within: Duration,
) -> Result<String, String> {
    let scan = async {
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    if let Some(url) = extract_url(&line) {
                        return Ok(url);
                    }
                }
                Ok(None) => return Err("sign-in ended before a URL appeared".to_string()),
                Err(e) => return Err(format!("could not read sign-in output: {e}")),
            }
        }
    };
    match timeout(within, scan).await {
        Ok(result) => result,
        Err(_) => Err("timed out waiting for the sign-in URL".to_string()),
    }
}

/// Pull the first `https://…` token out of a line (stops at the first whitespace).
fn extract_url(line: &str) -> Option<String> {
    let start = line.find("https://")?;
    let rest = &line[start..];
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

/// Slice the first complete-looking JSON object (`{ … }`) out of mixed output, so a
/// stray `npx`/warning line before the payload doesn't break parsing.
fn extract_json_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    (end > start).then(|| &text[start..=end])
}

/// Split a configured agent command into `(program, args)` with the same quoting
/// rules the ACP SDK uses. Rejects the JSON server-spec form (it isn't a runnable
/// program) and an empty command.
fn split_command(command: &str) -> Result<(String, Vec<String>), AcpError> {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return Err(AcpError::Spawn("no agent command is configured".into()));
    }
    if trimmed.starts_with('{') {
        return Err(AcpError::Protocol(
            "sign-in is only available for command-style agents".into(),
        ));
    }
    let mut parts = shell_words::split(trimmed)
        .map_err(|e| AcpError::Spawn(format!("could not parse the agent command: {e}")))?;
    if parts.is_empty() {
        return Err(AcpError::Spawn("no agent command is configured".into()));
    }
    let program = parts.remove(0);
    Ok((program, parts))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_url_from_a_noisy_line() {
        let line = "If the browser didn't open, visit: https://claude.com/cai/oauth/authorize?code=true&x=1 now";
        assert_eq!(
            extract_url(line).as_deref(),
            Some("https://claude.com/cai/oauth/authorize?code=true&x=1")
        );
        assert_eq!(extract_url("no url here"), None);
    }

    #[test]
    fn extracts_json_object_amid_noise() {
        let text = "npm warn exec\n{\n  \"loggedIn\": true\n}\n";
        assert_eq!(
            extract_json_object(text),
            Some("{\n  \"loggedIn\": true\n}")
        );
        assert_eq!(extract_json_object("no json"), None);
    }

    #[test]
    fn parses_a_full_status_payload() {
        let json = r#"{
            "loggedIn": true,
            "authMethod": "claude.ai",
            "email": "a@b.com",
            "orgName": "a@b.com's Organization",
            "subscriptionType": "max"
        }"#;
        let status: AuthStatus = serde_json::from_str(json).unwrap();
        assert!(status.logged_in);
        assert_eq!(status.email.as_deref(), Some("a@b.com"));
        assert_eq!(status.subscription_type.as_deref(), Some("max"));
        assert_eq!(status.auth_method.as_deref(), Some("claude.ai"));
    }

    #[test]
    fn parses_a_logged_out_payload() {
        let status: AuthStatus = serde_json::from_str(r#"{"loggedIn": false}"#).unwrap();
        assert!(!status.logged_in);
        assert!(status.email.is_none());
    }

    #[test]
    fn splits_a_command_like_the_sdk() {
        let (prog, args) = split_command("npx -y @agentclientprotocol/claude-agent-acp").unwrap();
        assert_eq!(prog, "npx");
        assert_eq!(args, ["-y", "@agentclientprotocol/claude-agent-acp"]);
    }

    #[test]
    fn rejects_json_and_empty_commands() {
        assert!(split_command("  ").is_err());
        assert!(split_command(r#"{"command": "x"}"#).is_err());
    }
}
