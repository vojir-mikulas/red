//! Mongo tab management, split out of `docbrowse/mod.rs` (mirrors
//! `kvbrowse/tabs.rs`): open/close/step/activate/drop/reorder collection tabs,
//! the split view and pin, and the tab context menu. A second `impl AppState`
//! block over the parent's state (`use super::*`).

use gpui::Context;
use red_service::{Command, SessionId};

use crate::app::{AppState, SplitHalf, SplitWorkspace, TabWorkspace};

use super::*;

impl AppState {
    /// Open a new blank tab in the focused half (the ＋ / ⌘T action). Its body
    /// shows the "pick a collection" hint; clicking a collection in the sidebar
    /// fills it. Mirrors the SQL side's `new_query` / Redis `kv_new_empty_tab`.
    pub(crate) fn doc_new_empty_tab(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_mut())
        else {
            return;
        };
        let half = view.focused_half();
        let id = view.tab_seq;
        view.tab_seq += 1;
        view.tabs.push(MongoTab {
            id,
            title: "New tab".to_string(),
            state: MongoTabState::Empty,
            pane: half,
            pinned: false,
        });
        let new_idx = view.tabs.len() - 1;
        view.set_pane_active(half, new_idx);
        view.tab_scroll.scroll_to_item(new_idx);
        cx.notify();
    }

    /// Open `db.coll` from the sidebar tree. `new_tab` (⌘-click) always opens a
    /// fresh, independent tab — so the same collection can live in several tabs at
    /// once (each with its own filter, paging, inspector, and unsaved edits, like
    /// the SQL/Redis shells). A plain click fills a focused blank tab, else focuses
    /// an already-open tab for the collection, else opens a new one.
    pub(crate) fn doc_open_collection(
        &mut self,
        session: SessionId,
        db: String,
        coll: String,
        new_tab: bool,
        cx: &mut Context<Self>,
    ) {
        // Whether the focused tab is a blank one we should fill in place.
        let focused_blank = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_ref())
            .map(|v| {
                v.tabs
                    .get(v.focused_tab_index())
                    .is_some_and(|t| matches!(t.state, MongoTabState::Empty))
            })
            .unwrap_or(false);

        // A plain click on an already-open collection focuses its tab rather than
        // thrashing a duplicate — unless we're forcing a new tab or filling a blank.
        if !new_tab && !focused_blank {
            let existing = self
                .conn_mut(Some(session))
                .and_then(|a| a.doc_view.as_ref())
                .and_then(|v| {
                    v.tabs.iter().position(|t| match &t.state {
                        MongoTabState::Collection(c) => c.db == db && c.coll == coll,
                        MongoTabState::Empty => false,
                    })
                });
            if let Some(idx) = existing {
                self.doc_activate_tab(session, idx, cx);
                return;
            }
        }
        // Build the collection's state (its own epoch) before borrowing the view,
        // and seed its first window (this also fetches the count). The grid's
        // load-on-scroll takes over from there.
        let epoch = crate::result::next_kv_epoch();
        let sender = self.service.command_sender(session);
        let coll_view = CollView::new(epoch, db.clone(), coll.clone(), sender, cx);
        coll_view.seed_browse();
        let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_mut())
        else {
            return;
        };
        let half = view.focused_half();
        let focused_idx = view.focused_tab_index();
        // Fill a focused blank tab (unless forcing a new tab), else append.
        let idx = if !new_tab
            && view
                .tabs
                .get(focused_idx)
                .is_some_and(|t| matches!(t.state, MongoTabState::Empty))
        {
            view.tabs[focused_idx].state = MongoTabState::Collection(Box::new(coll_view));
            view.tabs[focused_idx].title = coll.clone();
            focused_idx
        } else {
            let id = view.tab_seq;
            view.tab_seq += 1;
            view.tabs.push(MongoTab {
                id,
                title: coll.clone(),
                state: MongoTabState::Collection(Box::new(coll_view)),
                pane: half,
                pinned: false,
            });
            view.tabs.len() - 1
        };
        view.set_pane_active(half, idx);
        view.tab_scroll.scroll_to_item(idx);
        cx.notify();
    }

    /// Duplicate the tab `id` into a fresh, fully independent tab of the same
    /// collection, carrying over its applied filter. The duplicate has its own
    /// epoch, paging, inspector, and unsaved edits, so the two can be browsed and
    /// edited side by side (the context-menu path to "multiple tabs of one type").
    pub(crate) fn doc_duplicate_tab(
        &mut self,
        session: SessionId,
        id: u64,
        cx: &mut Context<Self>,
    ) {
        let src = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_ref())
            .and_then(|v| {
                v.tabs
                    .iter()
                    .find(|t| t.id == id)
                    .and_then(|t| match &t.state {
                        MongoTabState::Collection(c) => {
                            Some((c.db.clone(), c.coll.clone(), c.filter.clone()))
                        }
                        MongoTabState::Empty => None,
                    })
            });
        let Some((db, coll, filter)) = src else {
            return;
        };
        // Build the copy (its own epoch), seed its filter, then seed the browse
        // (which reads that filter) before borrowing the view.
        let epoch = crate::result::next_kv_epoch();
        let sender = self.service.command_sender(session);
        let mut coll_view = CollView::new(epoch, db.clone(), coll.clone(), sender, cx);
        coll_view.filter = filter.clone();
        if let Some(f) = &filter {
            coll_view
                .filter_input
                .update(cx, |input, cx| input.set_content(f.clone(), cx));
        }
        coll_view.seed_browse();
        let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_mut())
        else {
            return;
        };
        let half = view.focused_half();
        let tab_id = view.tab_seq;
        view.tab_seq += 1;
        view.tabs.push(MongoTab {
            id: tab_id,
            title: coll.clone(),
            state: MongoTabState::Collection(Box::new(coll_view)),
            pane: half,
            pinned: false,
        });
        let idx = view.tabs.len() - 1;
        view.set_pane_active(half, idx);
        view.tab_scroll.scroll_to_item(idx);
        cx.notify();
    }

    /// Step the focused half's active tab one slot forward/back, wrapping (the
    /// ctrl-tab / ctrl-shift-tab bindings). Shares the wrap math with the SQL/
    /// Redis sides via [`crate::app::tabs::cycle_tab_index`].
    pub(crate) fn doc_step_tab(
        &mut self,
        session: SessionId,
        forward: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_mut())
        else {
            return;
        };
        let half = view.focused_half();
        let pane_tabs = view.pane_tab_indices(half);
        let cur = view.focused_tab_index();
        let Some(next) = crate::app::tabs::cycle_tab_index(&pane_tabs, cur, forward) else {
            return;
        };
        view.set_pane_active(half, next);
        view.tab_scroll.scroll_to_item(next);
        view.tab_menu = None;
        cx.notify();
    }

    /// Activate the tab at `index`: make it its half's active tab and focus that
    /// half (each strip shows only its own tabs, so a click never crosses).
    pub(crate) fn doc_activate_tab(
        &mut self,
        session: SessionId,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_mut())
        else {
            return;
        };
        let Some(half) = view.tabs.get(index).map(|t| t.pane) else {
            return;
        };
        view.set_pane_active(half, index);
        if let Some(s) = &mut view.split {
            s.focus = half;
        }
        view.tab_menu = None;
        cx.notify();
    }

    /// Close the tab at `index`: release its collection's in-flight read epoch and
    /// drop it, restoring the pane invariants. The last tab can't be removed — it
    /// resets to a blank chooser (mirrors the SQL/Redis invariant).
    pub(crate) fn doc_close_tab(
        &mut self,
        session: SessionId,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_mut())
        else {
            return;
        };
        if index >= view.tabs.len() {
            return;
        }
        // A collection tab owns a browse epoch with a possibly in-flight find;
        // `CloseResult` cancels it at the engine.
        let close_epoch = match &view.tabs[index].state {
            MongoTabState::Collection(c) => Some(c.epoch),
            MongoTabState::Empty => None,
        };
        if view.tabs.len() <= 1 {
            // Keep at least one tab: reset the only tab to the blank chooser.
            view.tabs[index].state = MongoTabState::Empty;
            view.tabs[index].title = "New tab".to_string();
            view.tab_menu = None;
            if let Some(epoch) = close_epoch {
                self.service
                    .send_to(session, Command::CloseResult { epoch });
            }
            cx.notify();
            return;
        }
        view.tabs.remove(index);
        if view.active_tab > index {
            view.active_tab -= 1;
        }
        if let Some(s) = &mut view.split
            && s.secondary > index
        {
            s.secondary -= 1;
        }
        view.tab_menu = None;
        view.normalize_panes();
        if let Some(epoch) = close_epoch {
            self.service
                .send_to(session, Command::CloseResult { epoch });
        }
        cx.notify();
    }

    /// Close the tab with `id` (the context menu's Close item; resolves the id to
    /// a current index first, since positions shift).
    pub(crate) fn doc_close_tab_by_id(
        &mut self,
        session: SessionId,
        id: u64,
        cx: &mut Context<Self>,
    ) {
        let idx = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_ref())
            .and_then(|v| v.tab_index_by_id(id));
        if let Some(idx) = idx {
            self.doc_close_tab(session, idx, cx);
        }
    }

    /// Bulk close from the tab context menu: Close Others / Close Left / Close
    /// Right / Close All, resolved against `id`'s own pane and skipping pinned
    /// tabs (mirrors the Redis `kv_close_tab_group`).
    pub(crate) fn doc_close_tab_group(
        &mut self,
        session: SessionId,
        id: u64,
        scope: crate::app::TabCloseScope,
        cx: &mut Context<Self>,
    ) {
        use crate::app::TabCloseScope;
        if scope == TabCloseScope::One {
            self.doc_close_tab_by_id(session, id, cx);
            return;
        }
        let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_ref())
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
        // Resolve to stable ids now (indices shift as we close), skipping pinned.
        let target_ids: Vec<u64> = target_indices
            .into_iter()
            .filter(|&i| !view.tabs[i].pinned)
            .map(|i| view.tabs[i].id)
            .collect();
        for target in target_ids {
            self.doc_close_tab_by_id(session, target, cx);
        }
    }

    // --- drag reorder (mirrors the Redis drop helpers) ---

    /// Move the dragged tab (`from`) into `half` and reorder it to the current
    /// drop-target gap. Clears the gap indicator.
    pub(crate) fn doc_drop_tab(
        &mut self,
        session: SessionId,
        from: usize,
        half: SplitHalf,
        cx: &mut Context<Self>,
    ) {
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_mut())
        {
            view.reorder_tab(from, half);
            cx.notify();
        }
    }

    pub(crate) fn doc_set_tab_drop_target(
        &mut self,
        session: SessionId,
        gap: usize,
        cx: &mut Context<Self>,
    ) {
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_mut())
            && view.set_drop_target(gap)
        {
            cx.notify();
        }
    }

    pub(crate) fn doc_clear_tab_drop_target(&mut self, session: SessionId, cx: &mut Context<Self>) {
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_mut())
            && view.clear_drop_target()
        {
            cx.notify();
        }
    }

    // --- split panes ---

    /// Toggle the side-by-side split (⌘\, routed here for a Mongo connection).
    /// The split mechanics live in [`SplitWorkspace`], shared with the Redis
    /// workspace; this wrapper only resolves the view and notifies.
    pub(crate) fn doc_toggle_split(&mut self, session: SessionId, cx: &mut Context<Self>) {
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_mut())
        {
            view.split_toggle();
            cx.notify();
        }
    }

    /// Set the focused half (a per-half mouse-down picks this). No-op when not
    /// split or unchanged.
    pub(crate) fn doc_set_split_focus(
        &mut self,
        session: SessionId,
        half: SplitHalf,
        cx: &mut Context<Self>,
    ) {
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_mut())
            && view.split_set_focus(half)
        {
            cx.notify();
        }
    }

    /// Move focus to the other half (the ⌥⌘\ action). No-op when not split.
    pub(crate) fn doc_focus_other_half(&mut self, session: SessionId, cx: &mut Context<Self>) {
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_mut())
            && view.split_focus_other()
        {
            cx.notify();
        }
    }

    /// Move the tab with `id` to the other split half (tab context menu). If not
    /// split, opens the split first so there's a half to move to.
    pub(crate) fn doc_move_tab_to_other_half(
        &mut self,
        session: SessionId,
        id: u64,
        cx: &mut Context<Self>,
    ) {
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_mut())
        {
            view.split_move_tab(id);
            cx.notify();
        }
    }

    /// Pin/unpin the tab with `id` (pinned tabs sort ahead in their strip).
    pub(crate) fn doc_toggle_tab_pin(
        &mut self,
        session: SessionId,
        id: u64,
        cx: &mut Context<Self>,
    ) {
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_mut())
            && let Some(idx) = view.tab_index_by_id(id)
        {
            view.toggle_pin_at(idx);
            view.tab_menu = None;
            cx.notify();
        }
    }

    /// Open / close the tab right-click context menu.
    pub(crate) fn doc_open_tab_menu(
        &mut self,
        session: SessionId,
        id: u64,
        pos: gpui::Point<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) {
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_mut())
        {
            view.tab_menu = Some((id, pos));
            cx.notify();
        }
    }

    pub(crate) fn doc_close_tab_menu(&mut self, session: SessionId, cx: &mut Context<Self>) {
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_mut())
            && view.tab_menu.take().is_some()
        {
            cx.notify();
        }
    }

    /// Open / close the documents toolbar's "Actions" dropdown.
    pub(crate) fn doc_open_actions_menu(
        &mut self,
        session: SessionId,
        pos: gpui::Point<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) {
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_mut())
        {
            view.actions_menu = Some(pos);
            cx.notify();
        }
    }

    pub(crate) fn doc_close_actions_menu(&mut self, session: SessionId, cx: &mut Context<Self>) {
        if let Some(view) = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_mut())
            && view.actions_menu.take().is_some()
        {
            cx.notify();
        }
    }

    /// ⌘R: reload the databases list and re-seed the focused collection's browse
    /// (back to the first window; the count is re-read too).
    pub(crate) fn doc_refresh(&mut self, session: SessionId, cx: &mut Context<Self>) {
        self.doc_start_browse(session, cx);
        if let Some(c) = self
            .conn_mut(Some(session))
            .and_then(|a| a.doc_view.as_mut())
            .and_then(|v| v.focused_coll_mut())
        {
            c.loading = true;
            c.inspector = None;
            c.inspector_doc = None;
            c.expanded_rows.clear();
            c.seed_browse();
        }
        cx.notify();
    }
}
