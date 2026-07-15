//! The self-updater (Phase 3 of docs/plans/self-update.md): poll GitHub Releases,
//! download a newer build, swap it over the installed app, and report
//! `ReadyToRestart`. The relaunch itself is a UI concern (spawn the new build +
//! exit). The check loop, API fetch, semver comparison, and download-host pinning
//! are platform-agnostic; only asset selection, the install-root probe, the swap,
//! and the relaunch differ per OS.
//!
//! **macOS** swaps a notarized `.dmg` over `/Applications/Red.app`. **Linux** is
//! AppImage-only: it replaces the running `$APPIMAGE` in place (see the Linux
//! `download_and_swap`). Other platforms (Windows) report `Unsupported` and fall
//! back to a manual download. Integrity on macOS rests on notarization; on Linux,
//! which has no equivalent, on a `.sha256` sidecar published with the release.
//!
//! Network and bundle operations shell out to the platform tools the release
//! pipeline already relies on (`curl`, `hdiutil`, `rsync`), so the backend takes
//! on no new HTTP dependency (just `serde_json` to read one API response). The
//! task runs on the `red-service` Tokio thread but off the dispatch loop, and all
//! blocking work is wrapped in `spawn_blocking`, so the UI never stalls.
//!
//! Integrity rests on notarization, not a checksum: the release pipeline staples
//! the dmg, so Gatekeeper validates it on mount: a tampered or truncated
//! download fails to attach. See the "checksum vs. notarization" decision in the
//! plan.
//!
//! Authenticity is enforced on top of that integrity story: before the swap the
//! mounted bundle is run through `codesign --verify --deep --strict` and
//! `spctl --assess --type execute`, and its Team ID is required to match the
//! *running* (already-Gatekeeper-validated) bundle's, so a compromised release
//! serving some *other* notarized app can't be installed over Red. The download
//! host is pinned to GitHub's asset hosts, and `curl` carries connect/total
//! timeouts so a black-hole host can't tie up a blocking thread.
//!
//! The swap is staged, not in place: the new bundle is rsynced to a sibling
//! `Red.app.new`, then swapped in with atomic renames (with rollback); a failure
//! mid-copy never leaves a half-written, unrunnable `Red.app` behind.

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
    /// (Re)configure cadence + identity, sent at launch and on each settings
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
                None => break, // dispatch dropped the sender (service shutting down)
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
                if let Some(cfg) = &config
                    && cfg.enabled {
                        check(&events, cfg).await;
                    }
            }
        }
    }
}

/// One full check → (maybe) download → swap cycle, emitting `UpdateState` at each
/// transition. Blocking steps run on the blocking pool so the task, and the
/// dispatch loop sharing this thread, stay responsive.
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
            );
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
            );
        }
    };
    let asset_url = match release.asset_url {
        Some(url) => url,
        None => {
            return emit(
                events,
                UpdateState::Unsupported {
                    version: release.version,
                    url: release.html_url,
                },
            );
        }
    };
    let checksum_url = release.checksum_url;

    emit(
        events,
        UpdateState::Downloading {
            version: release.version.clone(),
            pct: 0,
        },
    );

    let version = release.version;
    let app = app_root.to_string_lossy().into_owned();
    match tokio::task::spawn_blocking(move || {
        download_and_swap(&asset_url, checksum_url.as_deref(), Path::new(&app))
    })
    .await
    {
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

/// The release-asset filename suffix this platform self-installs from: the macOS
/// `.dmg` or the Linux `.AppImage`. On platforms with no in-place updater
/// (Windows) `installed_app_root()` returns `None` first, so this is never used;
/// it just needs a value so the shared `fetch_latest_release` compiles.
#[cfg(target_os = "macos")]
const ASSET_SUFFIX: &str = ".dmg";
#[cfg(target_os = "linux")]
const ASSET_SUFFIX: &str = ".AppImage";
#[cfg(target_os = "windows")]
const ASSET_SUFFIX: &str = ".exe";
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
const ASSET_SUFFIX: &str = ".dmg";

/// The release fields the updater needs from the GitHub API.
struct Release {
    /// The tag, e.g. `v0.2.0`.
    version: String,
    /// The release's GitHub page, for the manual-download fallback.
    html_url: String,
    /// The self-install asset's download URL for this platform (`.dmg` /
    /// `.AppImage`), if present and served from a GitHub host.
    asset_url: Option<String>,
    /// The asset's `.sha256` sidecar URL, if the release publishes one. Used for
    /// integrity where there's no OS-level notarization (Linux).
    checksum_url: Option<String>,
}

/// GET the repo's latest *non-prerelease* release and pull out the dmg asset.
/// `/releases/latest` already excludes drafts and prereleases server-side.
fn fetch_latest_release(repo: &str) -> Result<Release, String> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let mut cmd = std::process::Command::new("curl");
    cmd.args([
        "-sSL",
        "--fail",
        "--proto",
        "=https",
        "--connect-timeout",
        "30",
        "--max-time",
        "60",
        "--max-redirs",
        "5",
        "-H",
        "Accept: application/vnd.github+json",
        "-H",
        "User-Agent: red-updater",
        &url,
    ]);
    // GUI-subsystem Windows builds would flash a console for each child process
    // (including these background checks), so spawn curl headless.
    #[cfg(target_os = "windows")]
    no_window(&mut cmd);
    let out = cmd.output().map_err(|e| format!("launching curl: {e}"))?;
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

    // Pick this platform's self-install asset by filename suffix, plus its optional
    // `.sha256` sidecar. Only accept a URL pointing at a GitHub asset host: the
    // bytes are fed straight to `curl`, so a release that named some other host (a
    // compromised API response) is treated as "no installable asset" and the UI
    // falls back to the manual-download link.
    let assets = json["assets"].as_array();
    let mut asset_url = None;
    let mut asset_name = None;
    if let Some(assets) = assets {
        for asset in assets {
            let Some(name) = asset["name"].as_str() else {
                continue;
            };
            if !name.ends_with(ASSET_SUFFIX) {
                continue;
            }
            if let Some(url) = asset["browser_download_url"].as_str()
                && is_allowed_download_url(url)
            {
                asset_url = Some(url.to_string());
                asset_name = Some(name.to_string());
                break;
            }
        }
    }
    let checksum_url = match (asset_name, assets) {
        (Some(name), Some(assets)) => {
            let want = format!("{name}.sha256");
            assets.iter().find_map(|asset| {
                let url = (asset["name"].as_str()? == want)
                    .then(|| asset["browser_download_url"].as_str())
                    .flatten()?;
                is_allowed_download_url(url).then(|| url.to_string())
            })
        }
        _ => None,
    };

    Ok(Release {
        version,
        html_url,
        asset_url,
        checksum_url,
    })
}

/// Whether `url` is an `https` URL on a GitHub asset host. GitHub serves release
/// assets from `github.com` (302-redirecting to `objects.githubusercontent.com`),
/// so both are allowed; anything else is rejected before the bytes reach `curl`.
fn is_allowed_download_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    // Host is everything up to the first `/`, `?`, or `#`; reject any userinfo
    // (`@`) or port (`:`) so `github.com` can't be spoofed by `github.com@evil`.
    let host = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if host.contains('@') || host.contains(':') {
        return false;
    }
    host == "github.com"
        || host == "objects.githubusercontent.com"
        || host.ends_with(".githubusercontent.com")
}

/// `true` when `latest` is a strictly higher semver than `current`. A tag that
/// doesn't parse is treated as not-newer; we never downgrade or sidegrade on a
/// tag we can't compare. Prerelease precedence follows semver: a release outranks
/// its own prerelease (so `v0.2.0` *is* newer than `v0.2.0-rc1`; a user on an rc
/// is offered the final), and two prereleases of the same core compare by their
/// identifier so `-rc2` > `-rc1`.
fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_semver(latest), parse_semver(current)) {
        (Some(l), Some(c)) => semver_cmp(&l, &c) == std::cmp::Ordering::Greater,
        _ => false,
    }
}

/// Major/minor/patch plus an optional prerelease identifier (the bit after `-`).
type Semver = (u64, u64, u64, Option<String>);

/// Parse a `vMAJOR.MINOR.PATCH[-PRERELEASE][+BUILD]` tag. Build metadata is
/// dropped (it has no precedence); the prerelease identifier is retained so
/// release-vs-prerelease ordering is correct. Missing patch defaults to 0.
fn parse_semver(tag: &str) -> Option<Semver> {
    // Build metadata (`+…`) never affects precedence, so strip it first.
    let body = tag.trim().trim_start_matches('v');
    let body = body.split('+').next().unwrap_or(body);
    let (core, pre) = match body.split_once('-') {
        Some((core, pre)) => (core, Some(pre.to_string())),
        None => (body, None),
    };
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch, pre))
}

/// Semver precedence: compare the numeric core first; on a tie a release (no
/// prerelease) outranks any prerelease of that core, and two prereleases compare
/// lexically by identifier.
fn semver_cmp(l: &Semver, r: &Semver) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let core = (l.0, l.1, l.2).cmp(&(r.0, r.1, r.2));
    if core != Ordering::Equal {
        return core;
    }
    match (&l.3, &r.3) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater, // release > its prerelease
        (Some(_), None) => Ordering::Less,
        (Some(a), Some(b)) => a.cmp(b),
    }
}

/// The installed `.app` bundle root *if* we're allowed to swap it: the running
/// executable must live in `…/Red.app/Contents/MacOS/Red` under a writable
/// `/Applications`. Anything else (a `cargo run` dev build, a Homebrew/read-only
/// install) returns `None`, which the caller surfaces as `Unsupported` with a
/// manual-download link, matching the plan's "don't fight the package manager".
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

/// The installed AppImage path *if* we're allowed to replace it. Self-update only
/// applies to an AppImage install: its runtime exports `$APPIMAGE` = the absolute
/// path of the running `.AppImage`. A distro/Flatpak package or a `cargo run` dev
/// build has no `$APPIMAGE` (→ `Unsupported` + manual-download link, per the plan's
/// "don't fight the package manager"). The file and its directory must be writable
/// so we can stage a sibling temp and atomically rename it into place.
#[cfg(target_os = "linux")]
fn installed_app_root() -> Option<std::path::PathBuf> {
    let path = std::path::PathBuf::from(std::env::var_os("APPIMAGE")?);
    let writable = |p: &Path| {
        std::fs::metadata(p)
            .map(|m| !m.permissions().readonly())
            .unwrap_or(false)
    };
    let ok = path.is_file() && writable(&path) && path.parent().map(writable).unwrap_or(false);
    ok.then_some(path)
}

/// The installed `Red.exe` path *if* we're allowed to replace it. Self-update only
/// applies to a **portable** install: the distributed zip drops a `.red-portable`
/// marker next to the exe, so a `cargo run` dev build or any loose exe (no marker)
/// is never silently overwritten. The exe and its directory must be writable so we
/// can stage a sibling temp and rename it into place (a Program Files install would
/// need elevation → the rename fails → `Failed`, not a silent no-op).
#[cfg(target_os = "windows")]
fn installed_app_root() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    if !dir.join(".red-portable").is_file() {
        return None;
    }
    let writable = |p: &Path| {
        std::fs::metadata(p)
            .map(|m| !m.permissions().readonly())
            .unwrap_or(false)
    };
    (writable(&exe) && writable(dir)).then_some(exe)
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn installed_app_root() -> Option<std::path::PathBuf> {
    None
}

/// Download the dmg, mount it (Gatekeeper validates the staple here), verify the
/// mounted bundle's signature + Team ID against the running app, then stage-and-
/// swap it over the installed bundle with atomic renames. The mount is always
/// detached and the temp dir cleaned, even on a failed swap.
#[cfg(target_os = "macos")]
fn download_and_swap(
    dmg_url: &str,
    _checksum_url: Option<&str>,
    app_root: &Path,
) -> Result<(), String> {
    // macOS integrity rests on notarization (validated on mount), not the sidecar
    // checksum, so `_checksum_url` is unused here.
    //
    // Defence in depth: the URL was already screened when the release was parsed,
    // but re-check here so this entry point can't be handed an arbitrary host.
    if !is_allowed_download_url(dmg_url) {
        return Err("refusing to download from a non-GitHub host".into());
    }

    // A private, hard-to-pre-create temp dir: `create_dir` (not `_all`) fails if
    // the path already exists (defeating a symlink planted at a predictable name),
    // and a nanosecond-stamped name keeps two updates (or an attacker's guess)
    // from colliding. 0700 so only we can read the bytes mid-download.
    let tmp = std::env::temp_dir().join(format!(
        "red-update-{}-{}",
        std::process::id(),
        unique_suffix()
    ));
    std::fs::create_dir(&tmp).map_err(|e| format!("creating temp dir: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o700));
    }
    let dmg = tmp.join("Red.dmg");
    let mount = tmp.join("mnt");

    let result = (|| -> Result<(), String> {
        let dmg_path = dmg.to_str().ok_or("non-UTF-8 temp path")?;
        run_cmd(
            "curl",
            &[
                "-sSL",
                "--fail",
                "--proto",
                "=https",
                "--connect-timeout",
                "30",
                "--max-time",
                "600",
                "--max-redirs",
                "5",
                "-o",
                dmg_path,
                dmg_url,
            ],
        )?;

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

        let mounted_app = mount.join("Red.app");
        // Authenticity gate: the staple proved integrity on mount, but not that
        // the contained app is *ours*. Verify the signature and require its Team
        // ID to match the running bundle's, then stage-and-swap. Detach happens
        // below regardless of this result.
        let swap = verify_authentic(&mounted_app, app_root)
            .and_then(|()| staged_swap(&mounted_app, app_root));

        let _ = std::process::Command::new("hdiutil")
            .args(["detach", mount_path])
            .output();
        swap
    })();

    let _ = std::fs::remove_dir_all(&tmp);
    result
}

/// Verify the mounted bundle is a genuine, Gatekeeper-acceptable Red build signed
/// by the same Team ID as the *running* app. Three independent checks: a strict
/// signature verification, a Gatekeeper assessment (notarization), and a Team ID
/// match, so a compromised release serving a different (but notarized) app can't
/// be installed over Red.
#[cfg(target_os = "macos")]
fn verify_authentic(mounted_app: &Path, running_app: &Path) -> Result<(), String> {
    let app = mounted_app.to_str().ok_or("non-UTF-8 mounted app path")?;
    run_cmd("codesign", &["--verify", "--deep", "--strict", app])
        .map_err(|e| format!("downloaded bundle failed signature verification: {e}"))?;
    run_cmd("spctl", &["--assess", "--type", "execute", app])
        .map_err(|e| format!("downloaded bundle is not notarized/accepted: {e}"))?;

    let theirs = team_identifier(mounted_app)
        .ok_or("downloaded bundle has no Team ID; refusing to install")?;
    // The running app passed Gatekeeper at launch; if we can read its Team ID,
    // require a match. If we can't (an unsigned dev build, or a codesign-output
    // change), the signature + spctl checks above still stand on their own, but
    // log it, since it's the anti-substitution control silently dropping out.
    match team_identifier(running_app) {
        Some(ours) if theirs != ours => {
            return Err(format!(
                "downloaded bundle Team ID ({theirs}) does not match the installed app ({ours})"
            ));
        }
        Some(_) => {}
        None => tracing::warn!(
            "could not read the running app's Team ID; installing on signature + notarization alone"
        ),
    }
    Ok(())
}

/// The `TeamIdentifier=` from `codesign -dvv` (printed to stderr). `None` when the
/// bundle is unsigned or has no team (an ad-hoc / dev build).
#[cfg(target_os = "macos")]
fn team_identifier(app: &Path) -> Option<String> {
    let out = std::process::Command::new("codesign")
        .args(["-dvv", app.to_str()?])
        .output()
        .ok()?;
    // `-dvv` writes the display to stderr.
    let text = String::from_utf8_lossy(&out.stderr);
    text.lines()
        .find_map(|line| line.trim().strip_prefix("TeamIdentifier="))
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty() && id != "not set")
}

/// Replace `app_root` with `staged` (a mounted `Red.app`) without ever leaving a
/// half-written bundle on disk: rsync into a sibling `Red.app.new`, then swap it in
/// with atomic renames; the running process keeps its already-mapped pages, so a
/// live swap is safe. On a failed final rename the previous bundle is restored.
#[cfg(target_os = "macos")]
fn staged_swap(staged: &Path, app_root: &Path) -> Result<(), String> {
    let parent = app_root.parent().ok_or("app has no parent dir")?;
    let new_app = parent.join("Red.app.new");
    let old_app = parent.join(format!("Red.app.old-{}", std::process::id()));

    // Clean any leftovers from a previous interrupted update.
    let _ = std::fs::remove_dir_all(&new_app);
    let _ = std::fs::remove_dir_all(&old_app);

    // Materialize the new bundle fully *beside* the live one. A failure here leaves
    // the running app untouched.
    let src = format!("{}/", staged.to_str().ok_or("bad mount path")?);
    let dst = format!("{}/", new_app.to_str().ok_or("bad staging path")?);
    if let Err(e) = run_cmd("rsync", &["-a", "--delete", &src, &dst]) {
        let _ = std::fs::remove_dir_all(&new_app);
        return Err(e);
    }

    // Two atomic renames: move the live bundle aside, move the new one in. If the
    // second fails, roll the original back so we never end up with no app.
    std::fs::rename(app_root, &old_app).map_err(|e| {
        let _ = std::fs::remove_dir_all(&new_app);
        format!("moving current bundle aside: {e}")
    })?;
    if let Err(e) = std::fs::rename(&new_app, app_root) {
        let _ = std::fs::rename(&old_app, app_root); // rollback
        let _ = std::fs::remove_dir_all(&new_app);
        return Err(format!("installing new bundle: {e}"));
    }

    let _ = std::fs::remove_dir_all(&old_app);
    Ok(())
}

/// A short, hard-to-guess suffix for the temp dir name, derived from the wall
/// clock's nanoseconds. Not a security boundary on its own (the 0700 dir +
/// `create_dir`-fails-if-exists are); it just makes the path unpredictable.
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// Linux AppImage self-update: download the new `.AppImage` beside the running one,
/// verify it against the release's `.sha256` sidecar (Linux builds aren't
/// OS-notarized, so integrity rests on that checksum, fetched over TLS from a
/// pinned GitHub host), mark it executable, then atomically rename it over the
/// running `$APPIMAGE`. Replacing an open file is safe on Linux: the live process
/// keeps its mapped inode and the next launch picks up the new file. Without a
/// sidecar we refuse to install rather than run unverified bytes.
#[cfg(target_os = "linux")]
fn download_and_swap(
    asset_url: &str,
    checksum_url: Option<&str>,
    target: &Path,
) -> Result<(), String> {
    // Defence in depth: both URLs were screened when the release was parsed.
    if !is_allowed_download_url(asset_url) {
        return Err("refusing to download from a non-GitHub host".into());
    }
    let checksum_url =
        checksum_url.ok_or("release has no .sha256 sidecar; refusing to self-update")?;
    if !is_allowed_download_url(checksum_url) {
        return Err("refusing to fetch checksum from a non-GitHub host".into());
    }

    let parent = target.parent().ok_or("AppImage has no parent dir")?;
    // Stage in the SAME directory as the target so the final rename is atomic (one
    // filesystem). A hidden pid+nanosecond name avoids colliding with a parallel
    // update and stays out of the user's way if we crash mid-download.
    let tmp = parent.join(format!(
        ".red-update-{}-{}.AppImage",
        std::process::id(),
        unique_suffix()
    ));

    let result = (|| -> Result<(), String> {
        let tmp_path = tmp.to_str().ok_or("non-UTF-8 temp path")?;
        run_cmd(
            "curl",
            &[
                "-sSL",
                "--fail",
                "--proto",
                "=https",
                "--connect-timeout",
                "30",
                "--max-time",
                "600",
                "--max-redirs",
                "5",
                "-o",
                tmp_path,
                asset_url,
            ],
        )?;

        let expected = fetch_checksum(checksum_url)?;
        let actual = sha256_file(tmp_path)?;
        if !actual.eq_ignore_ascii_case(&expected) {
            return Err(format!(
                "downloaded AppImage failed checksum (expected {expected}, got {actual})"
            ));
        }

        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("setting executable bit: {e}"))?;

        std::fs::rename(&tmp, target).map_err(|e| format!("installing new AppImage: {e}"))?;
        Ok(())
    })();

    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Fetch the `.sha256` sidecar and return its digest (the leading token of a
/// `sha256sum`-style `<hex>  <name>` line).
#[cfg(any(target_os = "linux", target_os = "windows"))]
fn fetch_checksum(url: &str) -> Result<String, String> {
    let mut cmd = std::process::Command::new("curl");
    cmd.args([
        "-sSL",
        "--fail",
        "--proto",
        "=https",
        "--connect-timeout",
        "30",
        "--max-time",
        "60",
        "--max-redirs",
        "5",
        url,
    ]);
    #[cfg(target_os = "windows")]
    no_window(&mut cmd);
    let out = cmd.output().map_err(|e| format!("launching curl: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "fetching checksum: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    parse_sha256_hex(&String::from_utf8_lossy(&out.stdout))
        .ok_or_else(|| "checksum sidecar is not a SHA-256 digest".into())
}

/// SHA-256 of a file via coreutils `sha256sum` (universally present on Linux),
/// keeping the updater free of a hashing dependency.
#[cfg(target_os = "linux")]
fn sha256_file(path: &str) -> Result<String, String> {
    let out = std::process::Command::new("sha256sum")
        .arg(path)
        .output()
        .map_err(|e| format!("launching sha256sum: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "sha256sum failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    parse_sha256_hex(&String::from_utf8_lossy(&out.stdout))
        .ok_or_else(|| "sha256sum produced no digest".into())
}

/// SHA-256 of a file via PowerShell's `Get-FileHash` (present on every supported
/// Windows), keeping the updater free of a hashing dependency. `.Hash` is clean
/// uppercase hex; the comparison is case-insensitive.
#[cfg(target_os = "windows")]
fn sha256_file(path: &str) -> Result<String, String> {
    let mut cmd = std::process::Command::new("powershell");
    cmd.args([
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        &format!(
            "(Get-FileHash -Algorithm SHA256 -LiteralPath {}).Hash",
            ps_quote(path)
        ),
    ]);
    no_window(&mut cmd);
    let out = cmd
        .output()
        .map_err(|e| format!("launching powershell: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "Get-FileHash failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    parse_sha256_hex(&String::from_utf8_lossy(&out.stdout))
        .ok_or_else(|| "Get-FileHash produced no digest".into())
}

/// Quote a path as a PowerShell single-quoted literal (backslashes are literal
/// inside single quotes; an embedded quote is escaped by doubling), so a path with
/// spaces or metacharacters can't break out of the `-Command` string.
#[cfg(target_os = "windows")]
fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Windows: spawn helper processes without flashing a console window: the release
/// build is a GUI-subsystem app, so a child console would pop up on every check.
#[cfg(target_os = "windows")]
fn no_window(cmd: &mut std::process::Command) -> &mut std::process::Command {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW)
}

/// The leading SHA-256 hex token of a `sha256sum`-style line, validated to be 64
/// hex chars. `None` for anything else.
#[cfg(any(target_os = "linux", target_os = "windows", test))]
fn parse_sha256_hex(s: &str) -> Option<String> {
    let tok = s.split_whitespace().next()?;
    (tok.len() == 64 && tok.bytes().all(|b| b.is_ascii_hexdigit())).then(|| tok.to_string())
}

/// Windows portable self-update: download the new `Red.exe` beside the running one,
/// verify it against the release's `.sha256` sidecar (no Authenticode check yet;
/// integrity rests on the checksum, fetched over TLS from a pinned GitHub host),
/// then replace-on-restart. A running `.exe` can't be deleted, but it *can* be
/// renamed: move the live exe to `Red.exe.old`, move the new one into place
/// (rolling back on failure), and reap the `.old` on next launch. Without a sidecar
/// we refuse rather than run unverified bytes.
#[cfg(target_os = "windows")]
fn download_and_swap(
    asset_url: &str,
    checksum_url: Option<&str>,
    target: &Path,
) -> Result<(), String> {
    if !is_allowed_download_url(asset_url) {
        return Err("refusing to download from a non-GitHub host".into());
    }
    let checksum_url =
        checksum_url.ok_or("release has no .sha256 sidecar; refusing to self-update")?;
    if !is_allowed_download_url(checksum_url) {
        return Err("refusing to fetch checksum from a non-GitHub host".into());
    }

    let parent = target.parent().ok_or("exe has no parent dir")?;
    let name = target
        .file_name()
        .and_then(|f| f.to_str())
        .ok_or("exe has no file name")?;
    // Stage in the SAME directory as the target so the final rename is atomic (one
    // volume). Unique pid+nanosecond name avoids colliding with a parallel update.
    let tmp = parent.join(format!(
        "red-update-{}-{}.exe",
        std::process::id(),
        unique_suffix()
    ));
    let old = parent.join(format!("{name}.old"));

    let result = (|| -> Result<(), String> {
        let tmp_path = tmp.to_str().ok_or("non-UTF-8 temp path")?;
        run_cmd(
            "curl",
            &[
                "-sSL",
                "--fail",
                "--proto",
                "=https",
                "--connect-timeout",
                "30",
                "--max-time",
                "600",
                "--max-redirs",
                "5",
                "-o",
                tmp_path,
                asset_url,
            ],
        )?;

        let expected = fetch_checksum(checksum_url)?;
        let actual = sha256_file(tmp_path)?;
        if !actual.eq_ignore_ascii_case(&expected) {
            return Err(format!(
                "downloaded exe failed checksum (expected {expected}, got {actual})"
            ));
        }

        // Clear any leftover `.old` from a prior update, then rename the live exe
        // aside and the new one into place. Roll back if the second rename fails so
        // we never end up with no exe at the install path.
        let _ = std::fs::remove_file(&old);
        std::fs::rename(target, &old).map_err(|e| format!("moving current exe aside: {e}"))?;
        if let Err(e) = std::fs::rename(&tmp, target) {
            let _ = std::fs::rename(&old, target);
            return Err(format!("installing new exe: {e}"));
        }
        Ok(())
    })();

    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn download_and_swap(
    _asset_url: &str,
    _checksum_url: Option<&str>,
    _target: &Path,
) -> Result<(), String> {
    Err("self-update is not supported on this platform".into())
}

/// Run a helper process, mapping a non-zero exit (or a launch failure) to a
/// human-readable error carrying its stderr.
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
fn run_cmd(program: &str, args: &[&str]) -> Result<(), String> {
    let mut cmd = std::process::Command::new(program);
    cmd.args(args);
    #[cfg(target_os = "windows")]
    no_window(&mut cmd);
    let out = cmd
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
        assert_eq!(parse_semver("v0.1.2"), Some((0, 1, 2, None)));
        assert_eq!(parse_semver("0.1.2"), Some((0, 1, 2, None)));
        assert_eq!(parse_semver("v1.2"), Some((1, 2, 0, None)));
        assert_eq!(
            parse_semver("v0.2.0-rc1"),
            Some((0, 2, 0, Some("rc1".into())))
        );
        // Build metadata is dropped; the prerelease (if any) is kept.
        assert_eq!(parse_semver("v1.0.0+build5"), Some((1, 0, 0, None)));
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

    #[test]
    fn prerelease_precedence_offers_the_final_release() {
        // A user on a prerelease IS offered the matching final release.
        assert!(is_newer("v0.2.0", "v0.2.0-rc1"));
        // But not the other way round, and a release is never "newer" than itself.
        assert!(!is_newer("v0.2.0-rc1", "v0.2.0"));
        assert!(!is_newer("v0.2.0", "v0.2.0"));
        // Later prerelease of the same core supersedes the earlier one.
        assert!(is_newer("v0.2.0-rc2", "v0.2.0-rc1"));
        assert!(!is_newer("v0.2.0-rc1", "v0.2.0-rc2"));
        // A higher core beats any prerelease regardless of suffix.
        assert!(is_newer("v0.3.0-rc1", "v0.2.0"));
    }

    #[test]
    fn sha256_hex_is_validated() {
        let valid = "a".repeat(64);
        // The leading token of a `sha256sum` line, or a bare digest, parses.
        assert_eq!(
            parse_sha256_hex(&format!("{valid}  Red-1.0.0-x86_64.AppImage")),
            Some(valid.clone())
        );
        assert_eq!(parse_sha256_hex(&valid), Some(valid));
        // Wrong length or non-hex is rejected, so a garbage sidecar can't pass.
        assert_eq!(parse_sha256_hex("deadbeef  short"), None);
        assert_eq!(parse_sha256_hex(&format!("{}  x", "g".repeat(64))), None);
        assert_eq!(parse_sha256_hex(""), None);
    }

    #[test]
    fn download_host_is_pinned_to_github() {
        // GitHub's two asset hosts are accepted.
        assert!(is_allowed_download_url(
            "https://github.com/o/r/releases/download/v1/Red.dmg"
        ));
        assert!(is_allowed_download_url(
            "https://objects.githubusercontent.com/github-production-release-asset/x/Red.dmg"
        ));
        // Everything else is refused before the bytes reach curl.
        assert!(!is_allowed_download_url(
            "http://github.com/o/r/Red.dmg" // not https
        ));
        assert!(!is_allowed_download_url("https://evil.example.com/Red.dmg"));
        assert!(!is_allowed_download_url(
            "https://evil.com/github.com/Red.dmg"
        ));
        // Userinfo / port spoofs that embed `github.com` in the authority.
        assert!(!is_allowed_download_url(
            "https://github.com@evil.com/Red.dmg"
        ));
        assert!(!is_allowed_download_url(
            "https://github.com.evil.com/Red.dmg"
        ));
    }
}
