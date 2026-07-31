//! The RedisJSON inspector's state and actions: the lazy document tree, its raw
//! view, and the per-node edits. Rendering lives in [`render`].
//!
//! The whole module exists to keep one promise: a node is only read when the
//! user opens it. [`JsonTreeState::nodes`] is a map of levels, not a document,
//! and the flattening in [`AppState::json_rows`] walks only what has been read,
//! so opening the root of a 200 MB document and opening the root of a small one
//! cost the same. State types come from the parent module (`use super::*`).

mod render;

use gpui::Context;
use red_core::kv::{
    JSON_NODE_WINDOW, JsonDoc, JsonKind, JsonNode, JsonNodeView, JsonPath, JsonSeg, KvEdit,
};
use red_service::{Command, SessionId};

use crate::app::AppState;

use super::*;

/// One rendered row of the tree: how deep it sits, the node it addresses, and
/// the summary it draws. Built per frame from the levels that have been read.
pub(crate) struct JsonRow {
    pub(crate) depth: usize,
    pub(crate) path: JsonPath,
    /// The row's label: a member name, or `[i]` for an array element. Empty at
    /// the root, which renders as `$`.
    pub(crate) label: String,
    pub(crate) kind: JsonKind,
    /// A scalar's value, or a container/large-string's size.
    pub(crate) detail: String,
    pub(crate) expandable: bool,
    pub(crate) expanded: bool,
    pub(crate) loading: bool,
    /// This row is the "load more" affordance for a windowed container rather
    /// than a node of its own, so a large array pages in place at any depth
    /// rather than only at the root.
    pub(crate) more: bool,
}

impl JsonTreeState {
    /// The starting state for a freshly-read document.
    ///
    /// A lazily-walked document arrives with its root level already read, so the
    /// tree can draw immediately; a whole-loaded one arrives as text, which the
    /// raw view shows without a further round trip (the tree reads its root only
    /// if the user asks for it).
    pub(crate) fn from_doc(doc: &JsonDoc) -> JsonTreeState {
        let mut state = JsonTreeState::default();
        match doc {
            JsonDoc::Loaded { text, .. } => {
                state.raw = true;
                state.raw_text = Some(red_core::Value::Text(text.clone().into()));
            }
            JsonDoc::Lazy { root, .. } => {
                state.nodes.insert(JsonPath::root(), root.clone());
                state.expanded.insert(JsonPath::root());
            }
        }
        state
    }
}

impl AppState {
    /// Whether a JSON node edit currently owns the shared value editor.
    pub(crate) fn kv_json_editing(&mut self, session: SessionId) -> bool {
        self.json_state(session)
            .is_some_and(|j| j.editing.is_some())
    }

    /// Flatten the levels read so far into the visible rows, depth-first through
    /// the expanded paths. Never touches an unread level, so this is the render
    /// half of the lazy walk.
    pub(crate) fn json_rows(&self, json: &JsonTreeState) -> Vec<JsonRow> {
        let mut rows = Vec::new();
        // A document whose root is a scalar has no level to walk, so it is drawn
        // as the one row it is rather than as an empty tree.
        if let Some(JsonNodeView::Scalar { kind, value }) = json.nodes.get(&JsonPath::root()) {
            rows.push(JsonRow {
                depth: 0,
                path: JsonPath::root(),
                label: "$".into(),
                kind: *kind,
                detail: value.to_string(),
                expandable: false,
                expanded: false,
                loading: false,
                more: false,
            });
            return rows;
        }
        push_json_level(json, &JsonPath::root(), 0, &mut rows);
        rows
    }

    /// Open or close a node. Opening a level that has not been read yet sends
    /// the one request that reads it.
    pub(crate) fn kv_json_toggle(
        &mut self,
        session: SessionId,
        path: JsonPath,
        cx: &mut Context<Self>,
    ) {
        let Some((epoch, key, needs_fetch)) = self.with_json_mut(session, |json| {
            if json.expanded.remove(&path) {
                return false;
            }
            json.expanded.insert(path.clone());
            !json.nodes.contains_key(&path)
        }) else {
            return;
        };
        if needs_fetch {
            self.kv_json_request(session, epoch, key, path, 0, cx);
        }
        cx.notify();
    }

    /// Select a node: what the breadcrumb names and the actions apply to. Also
    /// refreshes the raw view, which follows the selection.
    pub(crate) fn kv_json_select(
        &mut self,
        session: SessionId,
        path: JsonPath,
        cx: &mut Context<Self>,
    ) {
        let raw = self
            .with_json_mut(session, |json| {
                json.selected = Some(path.clone());
                json.raw_text = None;
                json.editing = None;
                json.error = None;
                json.raw
            })
            .map(|(_, _, raw)| raw)
            .unwrap_or(false);
        if raw {
            self.kv_json_load_raw(session, cx);
        }
        cx.notify();
    }

    /// Page the next window of a large array node into the tree.
    pub(crate) fn kv_json_load_more(
        &mut self,
        session: SessionId,
        path: JsonPath,
        cx: &mut Context<Self>,
    ) {
        let Some((epoch, key, offset)) =
            self.with_json_mut(session, |json| match json.nodes.get(&path) {
                Some(JsonNodeView::Container {
                    offset, children, ..
                }) => Some(offset + children.len() as u64),
                _ => None,
            })
        else {
            return;
        };
        if let Some(offset) = offset {
            self.kv_json_request(session, epoch, key, path, offset, cx);
        }
    }

    /// Switch between the tree and the raw text of the selected node.
    pub(crate) fn kv_json_set_raw(
        &mut self,
        session: SessionId,
        raw: bool,
        cx: &mut Context<Self>,
    ) {
        let Some((epoch, key, needs)) = self.with_json_mut(session, |json| {
            json.raw = raw;
            json.editing = None;
            if raw {
                (json.raw_text.is_none(), false)
            } else {
                // A whole-loaded document has never read its root level (its
                // text was enough for the raw view); opening the tree is what
                // asks for it.
                let root_missing = !json.nodes.contains_key(&JsonPath::root())
                    && !json.loading.contains(&JsonPath::root());
                if root_missing {
                    json.expanded.insert(JsonPath::root());
                }
                (false, root_missing)
            }
        }) else {
            return;
        };
        match needs {
            (true, _) => self.kv_json_load_raw(session, cx),
            (_, true) => self.kv_json_request(session, epoch, key, JsonPath::root(), 0, cx),
            _ => {}
        }
        cx.notify();
    }

    /// Fetch the serialized JSON at the selected path, for the raw view and for
    /// seeding an edit.
    pub(crate) fn kv_json_load_raw(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = active.kv_view.as_mut().and_then(|v| v.active_browse_mut()) else {
            return;
        };
        let epoch = browse.epoch;
        let Some(inspector) = &mut browse.inspector else {
            return;
        };
        let key = inspector.key.clone();
        let path = inspector.json.selected.clone().unwrap_or_default();
        // A whole-loaded document already holds its own text; asking the server
        // for it again would be a round trip for something in hand.
        if path.is_root()
            && let Some(KvValue::Json(JsonDoc::Loaded { text, .. })) = &inspector.value
        {
            inspector.json.raw_text = Some(red_core::Value::Text(text.clone().into()));
            cx.notify();
            return;
        }
        inspector.json.raw_text = None;
        self.service
            .send_to(session, Command::KvReadJsonText { epoch, key, path });
        cx.notify();
    }

    /// Copy the selected node's JSONPath, the thing a user pastes into a script.
    pub(crate) fn kv_json_copy_path(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let path = self
            .json_state(session)
            .map(|j| j.selected.clone().unwrap_or_default().expr());
        if let Some(path) = path {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(path));
        }
    }

    /// Copy the raw JSON currently shown for the selected node.
    pub(crate) fn kv_json_copy_value(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let text = self
            .json_state(session)
            .and_then(|j| j.raw_text.as_ref().map(json_text));
        if let Some(text) = text {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        }
    }

    /// Open the editor on the selected node's raw JSON.
    pub(crate) fn kv_json_start_edit(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = active.kv_view.as_mut().and_then(|v| v.active_browse_mut()) else {
            return;
        };
        let Some(inspector) = &mut browse.inspector else {
            return;
        };
        // Only an uncapped body is editable: saving a truncated head back would
        // replace the node with its own prefix (the same rule strings follow).
        let Some(text) = inspector.json.raw_text.as_ref().and_then(|v| match v {
            red_core::Value::Text(s) => Some(s.to_string()),
            _ => None,
        }) else {
            return;
        };
        let path = inspector.json.selected.clone().unwrap_or_default();
        inspector.json.editing = Some(path);
        inspector.json.error = None;
        inspector
            .value_editor
            .update(cx, |ed, cx| ed.set_content(text, cx));
        cx.notify();
    }

    pub(crate) fn kv_json_cancel_edit(&mut self, session: SessionId, cx: &mut Context<Self>) {
        self.with_json_mut(session, |json| {
            json.editing = None;
            json.error = None;
        });
        cx.notify();
    }

    /// Validate the edited text and send the `JSON.SET`. A malformed document
    /// fails here, naming the offset, rather than at the server as a bare
    /// `ERR expected value`.
    pub(crate) fn kv_json_submit_edit(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = active.kv_view.as_mut().and_then(|v| v.active_browse_mut()) else {
            return;
        };
        let epoch = browse.epoch;
        let Some(inspector) = &mut browse.inspector else {
            return;
        };
        let Some(path) = inspector.json.editing.clone() else {
            return;
        };
        let key = inspector.key.clone();
        let value = inspector.value_editor.read(cx).content();
        if let Err(e) = red_core::kv::validate_json(&value) {
            inspector.json.error = Some(e.to_string());
            cx.notify();
            return;
        }
        let edit = KvEdit::JsonSet { key, path, value };
        self.service
            .send_to(session, Command::KvApplyEdit { epoch, edit });
    }

    /// Delete the selected node (`JSON.DEL`). Rides the same confirm policy as
    /// every other destructive Redis action; at the root it removes the key, so
    /// that case routes through the key-delete confirm instead.
    pub(crate) fn kv_json_delete_node(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(path) = self
            .json_state(session)
            .and_then(|j| j.selected.clone())
            .filter(|p| !p.is_root())
        else {
            // Deleting `$` is deleting the key; say so by using that path, which
            // already has its own confirm and its own recycle-bin undo.
            self.kv_request_delete(session, cx);
            return;
        };
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = active.kv_view.as_mut().and_then(|v| v.active_browse_mut()) else {
            return;
        };
        let epoch = browse.epoch;
        let Some(inspector) = &browse.inspector else {
            return;
        };
        let edit = KvEdit::JsonDelete {
            key: inspector.key.clone(),
            path,
        };
        self.service
            .send_to(session, Command::KvApplyEdit { epoch, edit });
    }

    /// Ask the service for one node's level.
    fn kv_json_request(
        &mut self,
        session: SessionId,
        epoch: red_service::Epoch,
        key: String,
        path: JsonPath,
        offset: u64,
        cx: &mut Context<Self>,
    ) {
        if let Some(active) = self.conn_mut(Some(session))
            && let Some(browse) = active.kv_view.as_mut().and_then(|v| v.active_browse_mut())
            && let Some(inspector) = &mut browse.inspector
        {
            inspector.json.loading.insert(path.clone());
        }
        self.service.send_to(
            session,
            Command::KvReadJsonNode {
                epoch,
                key,
                path,
                offset,
                count: JSON_NODE_WINDOW,
            },
        );
        cx.notify();
    }

    /// `Event::KvJsonNodeReady`: fold one level into the tree. A window past the
    /// first appends to the node already there, so paging a large array grows
    /// the level rather than replacing it.
    pub(crate) fn on_kv_json_node_ready(
        &mut self,
        session: Option<SessionId>,
        key: String,
        path: JsonPath,
        view: Option<JsonNodeView>,
        cx: &mut Context<Self>,
    ) {
        let Some(active) = self.conn_mut(session) else {
            return;
        };
        let Some(inspector) = active
            .kv_view
            .as_mut()
            .and_then(|v| v.active_browse_mut())
            .and_then(|b| b.inspector.as_mut())
            .filter(|i| i.key == key)
        else {
            return;
        };
        inspector.json.loading.remove(&path);
        match view {
            None => {
                inspector.json.nodes.remove(&path);
                inspector.json.expanded.remove(&path);
            }
            Some(fresh) => {
                let merged = match (inspector.json.nodes.remove(&path), fresh) {
                    (
                        Some(JsonNodeView::Container {
                            kind,
                            offset,
                            children: mut have,
                            ..
                        }),
                        JsonNodeView::Container {
                            len,
                            offset: next_offset,
                            children: more,
                            ..
                        },
                    ) if next_offset == offset + have.len() as u64 => {
                        have.extend(more);
                        JsonNodeView::Container {
                            kind,
                            len,
                            offset,
                            children: have,
                        }
                    }
                    (_, fresh) => fresh,
                };
                inspector.json.nodes.insert(path, merged);
            }
        }
        cx.notify();
    }

    /// `Event::KvJsonTextReady`: the raw view's body.
    pub(crate) fn on_kv_json_text_ready(
        &mut self,
        session: Option<SessionId>,
        key: String,
        path: JsonPath,
        text: Option<red_core::Value>,
        cx: &mut Context<Self>,
    ) {
        let Some(active) = self.conn_mut(session) else {
            return;
        };
        let Some(inspector) = active
            .kv_view
            .as_mut()
            .and_then(|v| v.active_browse_mut())
            .and_then(|b| b.inspector.as_mut())
            .filter(|i| i.key == key)
        else {
            return;
        };
        // Drop a reply for a node the selection has since moved off.
        if inspector.json.selected.clone().unwrap_or_default() != path {
            return;
        }
        inspector.json.raw_text = text;
        cx.notify();
    }

    /// The active browse tab's JSON state, for a read.
    fn json_state(&mut self, session: SessionId) -> Option<&JsonTreeState> {
        self.conn_mut(Some(session))?
            .kv_view
            .as_ref()?
            .active_browse()?
            .inspector
            .as_ref()
            .map(|i| &i.json)
    }

    /// Run `f` over the active browse tab's JSON state, handing back the tab's
    /// epoch and key alongside the result so a caller can issue a request
    /// without re-walking the same four `Option`s.
    fn with_json_mut<T>(
        &mut self,
        session: SessionId,
        f: impl FnOnce(&mut JsonTreeState) -> T,
    ) -> Option<(red_service::Epoch, String, T)> {
        let browse = self
            .conn_mut(Some(session))?
            .kv_view
            .as_mut()?
            .active_browse_mut()?;
        let epoch = browse.epoch;
        let inspector = browse.inspector.as_mut()?;
        let key = inspector.key.clone();
        Some((epoch, key, f(&mut inspector.json)))
    }
}

/// Append `path`'s children (and, recursively, any expanded ones) to `rows`.
fn push_json_level(json: &JsonTreeState, path: &JsonPath, depth: usize, rows: &mut Vec<JsonRow>) {
    let Some(JsonNodeView::Container { len, children, .. }) = json.nodes.get(path) else {
        return;
    };
    for child in children {
        let child_path = path.child(&child.seg);
        let expandable = child.kind.is_container();
        let expanded = expandable && json.expanded.contains(&child_path);
        rows.push(JsonRow {
            depth,
            label: child.seg.to_string(),
            kind: child.kind,
            detail: json_child_detail(child),
            path: child_path.clone(),
            expandable,
            expanded,
            loading: json.loading.contains(&child_path),
            more: false,
        });
        if expanded {
            push_json_level(json, &child_path, depth + 1, rows);
        }
    }
    // A windowed level is honest about it: say how much is loaded and offer the
    // rest, exactly like the big-list preview.
    if (children.len() as u64) < *len {
        rows.push(JsonRow {
            depth,
            path: path.clone(),
            label: format!("Load more ({} of {len})", children.len()),
            kind: JsonKind::Array,
            detail: String::new(),
            expandable: false,
            expanded: false,
            loading: json.loading.contains(path),
            more: true,
        });
    }
}

/// A child row's right-hand summary: its value when small enough to inline, its
/// size otherwise.
fn json_child_detail(node: &JsonNode) -> String {
    match (&node.preview, node.len) {
        (Some(v), _) => v.clone(),
        (None, Some(n)) if node.kind == JsonKind::String => format!("{n} chars"),
        (None, Some(n)) => format!("{n} items"),
        (None, None) => node.kind.label().to_string(),
    }
}

/// The breadcrumb text for a path: `$ › orders › [3] › lines`.
pub(crate) fn json_breadcrumb(path: &JsonPath) -> String {
    let mut out = String::from("$");
    for seg in path.segments() {
        out.push_str(" \u{203a} ");
        match seg {
            JsonSeg::Member(name) => out.push_str(name),
            JsonSeg::Index(i) => out.push_str(&format!("[{i}]")),
        }
    }
    out
}

/// A fetched JSON body as text, whether it arrived whole or capped.
pub(crate) fn json_text(value: &red_core::Value) -> String {
    match value {
        red_core::Value::Text(s) => s.to_string(),
        red_core::Value::Capped(c) => c.head.clone(),
        other => format!("{other:?}"),
    }
}
