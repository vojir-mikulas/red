// SPDX-License-Identifier: GPL-3.0-or-later

//! The backend thread. Mirrors `nyx-service`: a dedicated OS thread runs its own
//! Tokio runtime, owns the active database session, and communicates with the
//! GPUI UI over two channels — `Command` in (UI → service, a Tokio mpsc usable
//! from any thread) and `Event` out (service → UI, a `futures` mpsc the GPUI
//! foreground executor can `await` as a `Stream`). The UI never blocks on the
//! backend.

use std::sync::Arc;

use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};
use red_core::{ConnectionConfig, DbKind, QueryResult};
use red_driver::{DatabaseDriver, SqliteDriver};
use tokio::sync::mpsc::{
    unbounded_channel, UnboundedReceiver as CmdReceiver, UnboundedSender as CmdSender,
};

/// UI → service. One active session at a time, driven across many commands.
#[derive(Debug)]
pub enum Command {
    Connect(ConnectionConfig),
    Query(String),
    Shutdown,
}

/// service → UI. Streamed into the UI's async loop.
#[derive(Debug)]
pub enum Event {
    Connected,
    QueryComplete(QueryResult),
    Error(String),
}

/// The UI's handle on the backend: send commands, take the event stream once.
pub struct ServiceHandle {
    commands: CmdSender<Command>,
    events: Option<UnboundedReceiver<Event>>,
}

impl ServiceHandle {
    /// Fire a command at the backend. Infallible from the caller's view — if the
    /// backend is gone the command is dropped.
    pub fn send(&self, command: Command) {
        let _ = self.commands.send(command);
    }

    /// Take the event stream. Call once; it moves into the UI's async loop.
    pub fn take_events(&mut self) -> Option<UnboundedReceiver<Event>> {
        self.events.take()
    }
}

/// Spawn the backend thread and return its handle. The thread owns a
/// current-thread Tokio runtime; the blocking SQLite work runs on its blocking
/// pool, so the dispatch loop never stalls.
pub fn spawn() -> ServiceHandle {
    let (cmd_tx, cmd_rx) = unbounded_channel::<Command>();
    let (evt_tx, evt_rx) = futures::channel::mpsc::unbounded::<Event>();

    std::thread::Builder::new()
        .name("red-service".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("build red-service tokio runtime");
            rt.block_on(dispatch(cmd_rx, evt_tx));
        })
        .expect("spawn red-service thread");

    ServiceHandle {
        commands: cmd_tx,
        events: Some(evt_rx),
    }
}

async fn dispatch(mut commands: CmdReceiver<Command>, events: UnboundedSender<Event>) {
    let mut session: Option<Arc<dyn DatabaseDriver>> = None;

    while let Some(command) = commands.recv().await {
        match command {
            Command::Connect(config) => match connect(&config).await {
                Ok(driver) => {
                    session = Some(driver);
                    emit(&events, Event::Connected);
                }
                Err(message) => emit(&events, Event::Error(message)),
            },
            Command::Query(sql) => match &session {
                Some(driver) => match driver.query(&sql).await {
                    Ok(result) => emit(&events, Event::QueryComplete(result)),
                    Err(err) => emit(&events, Event::Error(err.to_string())),
                },
                None => emit(&events, Event::Error("not connected".into())),
            },
            Command::Shutdown => break,
        }
    }
}

async fn connect(config: &ConnectionConfig) -> Result<Arc<dyn DatabaseDriver>, String> {
    match config.kind {
        DbKind::Sqlite => {
            let driver = SqliteDriver::new(config.dsn.clone(), config.read_only);
            driver.ping().await.map_err(|e| e.to_string())?;
            Ok(Arc::new(driver))
        }
        DbKind::Postgres => Err("Postgres driver not yet implemented".into()),
    }
}

/// The UI may have dropped its receiver (window closed) — a failed send is the
/// expected shutdown path, not an error.
fn emit(events: &UnboundedSender<Event>, event: Event) {
    let _ = events.unbounded_send(event);
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use red_core::Value;

    #[tokio::test]
    async fn connect_and_query_roundtrip() {
        let mut handle = spawn();
        handle.send(Command::Connect(ConnectionConfig {
            name: "scratch".into(),
            kind: DbKind::Sqlite,
            dsn: ":memory:".into(),
            read_only: false,
        }));
        handle.send(Command::Query("SELECT 42 AS answer".into()));
        let mut events = handle.take_events().expect("event stream");

        assert!(matches!(events.next().await, Some(Event::Connected)));
        match events.next().await {
            Some(Event::QueryComplete(result)) => {
                assert_eq!(result.rows[0][0], Value::Integer(42));
            }
            other => panic!("expected QueryComplete, got {other:?}"),
        }

        handle.send(Command::Shutdown);
    }
}
