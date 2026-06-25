//! Startup environment fix-ups. macOS apps launched from Finder/Dock (rather than
//! a terminal) inherit a minimal `PATH` — roughly `/usr/bin:/bin:/usr/sbin:/sbin`
//! — that omits Homebrew (`/opt/homebrew/bin`) and version-manager shims. The ACP
//! agent is spawned as `npx …` (and the auth CLI likewise), so without this the
//! subprocess fails to start with `No such file or directory (os error 2)`.
//!
//! We patch the process `PATH` once, before any thread or subprocess is spawned,
//! so every child (the agent, the auth CLI, SSH tunnels) inherits a `PATH` that
//! can locate Node. Same problem Zed/VS Code solve at launch.

/// Ensure `node`/`npx` are reachable on `PATH` for GUI-launched macOS builds.
/// No-op off macOS (Windows/Linux GUI launches inherit a usable `PATH`).
///
/// Must run before the backend thread spawns — mutating the process environment
/// is only sound while the program is still single-threaded.
#[cfg(target_os = "macos")]
pub fn augment_path_for_gui_launch() {
    use std::path::Path;

    let mut entries: Vec<String> = std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();

    // The usual homes for a macOS Node install: Apple-silicon Homebrew, the
    // official Node .pkg / Intel Homebrew, and MacPorts. Appending (not
    // prepending) avoids shadowing system binaries.
    for dir in [
        "/opt/homebrew/bin",
        "/opt/homebrew/sbin",
        "/usr/local/bin",
        "/opt/local/bin",
    ] {
        if !entries.iter().any(|e| e == dir) && Path::new(dir).is_dir() {
            entries.push(dir.to_string());
        }
    }

    // Version managers (nvm/fnm/asdf/volta) install Node under the user's home
    // and only put it on `PATH` from a login/interactive shell. If the well-known
    // dirs didn't turn up a `node`, ask the login shell what its `PATH` is and
    // merge in anything new. Only paid by users the static dirs didn't already fix.
    if !resolves(&entries, "node") {
        if let Some(shell_path) = login_shell_path() {
            for dir in shell_path.split(':').filter(|s| !s.is_empty()) {
                if !entries.iter().any(|e| e == dir) {
                    entries.push(dir.to_string());
                }
            }
        }
    }

    let path = entries.join(":");
    // SAFETY: called from `main` before any other thread (the GPUI platform, the
    // backend runtime) is spawned, so no other thread can be reading the
    // environment concurrently.
    unsafe { std::env::set_var("PATH", &path) };
}

/// Whether `program` resolves to an executable file in one of `dirs`.
#[cfg(target_os = "macos")]
fn resolves(dirs: &[String], program: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;

    dirs.iter().any(|dir| {
        let candidate = std::path::Path::new(dir).join(program);
        std::fs::metadata(&candidate)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    })
}

/// Capture the `PATH` a user's interactive login shell sees (where nvm/fnm/asdf
/// init lives). Best-effort: any failure returns `None` and we keep the PATH we
/// have. Only stdout is trusted — the shell may chatter on stderr.
#[cfg(target_os = "macos")]
fn login_shell_path() -> Option<String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
    let output = std::process::Command::new(shell)
        // `-l -i` source the profile *and* rc files so version-manager hooks run;
        // `printf` (no trailing newline) keeps the payload clean.
        .args(["-l", "-i", "-c", "printf '%s' \"$PATH\""])
        // Detach stdin so a chatty rc can never block reading input, and discard
        // stderr so shell startup noise doesn't leak into our log.
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!path.is_empty()).then_some(path)
}

#[cfg(not(target_os = "macos"))]
pub fn augment_path_for_gui_launch() {}
