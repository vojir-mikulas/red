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
