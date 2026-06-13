//! The macOS self-updater (Phase 3 of docs/plans/self-update.md): poll GitHub
//! Releases, download a newer notarized `.dmg`, swap it over the installed
//! `/Applications/Red.app`, and report `ReadyToRestart`. The relaunch itself is a
//! UI concern (spawn the swapped bundle + exit).
//!
//! Network and bundle operations shell out to the platform tools the release
//! pipeline already relies on — `curl`, `hdiutil`, `rsync` — so the backend takes
//! on no new HTTP dependency (just `serde_json` to read one API response). The
//! task runs on the `red-service` Tokio thread but off the dispatch loop, and all
//! blocking work is wrapped in `spawn_blocking`, so the UI never stalls.
//!
//! Integrity rests on notarization, not a checksum: the release pipeline staples
//! the dmg, so Gatekeeper validates it on mount — a tampered or truncated
//! download fails to attach. See the "checksum vs. notarization" decision in the
//! plan.

use std::path::Path;

use red_core::UpdateState;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::protocol::UpdateConfig;
use crate::{Event, SessionId};

/// The events sink back to the UI (the same `futures` mpsc the dispatch loop
/// uses). Update events are session-less, so they carry `None`.
type Events = futures::channel::mpsc::UnboundedSender<(Option<SessionId>, Event)>;

/// Messages the dispatch loop forwards to the updater task.
pub(crate) enum UpdateControl {
    /// (Re)configure cadence + identity — sent at launch and on each settings
    /// reload. Toggling `enabled` or changing the interval re-arms the poll (and
    /// triggers an immediate check); a no-op config is ignored.
    Configure(UpdateConfig),
    /// Force an immediate check ("Check for updates" in the About tab).
    CheckNow,
}

fn emit(events: &Events, state: UpdateState) {
    let _ = events.unbounded_send((None, Event::UpdateState(state)));
}

/// The updater task. Owns its poll timer and the running config; spawned once by
/// the dispatch loop with a clone of the event sender. Exits when the control
/// channel closes (the service is shutting down).
pub(crate) async fn run(events: Events, mut control: UnboundedReceiver<UpdateControl>) {
    let mut config: Option<UpdateConfig> = None;
    // Disabled until a `Configure` with `enabled` arrives; rebuilt only when the
    // cadence actually changes, so toggling unrelated settings doesn't re-poll.
    let mut ticker: Option<tokio::time::Interval> = None;

    loop {
        let tick = async {
            match ticker.as_mut() {
                Some(t) => {
                    t.tick().await;
                }
                None => std::future::pending().await,
            }
        };

        tokio::select! {
            msg = control.recv() => match msg {
                None => break, // dispatch dropped the sender — service shutting down
                Some(UpdateControl::Configure(cfg)) => {
                    let cadence_changed = config
                        .as_ref()
                        .map(|c| c.enabled != cfg.enabled || c.interval != cfg.interval)
                        .unwrap_or(true);
                    if cadence_changed {
                        ticker = cfg.enabled.then(|| {
                            let mut t = tokio::time::interval(cfg.interval);
                            // `interval`'s first tick is immediate → an initial check
                            // at launch / when re-enabled.
                            t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                            t
                        });
                    }
                    config = Some(cfg);
                }
                Some(UpdateControl::CheckNow) => {
                    if let Some(cfg) = &config {
                        check(&events, cfg).await;
                    }
                }
            },
            _ = tick => {
                if let Some(cfg) = &config {
                    if cfg.enabled {
                        check(&events, cfg).await;
                    }
                }
            }
        }
    }
}

/// One full check → (maybe) download → swap cycle, emitting `UpdateState` at each
/// transition. Blocking steps run on the blocking pool so the task — and the
/// dispatch loop sharing this thread — stay responsive.
async fn check(events: &Events, cfg: &UpdateConfig) {
    emit(events, UpdateState::Checking);

    let repo = cfg.repo.clone();
    let release = match tokio::task::spawn_blocking(move || fetch_latest_release(&repo)).await {
        Ok(Ok(release)) => release,
        Ok(Err(reason)) => return emit(events, UpdateState::Failed { reason }),
        Err(_) => {
            return emit(
                events,
                UpdateState::Failed {
                    reason: "update check task failed".into(),
                },
            )
        }
    };

    if !is_newer(&release.version, &cfg.current_version) {
        return emit(
            events,
            UpdateState::UpToDate {
                current: cfg.current_version.clone(),
            },
        );
    }

    // A newer release exists. Can we self-swap in place?
    let app_root = match installed_app_root() {
        Some(path) => path,
        None => {
            return emit(
                events,
                UpdateState::Unsupported {
                    version: release.version,
                    url: release.html_url,
                },
            )
        }
    };
    let dmg_url = match release.dmg_url {
        Some(url) => url,
        None => {
            return emit(
                events,
                UpdateState::Unsupported {
                    version: release.version,
                    url: release.html_url,
                },
            )
        }
    };

    emit(
        events,
        UpdateState::Downloading {
            version: release.version.clone(),
            pct: 0,
        },
    );

    let version = release.version;
    let app = app_root.to_string_lossy().into_owned();
    match tokio::task::spawn_blocking(move || download_and_swap(&dmg_url, Path::new(&app))).await {
        Ok(Ok(())) => emit(events, UpdateState::ReadyToRestart { version }),
        Ok(Err(reason)) => emit(events, UpdateState::Failed { reason }),
        Err(_) => emit(
            events,
            UpdateState::Failed {
                reason: "update staging task failed".into(),
            },
        ),
    }
}

/// The release fields the updater needs from the GitHub API.
struct Release {
    /// The tag, e.g. `v0.2.0`.
    version: String,
    /// The release's GitHub page, for the manual-download fallback.
    html_url: String,
    /// The universal `.dmg` asset's download URL, if present.
    dmg_url: Option<String>,
}

/// GET the repo's latest *non-prerelease* release and pull out the dmg asset.
/// `/releases/latest` already excludes drafts and prereleases server-side.
fn fetch_latest_release(repo: &str) -> Result<Release, String> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let out = std::process::Command::new("curl")
        .args([
            "-sSL",
            "--fail",
            "-H",
            "Accept: application/vnd.github+json",
            "-H",
            "User-Agent: red-updater",
            &url,
        ])
        .output()
        .map_err(|e| format!("launching curl: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "GitHub API request failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }

    let json: serde_json::Value =
        serde_json::from_slice(&out.stdout).map_err(|e| format!("parsing GitHub response: {e}"))?;
    let version = json["tag_name"]
        .as_str()
        .ok_or_else(|| "GitHub release has no tag_name".to_string())?
        .to_string();
    let html_url = json["html_url"].as_str().unwrap_or_default().to_string();
    let dmg_url = json["assets"].as_array().and_then(|assets| {
        assets.iter().find_map(|asset| {
            let name = asset["name"].as_str()?;
            name.ends_with(".dmg")
                .then(|| asset["browser_download_url"].as_str())
                .flatten()
                .map(str::to_string)
        })
    });

    Ok(Release {
        version,
        html_url,
        dmg_url,
    })
}

/// `true` when `latest` is a strictly higher semver than `current`. A tag that
/// doesn't parse (or a prerelease suffix on either side) is treated as not-newer
/// — we never downgrade or sidegrade on a tag we can't compare.
fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_semver(latest), parse_semver(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

/// Parse a `vMAJOR.MINOR.PATCH` tag into a comparable tuple, ignoring any
/// prerelease/build suffix. Missing patch defaults to 0.
fn parse_semver(tag: &str) -> Option<(u64, u64, u64)> {
    let core = tag
        .trim()
        .trim_start_matches('v')
        .split(['-', '+'])
        .next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

/// The installed `.app` bundle root *if* we're allowed to swap it: the running
/// executable must live in `…/Red.app/Contents/MacOS/Red` under a writable
/// `/Applications`. Anything else (a `cargo run` dev build, a Homebrew/read-only
/// install) returns `None`, which the caller surfaces as `Unsupported` with a
/// manual-download link — matching the plan's "don't fight the package manager".
#[cfg(target_os = "macos")]
fn installed_app_root() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let app = exe.parent()?.parent()?.parent()?; // …/Red.app
    let is_bundle = app.extension().and_then(|e| e.to_str()) == Some("app");
    let writable = app.starts_with("/Applications")
        && app
            .parent()
            .and_then(|p| std::fs::metadata(p).ok())
            .map(|m| !m.permissions().readonly())
            .unwrap_or(false);
    (is_bundle && writable).then(|| app.to_path_buf())
}

#[cfg(not(target_os = "macos"))]
fn installed_app_root() -> Option<std::path::PathBuf> {
    None
}

/// Download the dmg, mount it (Gatekeeper validates the staple here), and replace
/// the installed bundle from the mounted copy. The mount is always detached and
/// the temp dir cleaned, even on a failed swap.
#[cfg(target_os = "macos")]
fn download_and_swap(dmg_url: &str, app_root: &Path) -> Result<(), String> {
    let tmp = std::env::temp_dir().join(format!("red-update-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).map_err(|e| format!("creating temp dir: {e}"))?;
    let dmg = tmp.join("Red.dmg");
    let mount = tmp.join("mnt");

    let result = (|| -> Result<(), String> {
        let dmg_path = dmg.to_str().ok_or("non-UTF-8 temp path")?;
        run_cmd("curl", &["-sSL", "--fail", "-o", dmg_path, dmg_url])?;

        std::fs::create_dir_all(&mount).map_err(|e| format!("creating mount dir: {e}"))?;
        let mount_path = mount.to_str().ok_or("non-UTF-8 mount path")?;
        run_cmd(
            "hdiutil",
            &[
                "attach",
                "-nobrowse",
                "-readonly",
                "-mountpoint",
                mount_path,
                dmg_path,
            ],
        )?;

        // rsync over the bundle in place. Trailing slashes copy *contents*;
        // `--delete` removes files the new build dropped, so no stale binary
        // lingers. Detach happens below regardless of this result.
        let src = format!(
            "{}/",
            mount.join("Red.app").to_str().ok_or("bad mount path")?
        );
        let dst = format!("{}/", app_root.to_str().ok_or("bad app path")?);
        let swap = run_cmd("rsync", &["-a", "--delete", &src, &dst]);

        let _ = std::process::Command::new("hdiutil")
            .args(["detach", mount_path])
            .output();
        swap
    })();

    let _ = std::fs::remove_dir_all(&tmp);
    result
}

#[cfg(not(target_os = "macos"))]
fn download_and_swap(_dmg_url: &str, _app_root: &Path) -> Result<(), String> {
    Err("self-update is only supported on macOS".into())
}

/// Run a helper process, mapping a non-zero exit (or a launch failure) to a
/// human-readable error carrying its stderr.
#[cfg(target_os = "macos")]
fn run_cmd(program: &str, args: &[&str]) -> Result<(), String> {
    let out = std::process::Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("launching {program}: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{program} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_parsing_tolerates_prefix_and_suffix() {
        assert_eq!(parse_semver("v0.1.2"), Some((0, 1, 2)));
        assert_eq!(parse_semver("0.1.2"), Some((0, 1, 2)));
        assert_eq!(parse_semver("v1.2"), Some((1, 2, 0)));
        assert_eq!(parse_semver("v0.2.0-rc1"), Some((0, 2, 0)));
        assert_eq!(parse_semver("nightly"), None);
    }

    #[test]
    fn newer_is_strict_and_safe_on_garbage() {
        assert!(is_newer("v0.2.0", "v0.1.4"));
        assert!(is_newer("v0.1.5", "v0.1.4"));
        assert!(is_newer("v1.0.0", "v0.9.9"));
        // Same or older never counts as newer.
        assert!(!is_newer("v0.1.4", "v0.1.4"));
        assert!(!is_newer("v0.1.3", "v0.1.4"));
        // Unparseable tags never trigger an update (no downgrade/sidegrade).
        assert!(!is_newer("garbage", "v0.1.4"));
        assert!(!is_newer("v0.2.0", "garbage"));
    }
}
