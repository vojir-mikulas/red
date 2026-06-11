//! Live-reload watcher over `settings.toml`.
//!
//! A background [`notify`] watcher forwards debounced change notifications to the
//! UI over an async channel the app drains on its foreground executor (mirroring
//! how backend `Event`s are drained). Editing the file in any editor re-applies
//! within a frame — no restart.
//!
//! **Self-write suppression.** RED's own atomic save (temp file + rename) trips
//! the watcher too. Before each save the app records a hash of the bytes it's
//! about to write via [`SettingsWatcher::note_self_write`]; when the watcher then
//! sees the file land with exactly those bytes it drops the event, so a UI-driven
//! save never triggers a reload storm.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

/// Owns the OS watcher (kept alive for its lifetime) and the self-write guard.
pub(crate) struct SettingsWatcher {
    _watcher: RecommendedWatcher,
    /// Hash of the bytes of our last self-write, ignored once when it lands.
    expected: Arc<Mutex<Option<u64>>>,
}

impl SettingsWatcher {
    /// Start watching `path`'s parent directory (watching the file directly misses
    /// the atomic-rename replace on most platforms). Returns the watcher plus the
    /// receiver the app drains; `None` if the platform watcher can't start —
    /// live reload is a convenience, never load-bearing.
    pub(crate) fn start(path: PathBuf) -> Option<(Self, UnboundedReceiver<()>)> {
        let dir = path.parent()?.to_path_buf();
        // The config dir may not exist on a fresh install; create it so the
        // watcher can attach (and so the first save lands somewhere watched).
        std::fs::create_dir_all(&dir).ok();
        let (tx, rx) = unbounded::<()>();
        let expected: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));

        let handler = ReloadHandler {
            path,
            tx,
            expected: expected.clone(),
            last_sent: Mutex::new(None),
        };
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

/// The watcher-thread side: filters events to our file, suppresses self-writes,
/// and debounces the bursts editors emit on save.
struct ReloadHandler {
    path: PathBuf,
    tx: UnboundedSender<()>,
    expected: Arc<Mutex<Option<u64>>>,
    last_sent: Mutex<Option<Instant>>,
}

impl ReloadHandler {
    /// Debounce window: editors fire several events per save; coalesce them.
    const DEBOUNCE: Duration = Duration::from_millis(120);

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

        // Suppress the reload our own atomic save just triggered.
        if let Ok(contents) = std::fs::read_to_string(&self.path) {
            if let Ok(mut slot) = self.expected.lock() {
                if *slot == Some(hash(&contents)) {
                    *slot = None;
                    return;
                }
            }
        }

        // Debounce: skip if we forwarded a notification a moment ago.
        if let Ok(mut last) = self.last_sent.lock() {
            let now = Instant::now();
            if last.is_some_and(|t| now.duration_since(t) < Self::DEBOUNCE) {
                return;
            }
            *last = Some(now);
        }

        let _ = self.tx.unbounded_send(());
    }
}

fn hash(contents: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    contents.hash(&mut hasher);
    hasher.finish()
}
