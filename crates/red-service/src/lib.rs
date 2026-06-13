//! The backend thread. Mirrors `nyx-service`: a dedicated OS thread runs its own
//! Tokio runtime, owns the active database session, and communicates with the
//! GPUI UI over two channels — `Command` in (UI → service, a Tokio mpsc usable
//! from any thread) and `Event` out (service → UI, a `futures` mpsc the GPUI
//! foreground executor can `await` as a `Stream`). The UI never blocks on the
//! backend.
//!
//! Querying is **pull-based and windowed**: `Query` opens a streaming cursor and
//! delivers the first window; each `FetchMore` pulls the next. This gives true
//! end-to-end backpressure (the backend never races ahead of the consumer) and
//! is the seam the result grid's lazy load-on-scroll plugs into. A fetch is
//! raced against incoming commands so a `Cancel` — or a `timeout` — can abort an
//! in-flight query out-of-band rather than dropping a future.
//!
//! Layout: [`protocol`] holds the `Command`/`Event`/`RunFetch` wire types,
//! [`dispatch`] the command pump, and this module the UI-facing handles.

mod dispatch;
mod protocol;
#[cfg(test)]
mod tests;
mod update;

pub use protocol::{Command, Event, RunFetch, SessionId, SortKey, UpdateConfig};

use futures::channel::mpsc::{unbounded, UnboundedReceiver};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender as CmdSender};

/// One command on its way to the backend, tagged with the session it routes to
/// (`None` for the session-less `TestConnection`/`Shutdown`). See `protocol`.
pub type Envelope = (Option<SessionId>, Command);

/// A cloneable handle that can only *send* commands, **bound to one session** so
/// its sends are stamped automatically. Handed to the result grid so its
/// load-on-scroll callback can request pages mid-render without touching the
/// (non-cloneable) `ServiceHandle`, the UI entity, or the session id.
#[derive(Clone)]
pub struct CommandSender {
    tx: CmdSender<Envelope>,
    session: SessionId,
}

impl CommandSender {
    pub fn send(&self, command: Command) {
        let _ = self.tx.send((Some(self.session), command));
    }
}

/// The UI's handle on the backend: send commands, take the event stream once.
pub struct ServiceHandle {
    commands: CmdSender<Envelope>,
    events: Option<UnboundedReceiver<(Option<SessionId>, Event)>>,
}

impl ServiceHandle {
    /// Fire a command at a specific session. Infallible from the caller's view —
    /// if the backend is gone the command is dropped.
    pub fn send_to(&self, session: SessionId, command: Command) {
        let _ = self.commands.send((Some(session), command));
    }

    /// Fire a session-less command (`TestConnection`, `Shutdown`,
    /// `SetActiveSession`).
    pub fn send_global(&self, command: Command) {
        let _ = self.commands.send((None, command));
    }

    /// A cloneable send-only handle bound to `session` (for the result grid's
    /// page requests).
    pub fn command_sender(&self, session: SessionId) -> CommandSender {
        CommandSender {
            tx: self.commands.clone(),
            session,
        }
    }

    /// Take the event stream. Call once; it moves into the UI's async loop. Each
    /// item is `(session, event)` — the session the event belongs to.
    pub fn take_events(&mut self) -> Option<UnboundedReceiver<(Option<SessionId>, Event)>> {
        self.events.take()
    }
}

/// Spawn the backend thread and return its handle. The thread owns a
/// current-thread Tokio runtime; the blocking SQLite work runs on its blocking
/// pool, so the dispatch loop never stalls.
pub fn spawn() -> ServiceHandle {
    let (cmd_tx, cmd_rx) = unbounded_channel::<Envelope>();
    // The event channel is unbounded, but its *heavy* producers are not: every
    // row-carrying event (`ResultPageLoaded` / `ResultRunLoaded` / `CopyRowsLoaded`)
    // is emitted only from a task holding a `page_fetch_limit` permit, and exports
    // from an `export_limit` permit (see `dispatch`). So even if the GPUI consumer
    // stalls (a modal, a slow frame), at most `MAX_CONCURRENT_PAGE_FETCHES` row
    // windows can be in flight — the queue can't balloon without bound. The
    // remaining producers (errors, status, throttled progress counts) carry small
    // payloads at human/throttled rates. Backpressure thus lives on the producer
    // side (the semaphores) rather than on the channel, which keeps `emit` a cheap
    // non-blocking call usable from the synchronous dispatch loop.
    let (evt_tx, evt_rx) = unbounded::<(Option<SessionId>, Event)>();

    std::thread::Builder::new()
        .name("red-service".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                // I/O enabled too: the Postgres driver's network connection needs it.
                .enable_all()
                .build()
                .expect("build red-service tokio runtime");
            // A panic in the dispatch loop (e.g. a driver bug) would otherwise just
            // drop the event sender, leaving the UI to wonder why the backend went
            // silent. Catch it, log it, and surface a clean error so the user sees
            // *something* before the thread exits. `report` keeps a live sender for
            // that final message even as the unwinding loop drops its own.
            let report = evt_tx.clone();
            let run =
                std::panic::AssertUnwindSafe(|| rt.block_on(dispatch::dispatch(cmd_rx, evt_tx)));
            if let Err(panic) = std::panic::catch_unwind(run) {
                let detail = panic_message(panic.as_ref());
                tracing::error!(detail, "red-service dispatch loop panicked");
                let _ = report
                    .unbounded_send((None, Event::Error(format!("backend crashed: {detail}"))));
            }
        })
        .expect("spawn red-service thread");

    ServiceHandle {
        commands: cmd_tx,
        events: Some(evt_rx),
    }
}

/// Best-effort message from a caught panic payload. Payloads are `dyn Any`, so
/// only the common `&str`/`String` forms are recoverable; anything else reports a
/// placeholder.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}
