//! The Redis key inspector and key-lifecycle operations, split out of
//! `kvbrowse/mod.rs` (guidelines D): opening/closing the value inspector, the
//! value/TTL/rename/collection/stream edit flows, create-key, recent keys, the
//! recycle-bin undo, and the inspector's paged collection/list/stream reads.
//! A second `impl AppState` block; the state types + free helpers it reads live on
//! the parent module (`use super::*`).

use std::rc::Rc;
use std::time::Duration;

use flint::prelude::*;
use gpui::{Context, Entity, UniformListScrollHandle, prelude::*, px};
use red_core::kv::{
    CollectionKind, KvCollection, KvCollectionPage, KvElement, KvStreamActionReq, KvStreamPage,
    KvType, KvValue, PendingEntry, RecycledKey, StreamAction, StreamConsumer, StreamGroup,
};
use red_service::{Command, SessionId};

use crate::app::{
    AppState, Notification, NotificationAction, RECYCLE_BIN_CAP, RecycleBatch, TabWorkspace,
};

use super::*;

impl AppState {
    pub(crate) fn on_kv_db_size(
        &mut self,
        session: Option<SessionId>,
        epoch: red_service::Epoch,
        count: u64,
        cx: &mut Context<Self>,
    ) {
        // `DBSIZE` is connection-level: store it on the view (shared by every
        // Browse tab), matched against the browse tab that requested it.
        let Some(view) = self.conn_mut(session).and_then(|a| a.kv_view.as_mut()) else {
            return;
        };
        if view.browse_by_scan_epoch_mut(epoch).is_none() {
            return;
        }
        view.db_size = Some(count);
        cx.notify();
    }

    /// A keyspace row was selected: open the inspector on it and kick off
    /// `KvReadValue`. Replaces whatever the inspector was showing before.
    /// Open the inspector on `key` (called with the resolved `KeyMeta`
    /// fields rather than a row index, so both the live browse table and
    /// the biggest-keys sample's table — two different backing `Vec`s — can
    /// open the same inspector without this needing to know which list a
    /// selection came from).
    pub(crate) fn kv_open_inspector(
        &mut self,
        session: SessionId,
        key: String,
        ttl: Option<Duration>,
        kv_type: KvType,
        cx: &mut Context<Self>,
    ) {
        let Some(browse) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_ref())
            .and_then(|v| v.active_browse())
        else {
            return;
        };
        let epoch = browse.epoch;

        // Record this key in the connection's recently-viewed list (newest-first,
        // deduped, capped) — the History dock's Keys section reads it.
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
        {
            view.recent_keys.retain(|r| r.key != key);
            view.recent_keys.insert(
                0,
                RecentKey {
                    key: key.clone(),
                    kv_type: kv_type.clone(),
                    ttl,
                    viewed_unix: crate::conversations::now_unix(),
                },
            );
            view.recent_keys.truncate(MAX_RECENT_KEYS);
        }
        self.kv_persist_recent_keys(session);

        // A multiline surface (no gutter, no frame of its own) so it reads as
        // the value body becoming editable in place, exactly like the SQL cell
        // inspector's inline editor. ⌘↵ (Run) saves; Esc cancels. Enter inserts
        // a newline, so multi-line JSON stays editable.
        let value_editor = cx.new(|cx| {
            CodeEditor::new(cx)
                .gutter(false)
                .resting_border(false)
                .corner_radius(px(0.))
                .soft_wrap(true)
                .a11y_label("Key value editor")
        });
        cx.subscribe(
            &value_editor,
            move |this, _, event: &CodeEditorEvent, cx| match event {
                CodeEditorEvent::Run => this.kv_submit_value_edit(session, cx),
                CodeEditorEvent::Escape => this.kv_cancel_editing_value(session, cx),
                _ => {}
            },
        )
        .detach();
        let ttl_editor =
            cx.new(|cx| TextInput::new(cx).with_placeholder("seconds, blank = no expiry"));
        cx.subscribe(&ttl_editor, move |this, _, event: &TextInputEvent, cx| {
            if matches!(event, TextInputEvent::Submit) {
                this.kv_submit_ttl_edit(session, cx);
            }
        })
        .detach();
        let rename_editor = cx.new(TextInput::new);
        cx.subscribe(
            &rename_editor,
            move |this, _, event: &TextInputEvent, cx| {
                if matches!(event, TextInputEvent::Submit) {
                    this.kv_submit_rename(session, cx);
                }
            },
        )
        .detach();
        let claim_editor = cx.new(|cx| TextInput::new(cx).with_placeholder("claim to consumer…"));
        cx.subscribe(&claim_editor, move |this, _, event: &TextInputEvent, cx| {
            if matches!(event, TextInputEvent::Submit) {
                this.kv_submit_claim(session, cx);
            }
        })
        .detach();
        let elem_name_editor = cx.new(TextInput::new);
        cx.subscribe(
            &elem_name_editor,
            move |this, _, event: &TextInputEvent, cx| {
                if matches!(event, TextInputEvent::Submit) {
                    this.kv_submit_collection_edit(session, cx);
                }
            },
        )
        .detach();
        let elem_value_editor = cx.new(TextInput::new);
        cx.subscribe(
            &elem_value_editor,
            move |this, _, event: &TextInputEvent, cx| {
                if matches!(event, TextInputEvent::Submit) {
                    this.kv_submit_collection_edit(session, cx);
                }
            },
        )
        .detach();

        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = active.kv_view.as_mut().and_then(|v| v.active_browse_mut()) else {
            return;
        };
        browse.inspector = Some(KvInspector {
            key: key.clone(),
            kv_type,
            ttl,
            value: None,
            collection_rows: Rc::new(Vec::new()),
            collection_cursor: 0,
            collection_exhausted: false,
            collection_head_only: false,
            collection_loading: false,
            collection_scroll: UniformListScrollHandle::new(),
            stream_rows: Rc::new(Vec::new()),
            stream_before: None,
            stream_exhausted: false,
            stream_loading: false,
            stream_scroll: UniformListScrollHandle::new(),
            stream_groups: StreamGroupsState {
                view: StreamView::Entries,
                loaded: false,
                loading: false,
                groups: Vec::new(),
                selected: None,
                consumers: Vec::new(),
                pending: Vec::new(),
                detail_loading: false,
                claiming: None,
                claim_editor,
            },
            value_editor,
            editing_value: false,
            str_preview: None,
            ttl_editor,
            editing_ttl: false,
            rename_editor,
            editing_key: false,
            confirm_delete: false,
            str_format: crate::inspector::ValueFormat::Auto,
            loading_full_value: false,
            edit_after_load: false,
            elem_name_editor,
            elem_value_editor,
            collection_edit: None,
            elem_error: None,
            value_loaded: false,
            value_error: None,
            ttl_error: None,
        });
        self.service
            .send_to(session, Command::KvReadValue { epoch, key });
        cx.notify();
    }

    /// Open a recently-viewed key (the History dock's Keys section): make sure
    /// the focused half shows a Browse tab, then open the inspector on it.
    pub(crate) fn kv_open_recent_key(
        &mut self,
        session: SessionId,
        key: String,
        kv_type: KvType,
        ttl: Option<Duration>,
        cx: &mut Context<Self>,
    ) {
        let is_browse = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_ref())
            .is_some_and(|v| matches!(v.active_state(), Some(RedisTabState::Browse(_))));
        if !is_browse {
            self.kv_new_empty_tab(session, cx);
            let id = self
                .conn_mut(Some(session))
                .and_then(|a| a.kv_view.as_ref())
                .and_then(|v| v.tabs.get(v.focused_tab_index()))
                .map(|t| t.id);
            if let Some(id) = id {
                self.kv_set_tab_kind(session, id, KvPanel::Browse, cx);
            }
        }
        self.kv_open_inspector(session, key, ttl, kv_type, cx);
    }

    /// Toggle the History dock's "Recently viewed keys" section collapsed/open
    /// (in-memory, reset per session).
    pub(crate) fn kv_toggle_recent_keys(&mut self, session: SessionId, cx: &mut Context<Self>) {
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
        {
            view.recent_keys_collapsed = !view.recent_keys_collapsed;
            cx.notify();
        }
    }

    /// Toggle the History dock's "Commands" section collapsed/open (in-memory).
    pub(crate) fn kv_toggle_commands(&mut self, session: SessionId, cx: &mut Context<Self>) {
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
        {
            view.commands_collapsed = !view.commands_collapsed;
            cx.notify();
        }
    }

    /// Clear the connection's recently-viewed keys (the History dock's trash).
    pub(crate) fn kv_clear_recent_keys(&mut self, session: SessionId, cx: &mut Context<Self>) {
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            && !view.recent_keys.is_empty()
        {
            view.recent_keys.clear();
            self.kv_persist_recent_keys(session);
            cx.notify();
        }
    }

    /// Seed a freshly-connected Redis view's recently-viewed list from the
    /// persisted store, so browsing history survives a restart.
    pub(crate) fn kv_seed_recent_keys(&mut self, session: SessionId, conn_id: &str) {
        let seeded: Vec<RecentKey> = self
            .redis_recent_keys
            .get(conn_id)
            .map(|recs| recs.iter().map(RecentKey::from_rec).collect())
            .unwrap_or_default();
        if seeded.is_empty() {
            return;
        }
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
        {
            view.recent_keys = seeded;
        }
    }

    /// Write the connection's current recently-viewed list to the persisted
    /// store (called after any change: record / clear / remove).
    fn kv_persist_recent_keys(&mut self, session: SessionId) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let conn_id = active.conn_id.clone();
        if conn_id.is_empty() {
            return;
        }
        let recs: Vec<crate::recent_keys::RecentKeyRec> = active
            .kv_view
            .as_ref()
            .map(|v| v.recent_keys.iter().map(RecentKey::to_rec).collect())
            .unwrap_or_default();
        self.redis_recent_keys.set(&conn_id, recs);
    }

    /// Drop a single recently-viewed key from the History dock's Keys section
    /// (the per-row remove button), leaving the rest of the list intact.
    pub(crate) fn kv_remove_recent_key(
        &mut self,
        session: SessionId,
        key: String,
        cx: &mut Context<Self>,
    ) {
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
        {
            let before = view.recent_keys.len();
            view.recent_keys.retain(|r| r.key != key);
            if view.recent_keys.len() != before {
                self.kv_persist_recent_keys(session);
                cx.notify();
            }
        }
    }

    /// Change the string inspector's display lens (Auto/Raw/JSON/Hex or a
    /// binary decoder).
    pub(crate) fn kv_set_str_format(
        &mut self,
        session: SessionId,
        fmt: crate::inspector::ValueFormat,
        cx: &mut Context<Self>,
    ) {
        let Some(inspector) = self.kv_inspector_mut(session) else {
            return;
        };
        inspector.str_format = fmt;
        // The preview renders through the lens, so rebuild it under the new one.
        self.kv_rebuild_str_preview(session, cx);
        cx.notify();
    }

    /// "Load full value": re-fetch the inspector's string key in full (a plain
    /// `GET`, no cap), for a value `read_value` returned as a `Value::Capped`.
    /// The reply comes back on `Event::KvValueReady` and replaces the capped
    /// body in place, mirroring the SQL cell inspector's load-full flow.
    /// Copy the inspected string value to the clipboard. Copies whatever's
    /// resident — the full text, or a capped value's loaded head (use "Load
    /// full value" first to copy the whole thing).
    pub(crate) fn kv_copy_string_value(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let text = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_ref())
            .and_then(|v| v.active_browse())
            .and_then(|b| b.inspector.as_ref())
            .and_then(|i| match i.value.as_ref()? {
                KvValue::Str(red_core::Value::Text(s)) => Some(s.to_string()),
                KvValue::Str(red_core::Value::Capped(c)) => Some(c.head.clone()),
                KvValue::Str(other) => Some(format!("{other:?}")),
                _ => None,
            });
        if let Some(text) = text {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        }
    }

    pub(crate) fn kv_load_full_value(&mut self, session: SessionId, cx: &mut Context<Self>) {
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
        if inspector.loading_full_value {
            return;
        }
        inspector.loading_full_value = true;
        let key = inspector.key.clone();
        self.service
            .send_to(session, Command::KvReadStringFull { epoch, key });
        cx.notify();
    }

    /// Build (or drop) the read-only, selectable preview editor for the
    /// inspector's string value, keyed off the current value + lens. Called
    /// whenever either changes — not per frame — so an in-progress selection
    /// and scroll survive. A non-string value, a still-loading value, or an
    /// open edit leaves `str_preview` empty (those render their own body).
    fn kv_rebuild_str_preview(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(inspector) = self.kv_inspector_mut(session) else {
            return;
        };
        let build = match (&inspector.value, inspector.editing_value) {
            (Some(KvValue::Str(v)), false) => Some((v.clone(), inspector.str_format)),
            _ => None,
        };
        let Some((value, fmt)) = build else {
            inspector.str_preview = None;
            return;
        };
        let (body, _summary, wrap) = crate::inspector::format_value_body(&value, fmt);
        let editor = cx.new(|cx| {
            let mut e = CodeEditor::new(cx)
                .gutter(false)
                .resting_border(false)
                .corner_radius(px(0.))
                .soft_wrap(wrap)
                .a11y_label("Key value")
                .with_content(body);
            e.set_read_only(true, cx);
            e
        });
        // Esc from the focused preview closes the inspector, matching Esc from
        // the keyspace grid (the editor swallows the key otherwise).
        let sub = cx.subscribe(&editor, move |this, _, event: &CodeEditorEvent, cx| {
            if matches!(event, CodeEditorEvent::Escape) {
                this.kv_close_inspector(session, cx);
            }
        });
        let Some(inspector) = self.kv_inspector_mut(session) else {
            return;
        };
        inspector.str_preview = Some(KvStrPreview { editor, sub });
    }

    pub(crate) fn kv_close_inspector(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = active.kv_view.as_mut().and_then(|v| v.active_browse_mut()) else {
            return;
        };
        browse.inspector = None;
        cx.notify();
    }

    // --- editing (see docs/plans/redis.md's editing phase) ---

    pub(crate) fn kv_start_editing_value(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = active.kv_view.as_mut().and_then(|v| v.active_browse_mut()) else {
            return;
        };
        let Some(inspector) = &mut browse.inspector else {
            return;
        };
        // A `read_value`-capped string only holds its head; editing must run on
        // the whole value or a save would truncate the key. Fetch the full
        // string first and defer opening the editor to `on_kv_value_ready`.
        if matches!(
            &inspector.value,
            Some(KvValue::Str(red_core::Value::Capped(_)))
        ) {
            inspector.edit_after_load = true;
            self.kv_load_full_value(session, cx);
            return;
        }
        let seed = match &inspector.value {
            Some(KvValue::Str(v)) => render_string_preview(v),
            _ => String::new(),
        };
        inspector
            .value_editor
            .update(cx, |ed, cx| ed.set_content(seed, cx));
        inspector.editing_value = true;
        // The editor owns the body while editing, so drop the read-only preview.
        inspector.str_preview = None;
        cx.notify();
    }

    pub(crate) fn kv_cancel_editing_value(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = active.kv_view.as_mut().and_then(|v| v.active_browse_mut()) else {
            return;
        };
        let Some(inspector) = &mut browse.inspector else {
            return;
        };
        inspector.editing_value = false;
        // Restore the selectable read-only preview now the editor is gone.
        self.kv_rebuild_str_preview(session, cx);
        cx.notify();
    }

    pub(crate) fn kv_submit_value_edit(&mut self, session: SessionId, cx: &mut Context<Self>) {
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
        let key = inspector.key.clone();
        // Preserve the key's existing TTL: `KEEPTTL` retains the server's actual
        // expiry exactly, so editing the value neither clears nor resets the
        // countdown (a re-applied `EX` snapshot would do both).
        let value = inspector.value_editor.read(cx).content();
        let edit = red_core::kv::KvEdit::SetString {
            key,
            value,
            ttl: red_core::kv::StringTtl::Keep,
        };
        self.service
            .send_to(session, Command::KvApplyEdit { epoch, edit });
    }

    pub(crate) fn kv_start_editing_ttl(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = active.kv_view.as_mut().and_then(|v| v.active_browse_mut()) else {
            return;
        };
        let Some(inspector) = &mut browse.inspector else {
            return;
        };
        let seed = inspector
            .ttl
            .map(|d| d.as_secs().to_string())
            .unwrap_or_default();
        inspector
            .ttl_editor
            .update(cx, |ti, cx| ti.set_content(seed, cx));
        inspector.ttl_error = None;
        inspector.editing_ttl = true;
        cx.notify();
    }

    pub(crate) fn kv_cancel_editing_ttl(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = active.kv_view.as_mut().and_then(|v| v.active_browse_mut()) else {
            return;
        };
        let Some(inspector) = &mut browse.inspector else {
            return;
        };
        inspector.editing_ttl = false;
        cx.notify();
    }

    /// Blank input persists the key (no expiry); otherwise parses as whole
    /// seconds. An unparseable, non-blank input reports inline in the popover
    /// (`ttl_error`) rather than silently doing nothing.
    pub(crate) fn kv_submit_ttl_edit(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some((epoch, key, text)) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_browse_mut())
            .and_then(|b| {
                let epoch = b.epoch;
                let inspector = b.inspector.as_ref()?;
                Some((
                    epoch,
                    inspector.key.clone(),
                    inspector.ttl_editor.read(cx).content().to_string(),
                ))
            })
        else {
            return;
        };
        let ttl = if text.trim().is_empty() {
            None
        } else {
            match text.trim().parse::<u64>() {
                Ok(secs) => Some(Duration::from_secs(secs)),
                Err(_) => {
                    if let Some(inspector) = self
                        .conn_mut(Some(session))
                        .and_then(|a| a.kv_view.as_mut())
                        .and_then(|v| v.active_browse_mut())
                        .and_then(|b| b.inspector.as_mut())
                    {
                        inspector.ttl_error =
                            Some("Enter whole seconds, or leave blank to persist".into());
                    }
                    cx.notify();
                    return;
                }
            }
        };
        if let Some(inspector) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_browse_mut())
            .and_then(|b| b.inspector.as_mut())
        {
            inspector.ttl_error = None;
        }
        let edit = red_core::kv::KvEdit::SetTtl { key, ttl };
        self.service
            .send_to(session, Command::KvApplyEdit { epoch, edit });
    }

    pub(crate) fn kv_start_editing_key(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = active.kv_view.as_mut().and_then(|v| v.active_browse_mut()) else {
            return;
        };
        let Some(inspector) = &mut browse.inspector else {
            return;
        };
        let seed = inspector.key.clone();
        inspector
            .rename_editor
            .update(cx, |ti, cx| ti.set_content(seed, cx));
        inspector.editing_key = true;
        cx.notify();
    }

    pub(crate) fn kv_cancel_editing_key(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = active.kv_view.as_mut().and_then(|v| v.active_browse_mut()) else {
            return;
        };
        let Some(inspector) = &mut browse.inspector else {
            return;
        };
        inspector.editing_key = false;
        cx.notify();
    }

    pub(crate) fn kv_submit_rename(&mut self, session: SessionId, cx: &mut Context<Self>) {
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
        let from = inspector.key.clone();
        let to = inspector.rename_editor.read(cx).content().to_string();
        if to.is_empty() || to == from {
            return;
        }
        let edit = red_core::kv::KvEdit::Rename { from, to };
        self.service
            .send_to(session, Command::KvApplyEdit { epoch, edit });
    }

    pub(crate) fn kv_request_delete(&mut self, session: SessionId, cx: &mut Context<Self>) {
        // Opt-out: when delete confirmations are disabled, delete straight away.
        if !self.settings.query.confirm_destructive {
            self.kv_confirm_delete(session, cx);
            return;
        }
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = active.kv_view.as_mut().and_then(|v| v.active_browse_mut()) else {
            return;
        };
        let Some(inspector) = &mut browse.inspector else {
            return;
        };
        inspector.confirm_delete = true;
        // Focus the modal so Flint's `Modal` hears Esc/Enter (see `render.rs`).
        self.focus_modal = true;
        cx.notify();
    }

    pub(crate) fn kv_cancel_delete(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = active.kv_view.as_mut().and_then(|v| v.active_browse_mut()) else {
            return;
        };
        let Some(inspector) = &mut browse.inspector else {
            return;
        };
        inspector.confirm_delete = false;
        cx.notify();
    }

    pub(crate) fn kv_confirm_delete(&mut self, session: SessionId, cx: &mut Context<Self>) {
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
        // Hide the confirm bar right away: the action is already committed.
        // If it somehow fails, the global error toast still fires; there's
        // just no stale confirm banner left sitting on screen.
        inspector.confirm_delete = false;
        let edit = red_core::kv::KvEdit::Delete {
            keys: vec![inspector.key.clone()],
        };
        self.service
            .send_to(session, Command::KvApplyEdit { epoch, edit });
        cx.notify();
    }

    /// Open the collection-element popover (add or edit a hash field / set
    /// member / zset member / list element), seeding the shared element editors.
    /// `seed_name` fills the field/member/list-value input; `seed_value` fills
    /// the hash-value/zset-score input (either is empty for an add).
    pub(crate) fn kv_open_collection_edit(
        &mut self,
        session: SessionId,
        kind: CollectionEditKind,
        seed_name: String,
        seed_value: String,
        cx: &mut Context<Self>,
    ) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = active.kv_view.as_mut().and_then(|v| v.active_browse_mut()) else {
            return;
        };
        let Some(inspector) = &mut browse.inspector else {
            return;
        };
        // Only one popover open at a time.
        inspector.editing_key = false;
        inspector.editing_ttl = false;
        inspector.elem_error = None;
        inspector
            .elem_name_editor
            .update(cx, |ti, cx| ti.set_content(seed_name, cx));
        inspector
            .elem_value_editor
            .update(cx, |ti, cx| ti.set_content(seed_value, cx));
        inspector.collection_edit = Some(kind);
        cx.notify();
    }

    pub(crate) fn kv_cancel_collection_edit(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = active.kv_view.as_mut().and_then(|v| v.active_browse_mut()) else {
            return;
        };
        let Some(inspector) = &mut browse.inspector else {
            return;
        };
        inspector.collection_edit = None;
        inspector.elem_error = None;
        cx.notify();
    }

    fn kv_set_elem_error(&mut self, session: SessionId, msg: String, cx: &mut Context<Self>) {
        if let Some(inspector) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_browse_mut())
            .and_then(|b| b.inspector.as_mut())
        {
            inspector.elem_error = Some(msg);
        }
        cx.notify();
    }

    /// Read the open element popover, build the matching `KvEdit`, and send
    /// it. A blank name / unparseable score surfaces inline in the popover
    /// (`elem_error`) rather than silently no-op'ing.
    pub(crate) fn kv_submit_collection_edit(&mut self, session: SessionId, cx: &mut Context<Self>) {
        use red_core::kv::KvEdit;
        let Some((epoch, key, kind, name, value)) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_browse_mut())
            .and_then(|b| {
                let epoch = b.epoch;
                let inspector = b.inspector.as_ref()?;
                let kind = inspector.collection_edit.clone()?;
                Some((
                    epoch,
                    inspector.key.clone(),
                    kind,
                    inspector.elem_name_editor.read(cx).content().to_string(),
                    inspector.elem_value_editor.read(cx).content().to_string(),
                ))
            })
        else {
            return;
        };

        // Parse a zset score up front so a bad value reports inline.
        let parse_score = |raw: &str| raw.trim().parse::<f64>();
        let edit = match kind {
            CollectionEditKind::AddHashField => {
                if name.is_empty() {
                    return self.kv_set_elem_error(session, "Field name is required".into(), cx);
                }
                KvEdit::SetField {
                    key,
                    field: name,
                    value,
                }
            }
            CollectionEditKind::EditHashField { field } => KvEdit::SetField { key, field, value },
            CollectionEditKind::AddSetMember => {
                if name.is_empty() {
                    return self.kv_set_elem_error(session, "Member is required".into(), cx);
                }
                KvEdit::SetAdd {
                    key,
                    members: vec![name],
                }
            }
            CollectionEditKind::EditSetMember { old } => {
                if name.is_empty() {
                    return self.kv_set_elem_error(session, "Member is required".into(), cx);
                }
                if name == old {
                    return self.kv_cancel_collection_edit(session, cx);
                }
                KvEdit::SetReplace {
                    key,
                    old,
                    new: name,
                }
            }
            CollectionEditKind::AddZSetMember => {
                if name.is_empty() {
                    return self.kv_set_elem_error(session, "Member is required".into(), cx);
                }
                let Ok(score) = parse_score(&value) else {
                    return self.kv_set_elem_error(session, "Score must be a number".into(), cx);
                };
                KvEdit::ZSetAdd {
                    key,
                    member: name,
                    score,
                }
            }
            CollectionEditKind::EditZSetScore { member } => {
                let Ok(score) = parse_score(&value) else {
                    return self.kv_set_elem_error(session, "Score must be a number".into(), cx);
                };
                KvEdit::ZSetAdd { key, member, score }
            }
            CollectionEditKind::AddListHead => KvEdit::ListPush {
                key,
                value: name,
                head: true,
            },
            CollectionEditKind::AddListTail => KvEdit::ListPush {
                key,
                value: name,
                head: false,
            },
            CollectionEditKind::EditListIndex { index } => KvEdit::ListSet {
                key,
                index,
                value: name,
            },
        };

        if let Some(inspector) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_browse_mut())
            .and_then(|b| b.inspector.as_mut())
        {
            inspector.collection_edit = None;
            inspector.elem_error = None;
        }
        self.service
            .send_to(session, Command::KvApplyEdit { epoch, edit });
        cx.notify();
    }

    /// Send a collection-element edit built from the current inspector key
    /// (the row-level Delete/replace helpers). No-op if the inspector closed.
    pub(super) fn kv_send_element_edit(
        &mut self,
        session: SessionId,
        make: impl FnOnce(String) -> red_core::kv::KvEdit,
        cx: &mut Context<Self>,
    ) {
        let Some((epoch, key)) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_ref())
            .and_then(|v| v.active_browse())
            .and_then(|b| Some((b.epoch, b.inspector.as_ref()?.key.clone())))
        else {
            return;
        };
        let edit = make(key);
        self.service
            .send_to(session, Command::KvApplyEdit { epoch, edit });
        cx.notify();
    }

    /// Open the "New key" popover, building its inputs (each submits the form).
    pub(crate) fn kv_open_create_key(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let sub = |editor: &Entity<TextInput>, cx: &mut Context<Self>| {
            cx.subscribe(editor, move |this, _, event: &TextInputEvent, cx| {
                if matches!(event, TextInputEvent::Submit) {
                    this.kv_submit_create_key(session, cx);
                }
            })
            .detach();
        };
        let name = cx.new(|cx| TextInput::new(cx).with_placeholder("key name…"));
        sub(&name, cx);
        let field = cx.new(|cx| TextInput::new(cx).with_placeholder("field…"));
        sub(&field, cx);
        let value = cx.new(|cx| TextInput::new(cx).with_placeholder("value…"));
        sub(&value, cx);
        let score = cx.new(|cx| TextInput::new(cx).with_placeholder("score (e.g. 1.0)"));
        sub(&score, cx);
        let ttl = cx.new(|cx| TextInput::new(cx).with_placeholder("seconds (optional)"));
        sub(&ttl, cx);

        if let Some(browse) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_browse_mut())
        {
            browse.create_key = Some(CreateKeyState {
                name,
                field,
                value,
                score,
                ttl,
                kv_type: KvType::String,
                list_head: false,
                error: None,
            });
        }
        // Focus the name field so the user can type immediately (see
        // `render.rs`'s `focus_create_key` handling), like the connection form.
        self.focus_create_key = true;
        cx.notify();
    }

    pub(crate) fn kv_cancel_create_key(&mut self, session: SessionId, cx: &mut Context<Self>) {
        if let Some(browse) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_browse_mut())
        {
            browse.create_key = None;
        }
        // Return focus to the browse from the dismissed modal.
        self.refocus_root = true;
        cx.notify();
    }

    pub(crate) fn kv_set_create_type(
        &mut self,
        session: SessionId,
        kv_type: KvType,
        cx: &mut Context<Self>,
    ) {
        if let Some(ck) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_browse_mut())
            .and_then(|b| b.create_key.as_mut())
        {
            ck.kv_type = kv_type;
            ck.error = None;
        }
        cx.notify();
    }

    /// Flip a new list's push end (`LPUSH` head ↔ `RPUSH` tail).
    pub(crate) fn kv_toggle_create_list_head(
        &mut self,
        session: SessionId,
        cx: &mut Context<Self>,
    ) {
        if let Some(ck) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_browse_mut())
            .and_then(|b| b.create_key.as_mut())
        {
            ck.list_head = !ck.list_head;
        }
        cx.notify();
    }

    fn kv_set_create_error(&mut self, session: SessionId, msg: String, cx: &mut Context<Self>) {
        if let Some(ck) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_browse_mut())
            .and_then(|b| b.create_key.as_mut())
        {
            ck.error = Some(msg);
        }
        cx.notify();
    }

    /// Validate the "New key" form, send the key's first write, then open the
    /// inspector on it so its value shows straight away. Blank required fields
    /// / a bad score report inline (`create_key.error`).
    pub(crate) fn kv_submit_create_key(&mut self, session: SessionId, cx: &mut Context<Self>) {
        use red_core::kv::KvEdit;
        let Some((epoch, name, kv_type, field, value, score, ttl, list_head)) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_browse_mut())
            .and_then(|b| {
                let epoch = b.epoch;
                let ck = b.create_key.as_ref()?;
                Some((
                    epoch,
                    ck.name.read(cx).content().trim().to_string(),
                    ck.kv_type.clone(),
                    ck.field.read(cx).content().to_string(),
                    ck.value.read(cx).content().to_string(),
                    ck.score.read(cx).content().to_string(),
                    ck.ttl.read(cx).content().trim().to_string(),
                    ck.list_head,
                ))
            })
        else {
            return;
        };
        if name.is_empty() {
            return self.kv_set_create_error(session, "Key name is required".into(), cx);
        }
        let key = name.clone();
        let edit = match &kv_type {
            KvType::String => {
                // An optional TTL in seconds; blank leaves the key persistent.
                let ttl = if ttl.is_empty() {
                    red_core::kv::StringTtl::Clear
                } else {
                    let Ok(secs) = ttl.parse::<f64>() else {
                        return self.kv_set_create_error(
                            session,
                            "TTL must be a number of seconds".into(),
                            cx,
                        );
                    };
                    if secs <= 0.0 {
                        return self.kv_set_create_error(
                            session,
                            "TTL must be greater than zero".into(),
                            cx,
                        );
                    }
                    red_core::kv::StringTtl::Set(Duration::from_secs_f64(secs))
                };
                KvEdit::SetString { key, value, ttl }
            }
            KvType::Hash => {
                if field.is_empty() {
                    return self.kv_set_create_error(session, "Field name is required".into(), cx);
                }
                KvEdit::SetField { key, field, value }
            }
            KvType::List => KvEdit::ListPush {
                key,
                value,
                head: list_head,
            },
            KvType::Set => {
                if value.is_empty() {
                    return self.kv_set_create_error(session, "Member is required".into(), cx);
                }
                KvEdit::SetAdd {
                    key,
                    members: vec![value],
                }
            }
            KvType::ZSet => {
                if value.is_empty() {
                    return self.kv_set_create_error(session, "Member is required".into(), cx);
                }
                let Ok(score) = score.trim().parse::<f64>() else {
                    return self.kv_set_create_error(session, "Score must be a number".into(), cx);
                };
                KvEdit::ZSetAdd {
                    key,
                    member: value,
                    score,
                }
            }
            KvType::Stream => {
                if field.is_empty() {
                    return self.kv_set_create_error(session, "Field name is required".into(), cx);
                }
                // First entry, one field/value pair; the id is server-assigned.
                KvEdit::StreamAdd {
                    key,
                    fields: vec![(field, value)],
                }
            }
            KvType::Other(_) => return,
        };

        if let Some(browse) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_browse_mut())
        {
            browse.create_key = None;
        }
        // The modal (and its focused input) is gone; return focus to the root so
        // keyboard nav keeps working.
        self.refocus_root = true;
        self.service
            .send_to(session, Command::KvApplyEdit { epoch, edit });
        // Show the freshly created key immediately.
        self.kv_open_inspector(session, name, None, kv_type, cx);
        cx.notify();
    }

    /// `Event::KvEditApplied`: patch local state so the UI reflects the edit
    /// without a full re-fetch. Drops the reply if the browse it targets has
    /// since been superseded (a filter restart bumped the epoch).
    pub(crate) fn on_kv_edit_applied(
        &mut self,
        session: Option<SessionId>,
        epoch: red_service::Epoch,
        edit: red_core::kv::KvEdit,
        cx: &mut Context<Self>,
    ) {
        use red_core::kv::KvEdit;
        let Some(active) = self.conn_mut(session) else {
            return;
        };
        // Route by the owning scan epoch across all tabs, not the focused tab: in
        // split view the edit must patch the tab it was issued from even if focus
        // moved. A superseded epoch (filter restart) still finds nothing and drops.
        let Some(browse) = active
            .kv_view
            .as_mut()
            .and_then(|v| v.browse_by_scan_epoch_mut(epoch))
        else {
            return;
        };
        match edit {
            KvEdit::SetString { key, value, ttl } => {
                if let Some(inspector) = &mut browse.inspector
                    && inspector.key == key
                {
                    inspector.value = Some(KvValue::Str(red_core::Value::Text(value.into())));
                    inspector.editing_value = false;
                }
                if let Some(row) = browse.rows_mut().iter_mut().find(|r| r.key == key) {
                    // Mirror the write's TTL intent onto the optimistic row.
                    match ttl {
                        red_core::kv::StringTtl::Keep => {}
                        red_core::kv::StringTtl::Clear => row.ttl = None,
                        red_core::kv::StringTtl::Set(d) => row.ttl = Some(d),
                    }
                }
            }
            KvEdit::SetField { key, field, value } => {
                if let Some(inspector) = &mut browse.inspector
                    && inspector.key == key
                    && let Some(KvValue::Hash(KvCollection::Loaded(pairs))) = &mut inspector.value
                {
                    match pairs.iter_mut().find(|(f, _)| *f == field) {
                        Some((_, v)) => *v = value,
                        None => pairs.push((field, value)),
                    }
                }
            }
            KvEdit::HashDelete { key, fields } => {
                if let Some(insp) = &mut browse.inspector
                    && insp.key == key
                {
                    if let Some(KvValue::Hash(KvCollection::Loaded(pairs))) = &mut insp.value {
                        pairs.retain(|(f, _)| !fields.contains(f));
                    }
                    Rc::make_mut(&mut insp.collection_rows).retain(|e| match e {
                        KvElement::Field(f, _) => !fields.contains(f),
                        _ => true,
                    });
                }
            }
            KvEdit::SetAdd { key, members } => {
                if let Some(insp) = &mut browse.inspector
                    && insp.key == key
                {
                    if let Some(KvValue::Set(KvCollection::Loaded(items))) = &mut insp.value {
                        for m in &members {
                            if !items.contains(m) {
                                items.push(m.clone());
                            }
                        }
                    }
                    for m in &members {
                        let present = insp
                            .collection_rows
                            .iter()
                            .any(|e| matches!(e, KvElement::Member(x) if x == m));
                        if !present {
                            Rc::make_mut(&mut insp.collection_rows)
                                .push(KvElement::Member(m.clone()));
                        }
                    }
                }
            }
            KvEdit::SetRemove { key, members } => {
                if let Some(insp) = &mut browse.inspector
                    && insp.key == key
                {
                    if let Some(KvValue::Set(KvCollection::Loaded(items))) = &mut insp.value {
                        items.retain(|m| !members.contains(m));
                    }
                    Rc::make_mut(&mut insp.collection_rows).retain(|e| match e {
                        KvElement::Member(x) => !members.contains(x),
                        _ => true,
                    });
                }
            }
            KvEdit::SetReplace { key, old, new } => {
                if let Some(insp) = &mut browse.inspector
                    && insp.key == key
                {
                    if let Some(KvValue::Set(KvCollection::Loaded(items))) = &mut insp.value
                        && let Some(slot) = items.iter_mut().find(|m| **m == old)
                    {
                        *slot = new.clone();
                    }
                    for e in Rc::make_mut(&mut insp.collection_rows).iter_mut() {
                        if let KvElement::Member(x) = e
                            && *x == old
                        {
                            *x = new.clone();
                        }
                    }
                }
            }
            KvEdit::ZSetAdd { key, member, score } => {
                if let Some(insp) = &mut browse.inspector
                    && insp.key == key
                {
                    if let Some(KvValue::ZSet(KvCollection::Loaded(items))) = &mut insp.value {
                        match items.iter().position(|(m, _)| *m == member) {
                            Some(pos) => items[pos].1 = score,
                            None => items.push((member.clone(), score)),
                        }
                    }
                    match insp
                        .collection_rows
                        .iter()
                        .position(|e| matches!(e, KvElement::Scored(m, _) if *m == member))
                    {
                        Some(pos) => {
                            if let KvElement::Scored(_, s) =
                                &mut Rc::make_mut(&mut insp.collection_rows)[pos]
                            {
                                *s = score;
                            }
                        }
                        None => Rc::make_mut(&mut insp.collection_rows)
                            .push(KvElement::Scored(member.clone(), score)),
                    }
                }
            }
            KvEdit::ZSetRemove { key, members } => {
                if let Some(insp) = &mut browse.inspector
                    && insp.key == key
                {
                    if let Some(KvValue::ZSet(KvCollection::Loaded(items))) = &mut insp.value {
                        items.retain(|(m, _)| !members.contains(m));
                    }
                    Rc::make_mut(&mut insp.collection_rows).retain(|e| match e {
                        KvElement::Scored(m, _) => !members.contains(m),
                        _ => true,
                    });
                }
            }
            KvEdit::ListSet { key, index, value } => {
                if let Some(insp) = &mut browse.inspector
                    && insp.key == key
                    && index >= 0
                {
                    let idx = index as usize;
                    if let Some(KvValue::List(KvCollection::Loaded(items))) = &mut insp.value
                        && let Some(slot) = items.get_mut(idx)
                    {
                        *slot = value.clone();
                    }
                    if let Some(KvElement::Member(x)) =
                        Rc::make_mut(&mut insp.collection_rows).get_mut(idx)
                    {
                        *x = value.clone();
                    }
                }
            }
            KvEdit::ListPush { key, value, head } => {
                if let Some(insp) = &mut browse.inspector
                    && insp.key == key
                {
                    if let Some(KvValue::List(KvCollection::Loaded(items))) = &mut insp.value {
                        if head {
                            items.insert(0, value.clone());
                        } else {
                            items.push(value.clone());
                        }
                    }
                    // For a large list, `collection_rows` is a head window:
                    // a head push shows immediately; a tail push only when
                    // the whole list is loaded (else it lands off-window).
                    if head {
                        Rc::make_mut(&mut insp.collection_rows)
                            .insert(0, KvElement::Member(value.clone()));
                    } else if insp.collection_exhausted && !insp.collection_head_only {
                        Rc::make_mut(&mut insp.collection_rows)
                            .push(KvElement::Member(value.clone()));
                    }
                }
            }
            KvEdit::ListRemove { key, value, .. } => {
                if let Some(insp) = &mut browse.inspector
                    && insp.key == key
                {
                    if let Some(KvValue::List(KvCollection::Loaded(items))) = &mut insp.value
                        && let Some(pos) = items.iter().position(|v| *v == value)
                    {
                        items.remove(pos);
                    }
                    if let Some(pos) = insp
                        .collection_rows
                        .iter()
                        .position(|e| matches!(e, KvElement::Member(x) if *x == value))
                    {
                        Rc::make_mut(&mut insp.collection_rows).remove(pos);
                    }
                }
            }
            KvEdit::ListRemoveAt { key, index } => {
                if let Some(insp) = &mut browse.inspector
                    && insp.key == key
                    && index >= 0
                {
                    let idx = index as usize;
                    if let Some(KvValue::List(KvCollection::Loaded(items))) = &mut insp.value
                        && idx < items.len()
                    {
                        items.remove(idx);
                    }
                    let rows = Rc::make_mut(&mut insp.collection_rows);
                    if idx < rows.len() {
                        rows.remove(idx);
                    }
                }
            }
            KvEdit::SetTtl { key, ttl } => {
                if let Some(inspector) = &mut browse.inspector
                    && inspector.key == key
                {
                    inspector.ttl = ttl;
                    inspector.editing_ttl = false;
                }
                if let Some(row) = browse.rows_mut().iter_mut().find(|r| r.key == key) {
                    row.ttl = ttl;
                }
            }
            KvEdit::Rename { from, to } => {
                if let Some(inspector) = &mut browse.inspector
                    && inspector.key == from
                {
                    inspector.key = to.clone();
                    inspector.editing_key = false;
                }
                if let Some(row) = browse.rows_mut().iter_mut().find(|r| r.key == from) {
                    row.key = to;
                }
            }
            KvEdit::Delete { keys } => {
                if let Some(inspector) = &browse.inspector
                    && keys.contains(&inspector.key)
                {
                    browse.inspector = None;
                }
                browse.rows_mut().retain(|r| !keys.contains(&r.key));
            }
            // A newly created stream: the inspector was already opened optimistically
            // on it (see `kv_submit_create_key`), which fetches the entry fresh, so
            // there's no local buffer to patch.
            KvEdit::StreamAdd { .. } => {}
        }
        // A `SetString` replaced the string body (and cleared the edit); refresh
        // the selectable preview so it shows the just-written value.
        if let Some(session) = session {
            self.kv_rebuild_str_preview(session, cx);
        }
        cx.notify();
    }

    /// `Event::KvKeysRecycled`: a delete captured these keys' `DUMP` snapshots.
    /// Hold them in the recycle bin and raise an "Undo" toast that restores them
    /// (see [`Self::kv_undo_delete`]). Emitted just before the matching
    /// `KvEditApplied` that removes the rows.
    pub(crate) fn on_kv_keys_recycled(
        &mut self,
        session: Option<SessionId>,
        epoch: red_service::Epoch,
        keys: Vec<RecycledKey>,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = session else { return };
        if keys.is_empty() {
            return;
        }
        let n = keys.len();
        let id = self.next_recycle_id;
        self.next_recycle_id += 1;
        self.recycle_bin.push(RecycleBatch {
            id,
            session,
            epoch,
            keys,
        });
        // Evict the oldest batches beyond the cap (a session-scoped, bounded bin).
        if self.recycle_bin.len() > RECYCLE_BIN_CAP {
            let overflow = self.recycle_bin.len() - RECYCLE_BIN_CAP;
            self.recycle_bin.drain(0..overflow);
        }
        let msg = if n == 1 {
            "Deleted 1 key".to_string()
        } else {
            format!("Deleted {n} keys")
        };
        // A persistent (no auto-dismiss) toast: the restore button is the undo
        // affordance, so it must stay until the user acts or closes it.
        self.push_notification(
            Notification {
                id: 0,
                variant: ToastVariant::Info,
                message: msg.into(),
                detail: Some("Restore to undo".into()),
                detail_label: None,
                auto_dismiss: None,
                export: None,
                expanded: false,
                hovered: false,
                dismiss_gen: 0,
                action: Some(NotificationAction::UndoDelete(id)),
            },
            cx,
        );
    }

    /// `Event::KvKeysRestored`: an undo finished. Confirm it and re-scan the
    /// active browse so the restored keys reappear.
    pub(crate) fn on_kv_keys_restored(
        &mut self,
        session: Option<SessionId>,
        _epoch: red_service::Epoch,
        count: u64,
        cx: &mut Context<Self>,
    ) {
        let msg = if count == 1 {
            "Restored 1 key".to_string()
        } else {
            format!("Restored {count} keys")
        };
        self.notify(ToastVariant::Success, msg, cx);
        if let Some(session) = session {
            self.kv_relaunch_browse(session, cx);
        }
    }

    /// The "Undo" toast action: `RESTORE` a recycle batch's keys on the server
    /// (see `Command::KvRestoreKeys`), consuming the batch.
    pub(crate) fn kv_undo_delete(&mut self, batch_id: u64, cx: &mut Context<Self>) {
        let Some(pos) = self.recycle_bin.iter().position(|b| b.id == batch_id) else {
            return; // already restored, evicted, or on another window
        };
        let batch = self.recycle_bin.remove(pos);
        self.service.send_to(
            batch.session,
            Command::KvRestoreKeys {
                epoch: batch.epoch,
                keys: batch.keys,
            },
        );
        cx.notify();
    }

    /// `Event::KvValueReady`: apply it if the inspector is still open on this
    /// key (a `key` comparison, not the browse's epoch, since the inspector
    /// can outlive a filter-triggered scan restart). A `Large` collection
    /// auto-loads its first page/window right away, same one-click-in flow
    /// as opening the inspector itself.
    pub(crate) fn on_kv_value_ready(
        &mut self,
        session: Option<SessionId>,
        key: String,
        value: Option<KvValue>,
        cx: &mut Context<Self>,
    ) {
        let Some(active) = self.conn_mut(session) else {
            return;
        };
        // Route by the inspector's key across all tabs, not the focused tab: in
        // split view the reply must reach the tab that asked even if focus moved.
        let Some(browse) = active
            .kv_view
            .as_mut()
            .and_then(|v| v.browse_by_inspector_key_mut(&key))
        else {
            return; // no open inspector on this key: a newer selection superseded it
        };
        let Some(inspector) = &mut browse.inspector else {
            return;
        };
        inspector.value = value.clone();
        inspector.value_loaded = true;
        inspector.value_error = None;
        // Whether this is the initial capped read or a "Load full value" reply,
        // the string body is now settled — drop the loading state either way.
        inspector.loading_full_value = false;
        // A pending "Edit" that was waiting on the full value can now open, but
        // only if the loaded body is editable text (a binary `Blob` is not).
        let start_edit = inspector.edit_after_load
            && matches!(&value, Some(KvValue::Str(red_core::Value::Text(_))));
        inspector.edit_after_load = false;
        cx.notify();
        let Some(session) = session else { return };
        if start_edit {
            self.kv_start_editing_value(session, cx);
            return;
        }
        // Build the selectable read-only preview for a freshly-loaded string.
        self.kv_rebuild_str_preview(session, cx);
        match value {
            Some(KvValue::Hash(KvCollection::Large { .. })) => {
                self.kv_load_collection_page(session, CollectionKind::Hash, cx);
            }
            Some(KvValue::Set(KvCollection::Large { .. })) => {
                self.kv_load_collection_page(session, CollectionKind::Set, cx);
            }
            Some(KvValue::ZSet(KvCollection::Large { .. })) => {
                self.kv_load_collection_page(session, CollectionKind::ZSet, cx);
            }
            Some(KvValue::List(KvCollection::Large { .. })) => {
                self.kv_load_list_preview(session, cx);
            }
            Some(KvValue::Stream(KvCollection::Large { .. })) => {
                self.kv_load_stream_page(session, cx);
            }
            _ => {}
        }
    }

    /// `Event::KvValueError`: a value read failed. Settle the inspector's
    /// value area on the error (for the matching key) instead of leaving it on
    /// a permanent "Loading…".
    pub(crate) fn on_kv_value_error(
        &mut self,
        session: Option<SessionId>,
        key: String,
        message: String,
        cx: &mut Context<Self>,
    ) {
        if let Some(inspector) = self
            .conn_mut(session)
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.browse_by_inspector_key_mut(&key))
            .and_then(|b| b.inspector.as_mut())
        {
            inspector.value_loaded = true;
            inspector.value_error = Some(message);
            inspector.loading_full_value = false;
            cx.notify();
        }
    }

    /// Fetch the next page of the inspector's big hash/set/zset, or the
    /// first page if none has loaded yet. The keyspace table's
    /// `on_visible_range` calls this too, once the sub-grid's own visible
    /// range nears the end of what's loaded (see `render_kv_inspector`).
    pub(crate) fn kv_load_collection_page(
        &mut self,
        session: SessionId,
        kind: CollectionKind,
        cx: &mut Context<Self>,
    ) {
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
        if inspector.collection_loading || inspector.collection_exhausted {
            return;
        }
        inspector.collection_loading = true;
        let key = inspector.key.clone();
        let cursor = inspector.collection_cursor;
        self.service.send_to(
            session,
            Command::KvReadCollectionPage {
                epoch,
                key,
                kind,
                cursor,
                budget: scan_budget(),
            },
        );
        cx.notify();
    }

    fn kv_load_list_preview(&mut self, session: SessionId, cx: &mut Context<Self>) {
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
        inspector.collection_loading = true;
        let key = inspector.key.clone();
        self.service.send_to(
            session,
            Command::KvReadListWindow {
                epoch,
                key,
                from_head: true,
                count: LIST_PREVIEW_COUNT,
            },
        );
        cx.notify();
    }

    /// The inspector sub-grid's `on_visible_range` hook, mirroring
    /// `kv_maybe_load_more` for the top-level keyspace table.
    pub(crate) fn kv_inspector_maybe_load_more(
        &mut self,
        session: SessionId,
        kind: CollectionKind,
        visible_end: usize,
        cx: &mut Context<Self>,
    ) {
        let loaded = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_ref())
            .and_then(|v| v.active_browse())
            .and_then(|b| b.inspector.as_ref())
            .map(|i| i.collection_rows.len());
        let Some(loaded) = loaded else {
            return;
        };
        if visible_end + LOAD_AHEAD_ROWS < loaded {
            return;
        }
        self.kv_load_collection_page(session, kind, cx);
    }

    pub(crate) fn on_kv_collection_page_ready(
        &mut self,
        session: Option<SessionId>,
        key: String,
        page: KvCollectionPage,
        cx: &mut Context<Self>,
    ) {
        let Some(active) = self.conn_mut(session) else {
            return;
        };
        let Some(browse) = active
            .kv_view
            .as_mut()
            .and_then(|v| v.browse_by_inspector_key_mut(&key))
        else {
            return;
        };
        let Some(inspector) = &mut browse.inspector else {
            return;
        };
        Rc::make_mut(&mut inspector.collection_rows).extend(page.elements);
        inspector.collection_cursor = page.next_cursor;
        inspector.collection_exhausted = page.exhausted;
        inspector.collection_loading = false;
        cx.notify();
    }

    pub(crate) fn on_kv_list_window_ready(
        &mut self,
        session: Option<SessionId>,
        key: String,
        values: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(active) = self.conn_mut(session) else {
            return;
        };
        let Some(browse) = active
            .kv_view
            .as_mut()
            .and_then(|v| v.browse_by_inspector_key_mut(&key))
        else {
            return;
        };
        let Some(inspector) = &mut browse.inspector else {
            return;
        };
        inspector.collection_rows = Rc::new(values.into_iter().map(KvElement::Member).collect());
        // A list's head-window preview is a one-shot fetch, not paged: mark it
        // exhausted (no more pages) but also head-only, so a tail append isn't
        // optimistically shown inside the head window (it lands off-window).
        inspector.collection_exhausted = true;
        inspector.collection_head_only = true;
        inspector.collection_loading = false;
        cx.notify();
    }

    /// Fetch the next (older) page of the inspector's big stream, or the first
    /// (newest) page if none has loaded yet. Mirrors `kv_load_collection_page`
    /// but continues by entry ID (`stream_before`) rather than a `*SCAN`
    /// cursor.
    pub(crate) fn kv_load_stream_page(&mut self, session: SessionId, cx: &mut Context<Self>) {
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
        if inspector.stream_loading || inspector.stream_exhausted {
            return;
        }
        inspector.stream_loading = true;
        let key = inspector.key.clone();
        let before = inspector.stream_before.clone();
        self.service.send_to(
            session,
            Command::KvReadStreamPage {
                epoch,
                key,
                before,
                count: STREAM_PAGE_COUNT,
            },
        );
        cx.notify();
    }

    /// The stream sub-grid's `on_visible_range` hook, mirroring
    /// `kv_inspector_maybe_load_more` for a big hash/set/zset.
    pub(crate) fn kv_inspector_maybe_load_more_stream(
        &mut self,
        session: SessionId,
        visible_end: usize,
        cx: &mut Context<Self>,
    ) {
        let loaded = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_ref())
            .and_then(|v| v.active_browse())
            .and_then(|b| b.inspector.as_ref())
            .map(|i| i.stream_rows.len());
        let Some(loaded) = loaded else {
            return;
        };
        if visible_end + LOAD_AHEAD_ROWS < loaded {
            return;
        }
        self.kv_load_stream_page(session, cx);
    }

    pub(crate) fn on_kv_stream_page_ready(
        &mut self,
        session: Option<SessionId>,
        key: String,
        page: KvStreamPage,
        cx: &mut Context<Self>,
    ) {
        let Some(active) = self.conn_mut(session) else {
            return;
        };
        let Some(browse) = active
            .kv_view
            .as_mut()
            .and_then(|v| v.browse_by_inspector_key_mut(&key))
        else {
            return;
        };
        let Some(inspector) = &mut browse.inspector else {
            return;
        };
        Rc::make_mut(&mut inspector.stream_rows).extend(page.entries);
        inspector.stream_before = page.next_before;
        inspector.stream_exhausted = page.exhausted;
        inspector.stream_loading = false;
        cx.notify();
    }

    // --- stream consumer groups (see docs/plans/redis.md's "stream
    // consumer-group management" gap) ---

    /// Switch the stream inspector between its entries grid and its
    /// consumer-group view. Opening the Groups tab for the first time kicks
    /// off the lazy `XINFO GROUPS` load.
    pub(crate) fn kv_set_stream_view(
        &mut self,
        session: SessionId,
        view: StreamView,
        cx: &mut Context<Self>,
    ) {
        let need_load = {
            let Some(inspector) = self.kv_inspector_mut(session) else {
                return;
            };
            inspector.stream_groups.view = view;
            view == StreamView::Groups && !inspector.stream_groups.loaded
        };
        if need_load {
            self.kv_load_stream_groups(session, cx);
        }
        cx.notify();
    }

    /// Fetch (or refresh) the stream's consumer groups.
    pub(crate) fn kv_load_stream_groups(&mut self, session: SessionId, cx: &mut Context<Self>) {
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
        inspector.stream_groups.loading = true;
        let key = inspector.key.clone();
        self.service
            .send_to(session, Command::KvStreamGroups { epoch, key });
        cx.notify();
    }

    pub(crate) fn on_kv_stream_groups_ready(
        &mut self,
        session: Option<SessionId>,
        key: String,
        groups: Vec<StreamGroup>,
        cx: &mut Context<Self>,
    ) {
        let Some(inspector) = self.kv_inspector_for(session) else {
            return;
        };
        if inspector.key != key {
            return;
        }
        inspector.stream_groups.loaded = true;
        inspector.stream_groups.loading = false;
        // Keep a valid selection: default to the first group, and if the
        // previously-selected group is gone (dropped meanwhile), fall back.
        let still_present = inspector
            .stream_groups
            .selected
            .as_ref()
            .is_some_and(|s| groups.iter().any(|g| &g.name == s));
        let auto_select = (!still_present).then(|| groups.first().map(|g| g.name.clone()));
        inspector.stream_groups.groups = groups;
        cx.notify();
        if let Some(Some(first)) = auto_select
            && let Some(session) = session
        {
            self.kv_select_stream_group(session, first, cx);
        }
    }

    /// Select a group and load its consumers + pending entries.
    pub(crate) fn kv_select_stream_group(
        &mut self,
        session: SessionId,
        group: String,
        cx: &mut Context<Self>,
    ) {
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
        inspector.stream_groups.selected = Some(group.clone());
        inspector.stream_groups.consumers.clear();
        inspector.stream_groups.pending.clear();
        inspector.stream_groups.claiming = None;
        inspector.stream_groups.detail_loading = true;
        self.service.send_to(
            session,
            Command::KvStreamConsumers {
                epoch,
                key: key.clone(),
                group: group.clone(),
            },
        );
        self.service.send_to(
            session,
            Command::KvStreamPending {
                epoch,
                key,
                group,
                count: STREAM_PENDING_COUNT,
            },
        );
        cx.notify();
    }

    pub(crate) fn on_kv_stream_consumers_ready(
        &mut self,
        session: Option<SessionId>,
        key: String,
        group: String,
        consumers: Vec<StreamConsumer>,
        cx: &mut Context<Self>,
    ) {
        let Some(inspector) = self.kv_inspector_for(session) else {
            return;
        };
        // Drop a reply for a key/group the inspector has since moved off.
        if inspector.key != key || inspector.stream_groups.selected.as_deref() != Some(&group) {
            return;
        }
        inspector.stream_groups.consumers = consumers;
        inspector.stream_groups.detail_loading = false;
        cx.notify();
    }

    pub(crate) fn on_kv_stream_pending_ready(
        &mut self,
        session: Option<SessionId>,
        key: String,
        group: String,
        pending: Vec<PendingEntry>,
        cx: &mut Context<Self>,
    ) {
        let Some(inspector) = self.kv_inspector_for(session) else {
            return;
        };
        if inspector.key != key || inspector.stream_groups.selected.as_deref() != Some(&group) {
            return;
        }
        inspector.stream_groups.pending = pending;
        inspector.stream_groups.detail_loading = false;
        cx.notify();
    }

    /// Acknowledge one pending entry (`XACK`), dropping it from the group's PEL.
    pub(crate) fn kv_stream_ack(&mut self, session: SessionId, id: String, cx: &mut Context<Self>) {
        self.kv_send_stream_action(session, KvStreamActionReq::Ack { ids: vec![id] }, cx);
    }

    /// Open the inline "claim to consumer" form for one pending entry.
    pub(crate) fn kv_start_claim(
        &mut self,
        session: SessionId,
        id: String,
        cx: &mut Context<Self>,
    ) {
        let Some(inspector) = self.kv_inspector_mut(session) else {
            return;
        };
        inspector
            .stream_groups
            .claim_editor
            .update(cx, |ti, cx| ti.set_content(String::new(), cx));
        inspector.stream_groups.claiming = Some(id);
        cx.notify();
    }

    pub(crate) fn kv_cancel_claim(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(inspector) = self.kv_inspector_mut(session) else {
            return;
        };
        inspector.stream_groups.claiming = None;
        cx.notify();
    }

    /// Submit the open claim form: reassign the pending entry to the typed
    /// consumer (`XCLAIM`, `min-idle 0` since the operator is deliberately
    /// reclaiming it now). A blank consumer name is a no-op.
    pub(crate) fn kv_submit_claim(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(inspector) = self.kv_inspector_mut(session) else {
            return;
        };
        let Some(id) = inspector.stream_groups.claiming.clone() else {
            return;
        };
        let consumer = inspector
            .stream_groups
            .claim_editor
            .read(cx)
            .content()
            .trim()
            .to_string();
        if consumer.is_empty() {
            return;
        }
        inspector.stream_groups.claiming = None;
        self.kv_send_stream_action(
            session,
            KvStreamActionReq::Claim {
                consumer,
                min_idle_ms: 0,
                ids: vec![id],
            },
            cx,
        );
    }

    /// Shared send path for `XACK`/`XCLAIM`: needs the selected group, which
    /// both actions target.
    fn kv_send_stream_action(
        &mut self,
        session: SessionId,
        action: KvStreamActionReq,
        cx: &mut Context<Self>,
    ) {
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
        let Some(group) = inspector.stream_groups.selected.clone() else {
            return;
        };
        let key = inspector.key.clone();
        self.service.send_to(
            session,
            Command::KvStreamAction {
                epoch,
                key,
                group,
                action,
            },
        );
        cx.notify();
    }

    pub(crate) fn on_kv_stream_action_done(
        &mut self,
        session: Option<SessionId>,
        key: String,
        group: String,
        action: StreamAction,
        count: u64,
        cx: &mut Context<Self>,
    ) {
        let verb = match action {
            StreamAction::Ack => "Acknowledged",
            StreamAction::Claim => "Claimed",
        };
        let plural = if count == 1 { "entry" } else { "entries" };
        self.notify(
            ToastVariant::Success,
            format!("{verb} {count} pending {plural} in \"{group}\""),
            cx,
        );
        let Some(session) = session else { return };
        // Refresh the affected group's detail and the group list (pending /
        // consumer counts just changed), matching the current inspector.
        let matches = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_ref())
            .and_then(|v| v.active_browse())
            .and_then(|b| b.inspector.as_ref())
            .is_some_and(|i| i.key == key && i.stream_groups.selected.as_deref() == Some(&group));
        if matches {
            self.kv_select_stream_group(session, group, cx);
            self.kv_load_stream_groups(session, cx);
        }
    }

    /// The current inspector for `session` if the browse is live, borrowed
    /// mutably — the shared preamble every group handler needs.
    fn kv_inspector_mut(&mut self, session: SessionId) -> Option<&mut KvInspector> {
        self.conn_mut(Some(session))?
            .kv_view
            .as_mut()?
            .active_browse_mut()?
            .inspector
            .as_mut()
    }

    /// Like [`kv_inspector_mut`](Self::kv_inspector_mut) but resolving the
    /// session `Option` an event carries (events are delivered with the
    /// originating `SessionId`, or `None` for the foreground).
    fn kv_inspector_for(&mut self, session: Option<SessionId>) -> Option<&mut KvInspector> {
        self.conn_mut(session)?
            .kv_view
            .as_mut()?
            .active_browse_mut()?
            .inspector
            .as_mut()
    }
}
