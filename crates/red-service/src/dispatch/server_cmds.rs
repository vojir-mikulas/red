//! The Server panel's command handlers, for all three seams: the live metrics
//! sample, the session listing, and the kill.
//!
//! Split out of [`schema_cmds`](super::schema_cmds) because this is a different
//! concern -- what the server is doing *now*, not what is in its catalog -- and
//! because it is the one dispatch area that must answer for SQL, Redis and Mongo
//! alike. Like every handler there, each of these borrows the (immutable)
//! session map, resolves its driver, and **spawns** the round trip: `CLIENT LIST`
//! on a server with ten thousand clients is not cheap, and the command pump
//! serves every other session's fetches and cancels.
//!
//! **The adapters are the seam.** `KvDriver::client_list` returns `ClientInfo`
//! and `DocDriver::current_ops` returns `DocOp`, because those are the engines'
//! actual shapes and that is what the driver traits are supposed to model.
//! Flattening them into [`ServerSession`] happens here, once, so the UI holds
//! one type and one code path. Both mappings are lossy in ways named at the
//! conversion site.

use std::collections::HashMap;

use red_core::server::ServerSnapshot;
use red_core::{ServerSession, SessionKey};

use crate::protocol::Epoch;
use crate::{Event, SessionId};

use super::session::{SessionDriver, SessionState};
use super::{Events, emit};

/// Most sessions one listing returns. The SQL drivers cap their own queries and
/// `current_ops` caps its pipeline; this bounds the one seam that cannot push a
/// limit down (`CLIENT LIST` is all-or-nothing), and is reported honestly
/// through the same `restricted` flag rather than silently truncating.
const MAX_SESSIONS: usize = 500;

/// `FetchServerMetrics`: one sample of the server's live state.
///
/// Failure is reported rather than swallowed. Unlike a catalog read that merely
/// leaves a tree branch empty, this call *is* the Overview's whole content, so a
/// silent failure would read as a healthy, idle server.
pub(super) fn fetch_server_metrics(
    sessions: &HashMap<SessionId, SessionState>,
    session_id: Option<SessionId>,
    events: &Events,
    epoch: Epoch,
) {
    let Some(id) = session_id else { return };
    let Some(driver) = sessions.get(&id).map(|s| s.driver.clone()) else {
        fail_metrics(events, session_id, epoch, "not connected");
        return;
    };
    let events = events.clone();
    tokio::spawn(async move {
        let sampled: red_core::Result<ServerSnapshot> = match &driver {
            SessionDriver::Sql(d) => d.server_metrics().await,
            SessionDriver::Kv(d) => d.server_metrics().await,
            SessionDriver::Doc(d) => d.server_metrics().await,
        };
        match sampled {
            Ok(snapshot) => emit(
                &events,
                session_id,
                Event::ServerMetricsReady { epoch, snapshot },
            ),
            Err(e) => emit(
                &events,
                session_id,
                Event::ServerMetricsFailed {
                    epoch,
                    message: e.to_string(),
                },
            ),
        }
    });
}

fn fail_metrics(events: &Events, session_id: Option<SessionId>, epoch: Epoch, message: &str) {
    emit(
        events,
        session_id,
        Event::ServerMetricsFailed {
            epoch,
            message: message.to_string(),
        },
    );
}

/// `ListServerSessions`: what the server is doing right now, whichever seam this
/// session holds.
///
/// Errors are reported rather than swallowed, for the same reason as above: this
/// call is the panel's whole content, so a failure has to be visible or the
/// panel just looks empty.
pub(super) fn list_server_sessions(
    sessions: &HashMap<SessionId, SessionState>,
    session_id: Option<SessionId>,
    events: &Events,
) {
    let Some(id) = session_id else { return };
    let Some(driver) = sessions.get(&id).map(|s| s.driver.clone()) else {
        emit(events, session_id, Event::Error("not connected".into()));
        return;
    };
    let events = events.clone();
    tokio::spawn(async move {
        let listed = match &driver {
            SessionDriver::Sql(d) => d.server_sessions().await,
            SessionDriver::Kv(d) => {
                // `CLIENT ID` first: without it every row would offer a kill,
                // including the connection the list came in on.
                let own = d.client_id().await;
                d.client_list()
                    .await
                    .map(|clients| cap(clients.iter().map(|c| from_client(c, own)).collect()))
            }
            SessionDriver::Doc(d) => d
                .current_ops()
                .await
                .map(|ops| cap(ops.iter().map(from_doc_op).collect())),
        };
        match listed {
            Ok((sessions, restricted)) => emit(
                &events,
                session_id,
                Event::ServerSessionsReady {
                    sessions,
                    restricted,
                },
            ),
            Err(e) => emit(&events, session_id, Event::Error(e.to_string())),
        }
    });
}

/// `KillServerSession`: stop one session's statement, or the session itself.
///
/// The read-only gate is enforced here as well as in the driver. The UI grades
/// the confirmation and the driver refuses a read-only connection; this is the
/// third rail, and it is deliberate for a command whose blast radius is someone
/// else's transaction. Deliberately absent from every AI and MCP tool catalog,
/// on every seam, including Mongo's `killOp`.
pub(super) fn kill_server_session(
    sessions: &HashMap<SessionId, SessionState>,
    session_id: Option<SessionId>,
    events: &Events,
    key: SessionKey,
    mode: red_core::KillMode,
) {
    let Some(id) = session_id else { return };
    let Some(state) = sessions.get(&id) else {
        emit(events, session_id, Event::Error("not connected".into()));
        return;
    };
    if state.read_only {
        emit(
            events,
            session_id,
            Event::Error("this connection is read-only".into()),
        );
        return;
    }
    let driver = state.driver.clone();
    let events = events.clone();
    tokio::spawn(async move {
        // The key is opaque text minted by the driver that listed it, so each
        // seam parses back its own shape. A key that does not parse is a stale
        // list or a transposed connection, and refusing beats killing whatever
        // the number happens to name on this server.
        let killed = match &driver {
            SessionDriver::Sql(d) => d.kill_session(&key, mode).await,
            SessionDriver::Kv(d) => match key.0.parse::<i64>() {
                Ok(id) => d.client_kill(id).await,
                Err(_) => Err(bad_key(&key)),
            },
            SessionDriver::Doc(d) => match key.0.parse::<i64>() {
                Ok(opid) => d.kill_op(opid).await,
                Err(_) => Err(bad_key(&key)),
            },
        };
        match killed {
            Ok(()) => emit(
                &events,
                session_id,
                Event::ServerSessionKilled { key, mode },
            ),
            Err(e) => emit(&events, session_id, Event::Error(e.to_string())),
        }
    });
}

fn bad_key(key: &SessionKey) -> red_core::RedError {
    red_core::RedError::Driver(format!("`{key}` is not a session on this server"))
}

/// Longest-running first, then capped. The order matters more than the cap: a
/// truncated list that kept the *newest* connections would hide exactly the
/// runaway operation the panel was opened to find.
fn cap(mut sessions: Vec<ServerSession>) -> (Vec<ServerSession>, bool) {
    sessions.sort_by(|a, b| {
        b.elapsed_secs
            .partial_cmp(&a.elapsed_secs)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let over = sessions.len() > MAX_SESSIONS;
    sessions.truncate(MAX_SESSIONS);
    (sessions, over)
}

/// One `CLIENT LIST` row as a [`ServerSession`].
///
/// Lossy in both directions, and deliberately so. Redis has no user per
/// connection and no lock manager, so `user`, `blocked_by` and `wait` are always
/// empty; `idle` and `resp` have no home here and stay in the Monitor tab's
/// richer client view, because widening the shared type for one engine's fields
/// is how a shared panel turns into a union of three engines' quirks.
///
/// `age` is the connection's lifetime, not the current command's: Redis does not
/// report per-command elapsed time. That makes it the wrong number to *sort* a
/// runaway command by, and the right one for "this connection has been parked
/// here for an hour", which is what a Redis operator actually looks for.
fn from_client(c: &red_core::kv::ClientInfo, own: Option<i64>) -> ServerSession {
    ServerSession {
        key: SessionKey(c.id.to_string()),
        user: None,
        application: (!c.name.is_empty()).then(|| c.name.clone()),
        client_addr: (!c.addr.is_empty()).then(|| c.addr.clone()),
        database: Some(format!("db{}", c.db)),
        state: c.flags.clone(),
        wait: None,
        blocked_by: Vec::new(),
        query: (!c.cmd.is_empty()).then(|| c.cmd.clone()),
        elapsed_secs: c.age as f64,
        // `None` from `CLIENT ID` means "could not tell", not "none of these is
        // ours": the panel then offers the kill and the confirm names the
        // address, rather than silently withholding it from every row.
        is_self: own.is_some_and(|own| own == c.id),
    }
}

/// One `$currentOp` entry as a [`ServerSession`].
///
/// Also lossy: Mongo reports no user or application name on an op, and the lock
/// wait is a boolean rather than a graph, so `wait` carries the fact and
/// `blocked_by` stays empty (`session_caps().has_wait_graph` is false for Mongo,
/// which is how the panel knows a flat list is the engine's answer rather than
/// "nothing is blocked").
fn from_doc_op(op: &red_core::doc::DocOp) -> ServerSession {
    ServerSession {
        key: SessionKey(op.opid.to_string()),
        user: None,
        application: None,
        client_addr: op.client.clone(),
        database: (!op.namespace.is_empty()).then(|| op.namespace.clone()),
        state: op.op.clone(),
        wait: op
            .waiting_for_lock
            .then(|| "waiting for a lock".to_string()),
        blocked_by: Vec::new(),
        query: op.command.clone(),
        elapsed_secs: op.secs_running,
        is_self: op.is_self,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use red_core::doc::DocOp;
    use red_core::kv::ClientInfo;

    fn client(id: i64, age: u64) -> ClientInfo {
        ClientInfo {
            id,
            addr: "10.0.0.4:51820".into(),
            name: "worker-3".into(),
            db: 2,
            age,
            idle: 4,
            flags: "N".into(),
            cmd: "get".into(),
            resp: "3".into(),
        }
    }

    fn op(opid: i64, secs: f64) -> DocOp {
        DocOp {
            opid,
            op: "getmore".into(),
            namespace: "shop.orders".into(),
            secs_running: secs,
            client: Some("10.0.0.9:40100".into()),
            command: Some("{\"getMore\":1}".into()),
            waiting_for_lock: false,
            is_self: false,
        }
    }

    #[test]
    fn a_redis_client_maps_onto_the_shared_session_shape() {
        let s = from_client(&client(42, 3_600), Some(7));
        assert_eq!(s.key, SessionKey("42".into()));
        assert_eq!(s.application.as_deref(), Some("worker-3"));
        assert_eq!(s.client_addr.as_deref(), Some("10.0.0.4:51820"));
        assert_eq!(s.database.as_deref(), Some("db2"));
        assert_eq!(s.state, "N");
        assert_eq!(s.query.as_deref(), Some("get"));
        assert_eq!(s.elapsed_secs, 3_600.0);
        // Redis has neither of these, and inventing one would be a lie the panel
        // would render as a wait graph.
        assert!(s.user.is_none());
        assert!(s.blocked_by.is_empty());
    }

    #[test]
    fn reds_own_redis_connection_is_marked_and_no_other_is() {
        assert!(from_client(&client(7, 1), Some(7)).is_self);
        assert!(!from_client(&client(8, 1), Some(7)).is_self);
        // `CLIENT ID` refused: nothing is claimed to be ours, so the panel
        // offers the kill behind its confirm rather than hiding every row's.
        assert!(!from_client(&client(7, 1), None).is_self);
    }

    #[test]
    fn empty_redis_fields_become_none_rather_than_empty_strings() {
        let mut c = client(1, 0);
        c.name.clear();
        c.addr.clear();
        c.cmd.clear();
        let s = from_client(&c, None);
        assert!(s.application.is_none());
        assert!(s.client_addr.is_none());
        assert!(s.query.is_none());
    }

    #[test]
    fn a_mongo_op_maps_onto_the_shared_session_shape() {
        let s = from_doc_op(&op(1234, 12.5));
        assert_eq!(s.key, SessionKey("1234".into()));
        assert_eq!(s.database.as_deref(), Some("shop.orders"));
        assert_eq!(s.state, "getmore");
        assert_eq!(s.client_addr.as_deref(), Some("10.0.0.9:40100"));
        assert_eq!(s.elapsed_secs, 12.5);
        assert!(s.wait.is_none());
    }

    #[test]
    fn a_lock_wait_becomes_a_reason_not_a_blocker_edge() {
        // Mongo reports the fact, not who holds the lock. Filling `blocked_by`
        // with a guess would draw a wait graph the engine never gave us.
        let mut o = op(1, 1.0);
        o.waiting_for_lock = true;
        let s = from_doc_op(&o);
        assert_eq!(s.wait.as_deref(), Some("waiting for a lock"));
        assert!(s.blocked_by.is_empty());
    }

    #[test]
    fn the_listing_caps_by_dropping_the_shortest_running() {
        let sessions: Vec<_> = (0..MAX_SESSIONS + 10)
            .map(|i| from_client(&client(i as i64, i as u64), None))
            .collect();
        let (capped, over) = cap(sessions);
        assert!(over);
        assert_eq!(capped.len(), MAX_SESSIONS);
        // Longest-running first, so what survives the cut is what matters.
        assert_eq!(capped[0].elapsed_secs, (MAX_SESSIONS + 9) as f64);
    }

    #[test]
    fn a_listing_under_the_cap_is_not_reported_as_truncated() {
        let (capped, over) = cap(vec![from_client(&client(1, 5), None)]);
        assert!(!over);
        assert_eq!(capped.len(), 1);
    }
}
