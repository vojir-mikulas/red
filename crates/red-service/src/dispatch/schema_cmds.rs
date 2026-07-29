//! Schema-introspection command handlers, extracted from the dispatch loop
//! (guidelines D): the read-only `LoadObjects`/`LoadForeignKeys`/`LoadEnums`/
//! `DescribeTable` arms. Each borrows the (immutable) session map plus the routing
//! session id and emits its reply; a guard failure just returns — the loop's
//! `continue` becomes an early `return` here. No session mutation, no spawns.

use std::collections::HashMap;

use red_core::TableRef;

use crate::{Event, SessionId};

use super::session::SessionState;
use super::{Events, emit};

/// `LoadObjects`: list the connection's schemas/objects for the tree.
pub(super) async fn load_objects(
    sessions: &HashMap<SessionId, SessionState>,
    session_id: Option<SessionId>,
    events: &Events,
) {
    let Some(id) = session_id else { return };
    let Some(state) = sessions.get(&id) else {
        emit(events, session_id, Event::Error("not connected".into()));
        return;
    };
    let Some(driver) = state.driver.as_sql().cloned() else {
        emit(
            events,
            session_id,
            Event::Error("not a SQL connection".into()),
        );
        return;
    };
    match driver.list_objects().await {
        Ok(schemas) => emit(events, session_id, Event::ObjectsLoaded { schemas }),
        Err(e) => emit(events, session_id, Event::Error(e.to_string())),
    }
}

/// `LoadObjectGroup`: one lazily-loaded object kind (routines, triggers,
/// sequences, types) for one namespace, sent when the user expands that group
/// node. Errors are swallowed the way the FK graph's are: a catalog this server
/// does not have (MariaDB sequences on MySQL, say) should leave the group empty,
/// not toast at someone who merely clicked a chevron.
pub(super) async fn load_object_group(
    sessions: &HashMap<SessionId, SessionState>,
    session_id: Option<SessionId>,
    events: &Events,
    namespace: String,
    kind: red_core::ObjectKind,
) {
    let Some(id) = session_id else { return };
    let Some(state) = sessions.get(&id) else {
        return;
    };
    let Some(driver) = state.driver.as_sql().cloned() else {
        return;
    };
    // Always reply, including with an empty list: the tree shows a spinner until
    // this lands, and a swallowed error would leave it spinning forever.
    let objects = driver
        .list_object_group(&namespace, kind)
        .await
        .unwrap_or_default();
    emit(
        events,
        session_id,
        Event::ObjectGroupLoaded {
            namespace,
            kind,
            objects,
        },
    );
}

/// `ObjectDdl`: one object's definition for the read-only DDL tab. Unlike the
/// group load, a failure here is reported: the user asked for exactly this
/// object's DDL, so "you cannot see it" is the answer, not silence.
pub(super) async fn object_ddl(
    sessions: &HashMap<SessionId, SessionState>,
    session_id: Option<SessionId>,
    events: &Events,
    epoch: crate::Epoch,
    namespace: String,
    name: String,
    kind: red_core::ObjectKind,
) {
    let Some(id) = session_id else { return };
    let Some(state) = sessions.get(&id) else {
        emit(
            events,
            session_id,
            Event::ObjectDdlFailed {
                epoch,
                message: "not connected".into(),
            },
        );
        return;
    };
    let Some(driver) = state.driver.as_sql().cloned() else {
        emit(
            events,
            session_id,
            Event::ObjectDdlFailed {
                epoch,
                message: "not a SQL connection".into(),
            },
        );
        return;
    };
    match driver.object_ddl(&namespace, &name, kind).await {
        Ok(ddl) => emit(
            events,
            session_id,
            Event::ObjectDdlReady {
                epoch,
                namespace,
                name,
                kind,
                ddl,
            },
        ),
        Err(e) => emit(
            events,
            session_id,
            Event::ObjectDdlFailed {
                epoch,
                message: e.to_string(),
            },
        ),
    }
}

/// `LoadForeignKeys`: the FK graph for click-through nav. Errors are swallowed —
/// FK navigation is optional, so a failed/unsupported introspection (including a
/// KV session with no SQL driver) leaves the graph empty rather than toasting.
pub(super) async fn load_foreign_keys(
    sessions: &HashMap<SessionId, SessionState>,
    session_id: Option<SessionId>,
    events: &Events,
) {
    let Some(id) = session_id else { return };
    let Some(state) = sessions.get(&id) else {
        return;
    };
    let Some(driver) = state.driver.as_sql().cloned() else {
        return;
    };
    if let Ok(graph) = driver.foreign_keys().await {
        emit(events, session_id, Event::ForeignKeysLoaded { graph });
    }
}

/// `LoadEnums`: a table's enum-typed columns for the in-cell picker. Optional like
/// the FK graph: a failed/unsupported lookup just leaves the picker without enum
/// suggestions rather than toasting.
pub(super) async fn load_enums(
    sessions: &HashMap<SessionId, SessionState>,
    session_id: Option<SessionId>,
    events: &Events,
    table: TableRef,
) {
    let Some(id) = session_id else { return };
    let Some(state) = sessions.get(&id) else {
        return;
    };
    let Some(driver) = state.driver.as_sql().cloned() else {
        return;
    };
    if let Ok(columns) = driver.enum_columns(&table).await {
        emit(events, session_id, Event::EnumsLoaded { table, columns });
    }
}

/// `DescribeTable`: a table's full detail (columns, keys, indexes) for the schema
/// panel and the keyset/FK plumbing.
pub(super) async fn describe_table(
    sessions: &HashMap<SessionId, SessionState>,
    session_id: Option<SessionId>,
    events: &Events,
    schema: String,
    table: String,
) {
    let Some(id) = session_id else { return };
    let Some(state) = sessions.get(&id) else {
        emit(events, session_id, Event::Error("not connected".into()));
        return;
    };
    let Some(driver) = state.driver.as_sql().cloned() else {
        emit(
            events,
            session_id,
            Event::Error("not a SQL connection".into()),
        );
        return;
    };
    match driver.describe_table(&schema, &table).await {
        Ok(detail) => emit(
            events,
            session_id,
            Event::TableDescribed {
                schema,
                table,
                detail,
            },
        ),
        Err(e) => emit(events, session_id, Event::Error(e.to_string())),
    }
}

/// `ListServerSessions`: what the server is doing right now.
///
/// Errors are reported rather than swallowed: unlike the FK graph, this panel's
/// whole content is this call, so a failure has to be visible or the panel just
/// looks empty.
pub(super) async fn list_server_sessions(
    sessions: &HashMap<SessionId, SessionState>,
    session_id: Option<SessionId>,
    events: &Events,
) {
    let Some(id) = session_id else { return };
    let Some(state) = sessions.get(&id) else {
        emit(events, session_id, Event::Error("not connected".into()));
        return;
    };
    let Some(driver) = state.driver.as_sql().cloned() else {
        return;
    };
    match driver.server_sessions().await {
        Ok((sessions, restricted)) => emit(
            events,
            session_id,
            Event::ServerSessionsReady {
                sessions,
                restricted,
            },
        ),
        Err(e) => emit(events, session_id, Event::Error(e.to_string())),
    }
}

/// `KillServerSession`: stop one session's statement, or the session itself.
///
/// The read-only gate is enforced here as well as in the driver. The UI grades
/// the confirmation and the driver refuses a read-only connection; this is the
/// third rail, and it is deliberate for a command whose blast radius is someone
/// else's transaction.
pub(super) async fn kill_server_session(
    sessions: &HashMap<SessionId, SessionState>,
    session_id: Option<SessionId>,
    events: &Events,
    key: red_core::SessionKey,
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
    let Some(driver) = state.driver.as_sql().cloned() else {
        return;
    };
    match driver.kill_session(&key, mode).await {
        Ok(()) => emit(events, session_id, Event::ServerSessionKilled { key, mode }),
        Err(e) => emit(events, session_id, Event::Error(e.to_string())),
    }
}

/// `BuildHealthReport`: the connection's health snapshot.
///
/// The namespace in force rides along, so a report on a MySQL connection bound to
/// one database covers that database rather than the whole server.
pub(super) async fn build_health_report(
    sessions: &HashMap<SessionId, SessionState>,
    session_id: Option<SessionId>,
    events: &Events,
) {
    let Some(id) = session_id else { return };
    let Some(state) = sessions.get(&id) else {
        emit(
            events,
            session_id,
            Event::HealthReportFailed {
                message: "not connected".into(),
            },
        );
        return;
    };
    let Some(driver) = state.driver.as_sql().cloned() else {
        emit(
            events,
            session_id,
            Event::HealthReportFailed {
                message: "not a SQL connection".into(),
            },
        );
        return;
    };
    match driver.health(None).await {
        Ok(report) => emit(events, session_id, Event::HealthReportReady { report }),
        Err(e) => emit(
            events,
            session_id,
            Event::HealthReportFailed {
                message: e.to_string(),
            },
        ),
    }
}

/// Build one side's snapshot: the namespace's objects plus each relation's
/// detail.
///
/// One `describe_table` per relation, awaited in order rather than fanned out: a
/// schema comparison is a background job, and a hundred concurrent describes
/// against a production catalog is the kind of thundering herd this codebase
/// avoids elsewhere. `abort` is checked per object so cancelling is prompt.
async fn snapshot(
    driver: &std::sync::Arc<dyn red_driver::DatabaseDriver>,
    engine: red_core::DbKind,
    namespace: &str,
    abort: &std::sync::atomic::AtomicBool,
) -> Result<red_core::schema_diff::SchemaSnapshot, red_core::RedError> {
    use std::sync::atomic::Ordering;

    let schemas = driver.list_objects().await?;
    let meta = schemas
        .iter()
        .find(|s| s.name == namespace)
        .ok_or_else(|| red_core::RedError::Query(format!("no schema named {namespace}")))?;
    let mut snap = red_core::schema_diff::SchemaSnapshot::from_meta(engine, meta);
    for obj in &meta.objects {
        if abort.load(Ordering::Relaxed) {
            return Err(red_core::RedError::Interrupted);
        }
        if !obj.kind.is_relation() {
            continue;
        }
        // A single unreadable object must not fail the whole comparison; it is
        // compared by existence instead, which is still useful.
        if let Ok(detail) = driver.describe_table(namespace, &obj.name).await {
            snap.details.insert(obj.name.clone(), detail);
        }
    }
    Ok(snap)
}

/// `DiffSchemas`: compare two namespaces structurally.
pub(super) async fn diff_schemas(
    sessions: &HashMap<SessionId, SessionState>,
    session_id: Option<SessionId>,
    events: &Events,
    id: crate::OpId,
    left_namespace: String,
    right_session: SessionId,
    right_namespace: String,
) {
    let fail = |message: String| {
        emit(events, session_id, Event::SchemaDiffFailed { id, message });
    };
    let Some(left_sid) = session_id else { return };
    let (Some(left_state), Some(right_state)) =
        (sessions.get(&left_sid), sessions.get(&right_session))
    else {
        fail("one of the connections isn't open".into());
        return;
    };
    let (Some(left_driver), Some(right_driver)) = (
        left_state.driver.as_sql().cloned(),
        right_state.driver.as_sql().cloned(),
    ) else {
        fail("both sides must be SQL connections".into());
        return;
    };
    let (left_kind, right_kind) = (left_state.kind, right_state.kind);

    // No abort plumbing yet: the comparison is catalog reads, bounded by the
    // schema's object count, and the UI has no cancel affordance for it. The flag
    // is threaded through `snapshot` so adding one is a wiring change, not a
    // rewrite.
    let abort = std::sync::atomic::AtomicBool::new(false);
    let left = match snapshot(&left_driver, left_kind, &left_namespace, &abort).await {
        Ok(s) => s,
        Err(e) => return fail(format!("left side: {e}")),
    };
    let right = match snapshot(&right_driver, right_kind, &right_namespace, &abort).await {
        Ok(s) => s,
        Err(e) => return fail(format!("right side: {e}")),
    };
    let delta = red_core::schema_diff::compare(&left, &right);
    emit(
        events,
        session_id,
        Event::SchemaDiffFinished {
            id,
            left: left_namespace,
            right: right_namespace,
            delta,
        },
    );
}

/// `LoadObjectCounts`: how many routines/triggers/sequences/types each namespace
/// holds, in one query.
///
/// Silent on failure, like the FK graph: an engine that cannot answer leaves the
/// tree on its click-to-discover path, which still works. Emitting an error
/// event would toast at someone who merely connected.
pub(super) async fn load_object_counts(
    sessions: &HashMap<SessionId, SessionState>,
    session_id: Option<SessionId>,
    events: &Events,
) {
    let Some(id) = session_id else { return };
    let Some(state) = sessions.get(&id) else {
        return;
    };
    let Some(driver) = state.driver.as_sql().cloned() else {
        return;
    };
    if let Ok(counts) = driver.object_group_counts().await {
        emit(events, session_id, Event::ObjectCountsReady { counts });
    }
}
