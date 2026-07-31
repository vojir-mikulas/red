//! Redis tab management + the key context-menu, split out of `kvbrowse/mod.rs`
//! (guidelines D): open/close/step/activate/drop/reorder tabs, the split view and
//! pin, the tab and key menus, key copy/annotate/delete, and opening a key in the
//! console. A second `impl AppState` block over the parent's state (`use super::*`).

use std::time::Duration;

use flint::prelude::*;
use gpui::{Context, prelude::*};
use red_core::kv::KvType;
use red_service::{Command, SessionId};

use crate::app::{ActiveConn, AppState, DropZone, PaneId, Phase, SplitWorkspace, TabWorkspace};

use super::*;

impl AppState {
    /// The session of the active connection when it's a Redis one, for the
    /// Redis palette commands (which run against the focused connection).
    pub(crate) fn kv_active_session(&self) -> Option<SessionId> {
        match &self.phase {
            Phase::Connected(a) if a.kv_view.is_some() => Some(a.session),
            _ => None,
        }
    }

    /// Open a specific Redis panel in a new tab (the palette's "redis: analyze /
    /// console / …" commands), reusing the empty-tab + set-kind flow so the
    /// panel's lazy first load fires just like the chooser.
    pub(crate) fn kv_open_panel(
        &mut self,
        session: SessionId,
        panel: KvPanel,
        cx: &mut Context<Self>,
    ) {
        self.kv_new_empty_tab(session, cx);
        let id = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_ref())
            .and_then(|v| v.tabs.last())
            .map(|t| t.id);
        if let Some(id) = id {
            self.kv_set_tab_kind(session, id, panel, cx);
        }
    }

    /// Open a new blank tab in the focused pane (the ＋ / ⌘T action). Its body
    /// shows the type chooser; picking a kind converts it in place via
    /// `kv_set_tab_kind`. Mirrors the SQL side's `new_query`.
    pub(crate) fn kv_new_empty_tab(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
        else {
            return;
        };
        let pane = view.focused_pane();
        let id = view.tab_seq;
        view.tab_seq += 1;
        view.tabs.push(RedisTab {
            id,
            title: "New tab".to_string(),
            state: RedisTabState::Empty,
            pane,
            pinned: false,
        });
        let new_idx = view.tabs.len() - 1;
        view.set_pane_active(pane, new_idx);
        cx.notify();
    }

    /// Convert the (blank) tab with `id` to `kind`, retitle it, and fire its
    /// lazy first load — the empty-tab chooser's action.
    pub(crate) fn kv_set_tab_kind(
        &mut self,
        session: SessionId,
        id: u64,
        kind: KvPanel,
        cx: &mut Context<Self>,
    ) {
        let state = RedisTabState::new(
            kind,
            session,
            self.settings.kv.default_query_mode.into(),
            cx,
        );
        let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
        else {
            return;
        };
        let Some(idx) = view.tab_index_by_id(id) else {
            return;
        };
        view.tabs[idx].state = state;
        view.tabs[idx].title = kind.label().to_string();
        let half = view.tabs[idx].pane;
        view.set_pane_active(half, idx);
        // Fire the chosen kind's lazy first load, the same way the old
        // single-panel shell did on first switch.
        match kind {
            KvPanel::Browse => self.kv_start_browse(session, cx),
            KvPanel::Monitor => self.kv_load_slowlog(session, cx),
            KvPanel::Analysis => self.kv_load_saved_analysis(session, cx),
            KvPanel::Keyspace => self.kv_keyspace_load_config(session, cx),
            KvPanel::Console | KvPanel::PubSub => {}
        }
        cx.notify();
    }

    /// Keyboard driving of the blank-tab chooser (see `render_kv_new_tab`), for
    /// the empty tab with stable id `id`: digits `1`–`6` pick a panel outright,
    /// ←/↑ and →/↓ move the highlight (wrapping), and Enter/Space commit the
    /// highlighted one. Returns `true` when it consumed the key.
    pub(crate) fn kv_new_tab_key(
        &mut self,
        session: SessionId,
        id: u64,
        key: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        let n = KV_NEW_TAB_CHOICES.len();
        // A direct digit pick (`1`–`6`).
        if let Ok(d) = key.parse::<usize>()
            && (1..=n).contains(&d)
        {
            self.kv_set_tab_kind(session, id, KV_NEW_TAB_CHOICES[d - 1].0, cx);
            return true;
        }
        let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
        else {
            return false;
        };
        match key {
            "left" | "up" => {
                view.new_tab_sel = (view.new_tab_sel + n - 1) % n;
                cx.notify();
                true
            }
            "right" | "down" => {
                view.new_tab_sel = (view.new_tab_sel + 1) % n;
                cx.notify();
                true
            }
            "enter" | "space" => {
                let sel = view.new_tab_sel.min(n - 1);
                self.kv_set_tab_kind(session, id, KV_NEW_TAB_CHOICES[sel].0, cx);
                true
            }
            _ => false,
        }
    }

    /// Step the focused half's active tab one slot forward/back, wrapping (the
    /// ctrl-tab / ctrl-shift-tab bindings). Shares the wrap math with the SQL
    /// side via [`crate::app::tabs::cycle_tab_index`].
    pub(crate) fn kv_step_tab(
        &mut self,
        session: SessionId,
        forward: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
        else {
            return;
        };
        let pane = view.focused_pane();
        let pane_tabs = view.pane_tab_indices(pane);
        let Some(cur) = view.focused_tab_index() else {
            return;
        };
        let Some(next) = crate::app::tabs::cycle_tab_index(&pane_tabs, cur, forward) else {
            return;
        };
        view.set_pane_active(pane, next);
        view.scroll_tab_into_view(next);
        view.tab_menu = None;
        cx.notify();
    }

    /// Activate the tab at `index`: make it its pane's active tab and focus
    /// that pane (each strip shows only its own tabs, so a click never crosses).
    pub(crate) fn kv_activate_tab(
        &mut self,
        session: SessionId,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
        else {
            return;
        };
        let Some(pane) = view.tabs.get(index).map(|t| t.pane) else {
            return;
        };
        view.set_pane_active(pane, index);
        view.set_focused_pane(pane);
        view.tab_menu = None;
        cx.notify();
    }

    /// Close the tab at `index`: tear down its backend subscription (MONITOR /
    /// Pub-Sub / keyspace watcher ride an epoch that must be released), drop
    /// it, and restore the pane invariants. The last tab can't be closed — the
    /// shell always shows something (mirrors the SQL invariant).
    pub(crate) fn kv_close_tab(
        &mut self,
        session: SessionId,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
        else {
            return;
        };
        if index >= view.tabs.len() {
            return;
        }
        // Release any backend epoch this tab owned: a live subscription
        // (MONITOR / Pub-Sub / keyspace watcher) or an in-flight scan run
        // (browse cursor + biggest-keys sample, a running analysis walk).
        // `CloseResult` cancels the in-flight fetch at the engine too.
        let close_epochs: Vec<red_service::Epoch> = match &view.tabs[index].state {
            RedisTabState::Monitor(m) => vec![m.epoch],
            RedisTabState::PubSub(p) => vec![p.epoch],
            RedisTabState::Keyspace(k) => vec![k.epoch],
            RedisTabState::Browse(b) => {
                let mut v = vec![b.epoch];
                if let Some(bk) = &b.big_keys {
                    v.push(bk.epoch);
                }
                v
            }
            RedisTabState::Analysis(a) if a.running => vec![a.epoch],
            RedisTabState::Empty | RedisTabState::Analysis(_) | RedisTabState::Console(_) => {
                Vec::new()
            }
        };
        if view.tabs.len() <= 1 {
            // The view must always show a tab, so we can't remove the only one —
            // but we must still release its epoch, or a lone MONITOR/Pub-Sub/
            // keyspace tab would leak its firehose connection forever. Reset it
            // to the blank chooser in place and release below.
            view.tabs[index].state = RedisTabState::Empty;
            view.tabs[index].title = "New tab".to_string();
            view.tab_menu = None;
            for epoch in close_epochs {
                self.service
                    .send_to(session, Command::CloseResult { epoch });
            }
            cx.notify();
            return;
        }
        view.tabs.remove(index);
        // Shift every pane's stored active index past the removed slot, then let
        // `normalize_panes` drop panes this emptied and re-point the rest.
        view.layout
            .remap_active_tabs(|i| if i > index { i - 1 } else { i });
        view.tab_menu = None;
        view.normalize_panes();
        for epoch in close_epochs {
            self.service
                .send_to(session, Command::CloseResult { epoch });
        }
        cx.notify();
    }

    // --- drag reorder (mirrors the SQL `drop_tab` / drop-target helpers) ---

    /// Move the dragged tab (`from`) into `pane`'s strip and reorder it to the
    /// current drop-target gap. Clears the gap indicator.
    pub(crate) fn kv_drop_tab(
        &mut self,
        session: SessionId,
        from: usize,
        pane: PaneId,
        cx: &mut Context<Self>,
    ) {
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
        {
            view.reorder_tab(from, pane);
            view.scroll_tab_into_view(from);
            cx.notify();
        }
    }

    /// Track the cursor over `pane` during a tab drag and record which zone of it
    /// the tab would land in, so the pane paints the matching highlight.
    pub(crate) fn kv_aim_tab_drop(
        &mut self,
        session: SessionId,
        from: usize,
        pane: PaneId,
        bounds: gpui::Bounds<gpui::Pixels>,
        position: gpui::Point<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
        else {
            return;
        };
        let Some(dragged) = view.dragged_tab(from) else {
            return;
        };
        if crate::panes::aim(
            &mut view.layout,
            pane,
            bounds,
            position,
            Self::KV_PANE_LIMITS,
            dragged,
        ) {
            cx.notify();
        }
    }

    /// The zone a drop on `pane` resolves to, or `None` when the drop is refused:
    /// whatever the highlight settled on, a plain move when the drag never aimed
    /// at a zone, and nothing at all when it aimed at a split too small to be
    /// usable (which the muted highlight already said).
    pub(crate) fn kv_resolved_drop_zone(
        &mut self,
        session: SessionId,
        pane: PaneId,
        cx: &mut Context<Self>,
    ) -> Option<DropZone> {
        let target = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_ref())
            .and_then(|v| v.layout.drop_target())
            .filter(|t| t.pane == pane);
        match target {
            Some(t) if t.allowed => Some(t.zone),
            // The zone rendered muted, so the drop has to honour that: doing
            // something else instead is the same broken promise as offering a
            // split that never happens.
            Some(t) if t.zone != DropZone::Center => {
                if let Some(v) = self
                    .conn_mut(Some(session))
                    .and_then(|a| a.kv_view.as_mut())
                {
                    v.layout.clear_drag();
                }
                self.notify(
                    flint::ToastVariant::Info,
                    crate::i18n::tr!(
                        "panes.too_small_to_split",
                        "Not enough room to split that pane"
                    ),
                    cx,
                );
                None
            }
            _ => Some(DropZone::Center),
        }
    }

    /// Finish a tab drag dropped on a pane's body: into that pane, or into a new
    /// pane split off the edge the cursor was nearest.
    pub(crate) fn kv_drop_tab_on_pane(
        &mut self,
        session: SessionId,
        from: usize,
        pane: PaneId,
        zone: DropZone,
        cx: &mut Context<Self>,
    ) {
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
        {
            view.drop_tab_into(from, pane, zone);
            cx.notify();
        }
    }

    pub(crate) fn kv_set_tab_drop_target(
        &mut self,
        session: SessionId,
        pane: PaneId,
        gap: usize,
        cx: &mut Context<Self>,
    ) {
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            && view.set_drop_gap(pane, gap)
        {
            cx.notify();
        }
    }

    pub(crate) fn kv_clear_tab_drop_target(
        &mut self,
        session: SessionId,
        pane: PaneId,
        cx: &mut Context<Self>,
    ) {
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            && view.clear_drop_gap(pane)
        {
            cx.notify();
        }
    }

    // --- split panes ---

    /// Split the focused pane (⌘\ right / ⌘⇧\ down, routed here for a Redis
    /// connection): a fresh pane with a blank tab, which takes focus. The
    /// mechanics live in [`SplitWorkspace`], shared with the Mongo workspace;
    /// this wrapper only resolves the view and notifies.
    pub(crate) fn kv_split_pane(
        &mut self,
        session: SessionId,
        zone: DropZone,
        cx: &mut Context<Self>,
    ) {
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
        {
            view.split_focused(zone);
            cx.notify();
        }
    }

    /// Close `pane`, folding its tabs into the pane that takes its space.
    pub(crate) fn kv_close_pane(
        &mut self,
        session: SessionId,
        pane: PaneId,
        cx: &mut Context<Self>,
    ) {
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            && view.close_pane(pane)
        {
            cx.notify();
        }
    }

    /// Fold every pane back into one, keeping the focused pane's tabs on screen.
    pub(crate) fn kv_unsplit(&mut self, session: SessionId, cx: &mut Context<Self>) {
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            && view.unsplit()
        {
            cx.notify();
        }
    }

    /// Set the focused pane (a per-pane mouse-down picks this). No-op when
    /// unchanged.
    pub(crate) fn kv_set_split_focus(
        &mut self,
        session: SessionId,
        pane: PaneId,
        cx: &mut Context<Self>,
    ) {
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            && view.set_focused_pane(pane)
        {
            cx.notify();
        }
    }

    /// Move focus to the next pane in visual order (the ⌥⌘\ action).
    pub(crate) fn kv_focus_other_half(&mut self, session: SessionId, cx: &mut Context<Self>) {
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            && view.focus_pane_step(true)
        {
            cx.notify();
        }
    }

    /// Move the tab with `id` to the next pane (tab context menu), splitting off
    /// a new one when it is the only pane.
    pub(crate) fn kv_move_tab_to_other_half(
        &mut self,
        session: SessionId,
        id: u64,
        cx: &mut Context<Self>,
    ) {
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
        {
            view.split_move_tab(id);
            cx.notify();
        }
    }

    /// Pin/unpin the tab with `id` (pinned tabs sort ahead in their strip).
    pub(crate) fn kv_toggle_tab_pin(
        &mut self,
        session: SessionId,
        id: u64,
        cx: &mut Context<Self>,
    ) {
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            && let Some(idx) = view.tab_index_by_id(id)
        {
            view.toggle_pin_at(idx);
            view.tab_menu = None;
            cx.notify();
        }
    }

    /// Open / close the tab right-click context menu.
    pub(crate) fn kv_open_tab_menu(
        &mut self,
        session: SessionId,
        id: u64,
        pos: gpui::Point<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) {
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
        {
            view.tab_menu = Some((id, pos));
            cx.notify();
        }
    }

    pub(crate) fn kv_close_tab_menu(&mut self, session: SessionId, cx: &mut Context<Self>) {
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            && view.tab_menu.take().is_some()
        {
            cx.notify();
        }
    }

    /// Open the right-click context menu for a key row (from either the live
    /// browse list or the biggest-keys sample). The type/TTL are captured now so
    /// the menu labels itself and its actions target the exact key, independent
    /// of what the inspector currently shows.
    pub(crate) fn kv_open_key_menu(
        &mut self,
        session: SessionId,
        key: String,
        kv_type: KvType,
        ttl: Option<Duration>,
        pos: gpui::Point<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) {
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
        {
            view.tab_menu = None;
            view.key_menu = Some(KeyMenu {
                key,
                kv_type,
                ttl,
                pos,
            });
            cx.notify();
        }
    }

    pub(crate) fn kv_close_key_menu(&mut self, session: SessionId, cx: &mut Context<Self>) {
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            && view.key_menu.take().is_some()
        {
            cx.notify();
        }
    }

    /// Menu action: put `key` on the clipboard.
    pub(crate) fn kv_copy_key_name(
        &mut self,
        session: SessionId,
        key: String,
        cx: &mut Context<Self>,
    ) {
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(key));
        self.kv_close_key_menu(session, cx);
    }

    /// Menu action: open the inspector on `key`, then enter one of the inline
    /// editors (rename / TTL) or raise the delete-confirm bar — reusing the
    /// inspector's existing edit flows so the menu is a shortcut, not a second
    /// implementation. `action` selects which one.
    pub(crate) fn kv_key_menu_edit(
        &mut self,
        session: SessionId,
        key: String,
        kv_type: KvType,
        ttl: Option<Duration>,
        action: KeyMenuEdit,
        cx: &mut Context<Self>,
    ) {
        self.kv_close_key_menu(session, cx);
        self.kv_open_inspector(session, key, ttl, kv_type, cx);
        match action {
            KeyMenuEdit::Rename => self.kv_start_editing_key(session, cx),
            KeyMenuEdit::Ttl => self.kv_start_editing_ttl(session, cx),
        }
    }

    /// The other open Redis connections a key can be copied to: writable Redis
    /// sessions other than `source` (the foreground one plus every warm parked
    /// one), as `(session, name)`, sorted by name. Empty when there's nowhere to
    /// copy to.
    pub(crate) fn kv_copy_targets(&self, source: SessionId) -> Vec<(SessionId, String)> {
        let mut out = Vec::new();
        let mut consider = |a: &ActiveConn| {
            if a.session != source
                && a.config.kind == red_core::DbKind::Redis
                && !a.config.read_only
            {
                out.push((a.session, a.config.name.clone()));
            }
        };
        if let Phase::Connected(a) = &self.phase {
            consider(a);
        }
        for a in self.parked.values() {
            consider(a);
        }
        out.sort_by(|a, b| a.1.cmp(&b.1));
        out
    }

    /// Menu action: copy `key` from `source` to another open Redis connection
    /// (`DUMP` here → `RESTORE ... REPLACE` there, on the backend).
    pub(crate) fn kv_copy_key_to(
        &mut self,
        source: SessionId,
        key: String,
        target: SessionId,
        cx: &mut Context<Self>,
    ) {
        self.kv_close_key_menu(source, cx);
        self.service.send_to(
            source,
            Command::KvCopyKeys {
                keys: vec![key],
                target_session: target,
            },
        );
    }

    /// `Event::KvKeysCopied`: a cross-server copy finished. Toast the outcome.
    pub(crate) fn on_kv_keys_copied(&mut self, copied: u64, failed: u64, cx: &mut Context<Self>) {
        let (variant, msg) = if failed == 0 {
            (
                ToastVariant::Success,
                if copied == 1 {
                    crate::i18n::tr!("kv.copied_one_key", "Copied 1 key")
                } else {
                    crate::i18n::tr!("kv.copied_keys_ok", "Copied {copied} keys", copied = copied)
                },
            )
        } else {
            (
                ToastVariant::Warning,
                crate::i18n::tr!(
                    "kv.copied_keys",
                    "Copied {copied} key(s), {failed} failed",
                    copied = copied,
                    failed = failed,
                ),
            )
        };
        self.notify(variant, msg, cx);
    }

    /// Menu action: toggle the key's favorite star (persisted immediately). A
    /// favorited key shows a ★ in the browse list and tree.
    pub(crate) fn kv_toggle_key_favorite(
        &mut self,
        session: SessionId,
        key: String,
        cx: &mut Context<Self>,
    ) {
        if let Some(conn_id) = self.conn_mut(Some(session)).map(|a| a.conn_id.clone()) {
            self.redis_key_meta.toggle_favorite(&conn_id, &key);
        }
        self.kv_close_key_menu(session, cx);
        // Keep the favourites-only filter (if on) in sync with the new star state.
        self.kv_refresh_meta_snapshots(session, cx);
        cx.notify();
    }

    /// Menu action: open the "Note & tags" editor for `key`, seeded from the
    /// saved annotation (a floating popover, like the "New key" one).
    pub(crate) fn kv_open_annotations(
        &mut self,
        session: SessionId,
        key: String,
        cx: &mut Context<Self>,
    ) {
        self.kv_close_key_menu(session, cx);
        let conn_id = self.conn_mut(Some(session)).map(|a| a.conn_id.clone());
        let (note_text, tags_text) = conn_id
            .as_deref()
            .and_then(|c| self.redis_key_meta.get(c, &key))
            .map(|a| (a.note.clone(), a.tags.join(", ")))
            .unwrap_or_default();
        let note =
            cx.new(|cx| TextInput::new(cx).with_placeholder(crate::i18n::tr!("kv.note", "note…")));
        note.update(cx, |ti, cx| ti.set_content(note_text, cx));
        let tags = cx.new(|cx| {
            TextInput::new(cx).with_placeholder(crate::i18n::tr!(
                "kv.tags_comma_separated",
                "tags, comma-separated…"
            ))
        });
        tags.update(cx, |ti, cx| ti.set_content(tags_text, cx));
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
        {
            view.annotate = Some(AnnotateState { key, note, tags });
        }
        cx.notify();
    }

    /// Save the open annotation editor: note verbatim, tags split on commas
    /// (empties dropped), persisted. Favorite is untouched.
    pub(crate) fn kv_submit_annotations(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some((conn_id, key, note, tags)) = self.conn_mut(Some(session)).and_then(|a| {
            let conn_id = a.conn_id.clone();
            let ann = a.kv_view.as_ref()?.annotate.as_ref()?;
            let note = ann.note.read(cx).content().to_string();
            let tags = ann
                .tags
                .read(cx)
                .content()
                .split(',')
                .map(|t| t.to_string())
                .collect::<Vec<_>>();
            Some((conn_id, ann.key.clone(), note, tags))
        }) else {
            return;
        };
        self.redis_key_meta
            .set_note_tags(&conn_id, &key, note, tags);
        self.kv_cancel_annotations(session, cx);
        // The edited tags may change what the tag filter matches; resync it.
        self.kv_refresh_meta_snapshots(session, cx);
    }

    /// Close the annotation editor without saving.
    pub(crate) fn kv_cancel_annotations(&mut self, session: SessionId, cx: &mut Context<Self>) {
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
        {
            view.annotate = None;
        }
        cx.notify();
    }

    /// Menu action: ask to delete `key`. Unlike the inspector (which has its own
    /// inline confirm bar), a delete straight from the list opens a proper modal
    /// — the destructive action deserves an unmissable "are you sure?".
    pub(crate) fn kv_request_delete_key(
        &mut self,
        session: SessionId,
        key: String,
        cx: &mut Context<Self>,
    ) {
        self.kv_close_key_menu(session, cx);
        self.confirm_kv_delete = Some((session, key));
        self.focus_modal = true;
        cx.notify();
    }

    pub(crate) fn kv_cancel_delete_key(&mut self, cx: &mut Context<Self>) {
        if self.confirm_kv_delete.take().is_some() {
            self.refocus_root = true;
            cx.notify();
        }
    }

    /// The modal's Delete button: commit the `DEL` against the active browse's
    /// epoch (so [`Self::on_kv_edit_applied`] patches the right list) and close.
    pub(crate) fn kv_confirm_delete_key(&mut self, cx: &mut Context<Self>) {
        let Some((session, key)) = self.confirm_kv_delete.take() else {
            return;
        };
        let epoch = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_ref())
            .and_then(|v| v.active_browse())
            .map(|b| b.epoch);
        if let Some(epoch) = epoch {
            let edit = red_core::kv::KvEdit::Delete { keys: vec![key] };
            self.service
                .send_to(session, Command::KvApplyEdit { epoch, edit });
        }
        self.refocus_root = true;
        cx.notify();
    }

    /// Menu action: seed the Console with the natural read-all command for
    /// `key`'s type (never auto-run — the user reviews and presses Enter),
    /// reusing [`Self::kv_seed_console`].
    pub(crate) fn kv_key_menu_open_console(
        &mut self,
        session: SessionId,
        kv_type: KvType,
        key: String,
        cx: &mut Context<Self>,
    ) {
        let cmd = kv_read_command(&kv_type, &key);
        self.kv_close_key_menu(session, cx);
        self.kv_seed_console(session, cmd, cx);
    }

    /// Close the tab with `id` (the context menu's Close item; resolves the id
    /// to a current index first, since positions shift).
    pub(crate) fn kv_close_tab_by_id(
        &mut self,
        session: SessionId,
        id: u64,
        cx: &mut Context<Self>,
    ) {
        let idx = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_ref())
            .and_then(|v| v.tab_index_by_id(id));
        if let Some(idx) = idx {
            self.kv_close_tab(session, idx, cx);
        }
    }

    /// Bulk close from the tab context menu: Close Others / Close Left / Close
    /// Right / Close All, resolved against `id`'s own pane and skipping pinned
    /// tabs (mirrors the SQL side's [`AppState::close_tab_group`]). Targets are
    /// collected as stable ids first, then closed one by one so shifting indices
    /// stay valid; `kv_close_tab`'s "keep at least one tab" guard is respected.
    pub(crate) fn kv_close_tab_group(
        &mut self,
        session: SessionId,
        id: u64,
        scope: crate::app::TabCloseScope,
        cx: &mut Context<Self>,
    ) {
        use crate::app::TabCloseScope;
        if scope == TabCloseScope::One {
            self.kv_close_tab_by_id(session, id, cx);
            return;
        }
        let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_ref())
        else {
            return;
        };
        let Some(idx) = view.tab_index_by_id(id) else {
            return;
        };
        let pane = view.tabs[idx].pane;
        let siblings = view.pane_tab_indices(pane);
        let Some(pos) = siblings.iter().position(|&i| i == idx) else {
            return;
        };
        let target_indices: Vec<usize> = match scope {
            TabCloseScope::One => return,
            TabCloseScope::All => siblings.clone(),
            TabCloseScope::Others => siblings.iter().copied().filter(|&i| i != idx).collect(),
            TabCloseScope::Left => siblings[..pos].to_vec(),
            TabCloseScope::Right => siblings[pos + 1..].to_vec(),
        };
        // Resolve to stable ids now (indices shift as we close), skipping pinned
        // tabs — those close only via the explicit "Close" item.
        let target_ids: Vec<u64> = target_indices
            .into_iter()
            .filter(|&i| !view.tabs[i].pinned)
            .map(|i| view.tabs[i].id)
            .collect();
        for target in target_ids {
            self.kv_close_tab_by_id(session, target, cx);
        }
    }
}
