//! Live-reload watcher over `settings.toml`.
//!
//! A background [`notify`] watcher forwards debounced change notifications to the
//! UI over an async channel the app drains on its foreground executor (mirroring
//! how backend `Event`s are drained). Editing the file in any editor re-applies
//! within a frame; no restart.
//!
//! **Trailing-edge debounce.** Editors fire several filesystem events per save
//! (and some save non-atomically: truncate, then write). A dedicated debounce
//! thread coalesces the burst and only reads/forwards once the file has been
//! quiet for [`DEBOUNCE`], so a reload never reflects a half-written file.
//!
//! **Self-write suppression.** RED's own atomic save (temp file + rename) trips
//! the watcher too. Before each save the app records a hash of the bytes it's
//! about to write via [`SettingsWatcher::note_self_write`]; when the debounce
//! thread then reads the settled file and sees exactly those bytes it drops the
//! event, so a UI-driven save never triggers a reload storm.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use futures::channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

/// How long the file must be quiet before a coalesced burst is forwarded.
const DEBOUNCE: Duration = Duration::from_millis(120);

/// Owns the OS watcher (kept alive for its lifetime) and the self-write guard.
pub(crate) struct SettingsWatcher {
    _watcher: RecommendedWatcher,
    /// Hash of the bytes of our last self-write, ignored once when it lands.
    expected: Arc<Mutex<Option<u64>>>,
}

impl SettingsWatcher {
    /// Start watching `path`'s parent directory (watching the file directly misses
    /// the atomic-rename replace on most platforms). Returns the watcher plus the
    /// receiver the app drains; `None` if the platform watcher can't start
    /// (live reload is a convenience, never load-bearing).
    pub(crate) fn start(path: PathBuf) -> Option<(Self, UnboundedReceiver<()>)> {
        let dir = path.parent()?.to_path_buf();
        // The config dir may not exist on a fresh install; create it so the
        // watcher can attach (and so the first save lands somewhere watched).
        std::fs::create_dir_all(&dir).ok();
        let (tx, rx) = unbounded::<()>();
        let expected: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));

        // The notify callback only pings the debounce thread; that thread does the
        // settle-wait, the settled read, self-write suppression, and the forward.
        let (ping_tx, ping_rx) = mpsc::channel::<()>();
        spawn_debounce(ping_rx, path.clone(), tx, expected.clone());

        let handler = ReloadHandler { path, ping_tx };
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
            if let Ok(event) = res {
                handler.handle(event);
            }
        })
        .ok()?;
        watcher.watch(&dir, RecursiveMode::NonRecursive).ok()?;

        Some((
            Self {
                _watcher: watcher,
                expected,
            },
            rx,
        ))
    }

    /// Record the bytes of an imminent self-write so the reload it triggers is
    /// dropped rather than echoed back into the running app.
    pub(crate) fn note_self_write(&self, contents: &str) {
        if let Ok(mut slot) = self.expected.lock() {
            *slot = Some(hash(contents));
        }
    }
}

/// The watcher-thread side: filters events to our file and pings the debounce
/// thread. Deliberately does no file IO; reading happens after the burst settles.
struct ReloadHandler {
    path: PathBuf,
    ping_tx: mpsc::Sender<()>,
}

impl ReloadHandler {
    fn handle(&self, event: Event) {
        if !matches!(
            event.kind,
            EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
        ) {
            return;
        }
        if !event.paths.iter().any(|p| p == &self.path) {
            return;
        }
        // Wake the debounce thread; a closed channel (watcher dropped) is ignored.
        let _ = self.ping_tx.send(());
    }
}

/// Coalesce a burst of file events and forward one reload once the file has been
/// quiet for [`DEBOUNCE`], reading the *settled* contents (so a partial mid-write
/// read can't win) and suppressing the app's own atomic save. Exits when the
/// watcher (and thus the ping sender) is dropped, or the UI receiver closes.
fn spawn_debounce(
    ping_rx: mpsc::Receiver<()>,
    path: PathBuf,
    tx: UnboundedSender<()>,
    expected: Arc<Mutex<Option<u64>>>,
) {
    thread::spawn(move || {
        // Block for the first event of a burst; exit when the watcher is dropped.
        while ping_rx.recv().is_ok() {
            // Drain follow-on events until the file is quiet for the window.
            loop {
                match ping_rx.recv_timeout(DEBOUNCE) {
                    Ok(()) => continue,
                    Err(mpsc::RecvTimeoutError::Timeout) => break,
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                }
            }
            // Settled: suppress our own atomic save, else forward one reload.
            if let Ok(contents) = std::fs::read_to_string(&path) {
                if let Ok(mut slot) = expected.lock() {
                    if *slot == Some(hash(&contents)) {
                        *slot = None;
                        continue;
                    }
                }
            }
            if tx.unbounded_send(()).is_err() {
                return; // UI gone; stop watching.
            }
        }
    });
}

fn hash(contents: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    contents.hash(&mut hasher);
    hasher.finish()
}
