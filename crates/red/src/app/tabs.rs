//! Query-tab management: the tab strip's activate / drag-reorder / close
//! lifecycle, opening blank tabs, the close-with-unsaved-work confirmation,
//! and the per-tab table-detail prefetch. Split out of `mod.rs`.

use super::*;

/// Next global tab index cycling within `pane_tabs` from `cur`, wrapping at the
/// ends. `None` when there's nothing to switch to (≤1 tab). Shared by the SQL
/// (`step_active_tab`) and Redis (`kv_step_tab`) tab-cycling so the wrap math
/// lives in one place.
pub(crate) fn cycle_tab_index(pane_tabs: &[usize], cur: usize, forward: bool) -> Option<usize> {
    if pane_tabs.len() <= 1 {
        return None;
    }
    let pos = pane_tabs.iter().position(|&g| g == cur).unwrap_or(0);
    let n = pane_tabs.len();
    Some(if forward {
        pane_tabs[(pos + 1) % n]
    } else {
        pane_tabs[(pos + n - 1) % n]
    })
}

impl AppState {
    // --- query tabs ---

    /// Focus tab `index`. Its editor and result become the visible ones.
    pub(crate) fn set_active_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Phase::Connected(active) = &mut self.phase
            && index < active.tabs.len()
        {
            active.set_focused_tab(index);
            // Selecting a partly off-screen tab scrolls it fully into view.
            active.scroll_tab_into_view(index);
        }
        cx.notify();
    }

    /// Point the drop indicator at `gap` (an insertion index `0..=tabs.len()`)
    /// in `pane`'s strip while a tab drag hovers it. Notifies only on change to
    /// keep the per-move churn cheap.
    pub(crate) fn set_tab_drop_target(&mut self, pane: PaneId, gap: usize, cx: &mut Context<Self>) {
        if let Phase::Connected(active) = &mut self.phase
            && active.set_drop_gap(pane, gap)
        {
            cx.notify();
        }
    }

    /// Drop the drop indicator (cursor left the tab strip mid-drag). Notifies
    /// only when something was showing.
    pub(crate) fn clear_tab_drop_target(&mut self, pane: PaneId, cx: &mut Context<Self>) {
        if let Phase::Connected(active) = &mut self.phase
            && active.clear_drop_gap(pane)
        {
            cx.notify();
        }
    }

    /// Finish a tab-strip drag onto `pane`'s strip: assign the dragged tab
    /// (`from`) to that pane and move it into the gap the indicator settled on.
    /// Focuses the pane; `normalize_panes` drops the source pane if the drag
    /// emptied it. Clears the indicator regardless.
    pub(crate) fn drop_tab(&mut self, from: usize, pane: PaneId, cx: &mut Context<Self>) {
        if let Phase::Connected(active) = &mut self.phase {
            active.reorder_tab(from, pane);
            active.scroll_tab_into_view(from);
        }
        cx.notify();
    }

    /// Track the cursor over `pane` during a tab drag and record which zone of it
    /// the tab would land in, so the pane paints the matching highlight.
    pub(crate) fn aim_tab_drop(
        &mut self,
        from: usize,
        pane: PaneId,
        bounds: gpui::Bounds<gpui::Pixels>,
        position: gpui::Point<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) {
        let Phase::Connected(active) = &mut self.phase else {
            return;
        };
        let Some(dragged) = active.dragged_tab(from) else {
            return;
        };
        if crate::panes::aim(
            &mut active.layout,
            pane,
            bounds,
            position,
            Self::PANE_LIMITS,
            dragged,
        ) {
            cx.notify();
        }
    }

    /// The zone a drop on `pane` resolves to, or `None` when the drop is refused:
    /// whatever the highlight settled on, a plain move when the drag never aimed
    /// at a zone, and nothing at all when it aimed at a split too small to be
    /// usable (which the muted highlight already said).
    pub(crate) fn resolved_drop_zone(
        &mut self,
        pane: PaneId,
        cx: &mut Context<Self>,
    ) -> Option<DropZone> {
        let target = match &self.phase {
            Phase::Connected(active) => active.layout.drop_target().filter(|t| t.pane == pane),
            _ => None,
        };
        match target {
            Some(t) if t.allowed => Some(t.zone),
            // The zone rendered muted, so the drop has to honour that: doing
            // something else instead is the same broken promise as offering a
            // split that never happens.
            Some(t) if t.zone != DropZone::Center => {
                if let Phase::Connected(active) = &mut self.phase {
                    active.layout.clear_drag();
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

    /// Finish a tab drag dropped on a pane's *body*: into that pane, or into a
    /// new pane split off the edge the cursor was nearest (which is how a
    /// single-pane workspace gets its second pane). No-op when nothing changed.
    pub(crate) fn drop_tab_on_pane(
        &mut self,
        from: usize,
        pane: PaneId,
        zone: crate::panes::DropZone,
        cx: &mut Context<Self>,
    ) {
        let moved = match &mut self.phase {
            Phase::Connected(active) => active.drop_tab_into(from, pane, zone),
            _ => return,
        };
        if moved {
            // Land focus in the pane the tab moved to on the next paint.
            self.pending_focus = Some(Pane::Editor);
        }
        cx.notify();
    }

    /// Push a freshly-built tab, focus it, and seed its completions. Returns the
    /// new index. Callers supply the tab (a blank query or a table preview).
    /// Eagerly describe every table once the skeleton lands, so column and
    /// `table.` completion covers the whole schema without the user expanding
    /// each node first. Details arrive as `TableDescribed` events that refresh the
    /// completion index. Capped so a pathological schema can't flood the backend;
    /// past the cap, tables still load lazily on tree expansion.
    pub(crate) fn prefetch_table_details(&mut self, cx: &mut Context<Self>) {
        const MAX_PREFETCH: usize = 200;
        let pending: Vec<(String, String)> = match &self.phase {
            Phase::Connected(active) => {
                let s = active.schema.read(cx);
                s.schemas
                    .iter()
                    .flat_map(|sc| {
                        sc.objects
                            .iter()
                            .map(move |obj| (sc.name.clone(), obj.name.clone()))
                    })
                    .filter(|key| !s.details.contains_key(key))
                    .take(MAX_PREFETCH)
                    .collect()
            }
            _ => return,
        };
        for (schema, table) in pending {
            self.send_active(Command::DescribeTable { schema, table });
        }
    }

    pub(crate) fn push_tab(&mut self, mut tab: QueryTab, cx: &mut Context<Self>) -> usize {
        let index = match &mut self.phase {
            Phase::Connected(active) => {
                // The new tab joins the focused pane and becomes its active tab.
                let pane = active.focused_pane();
                tab.pane = pane;
                active.tabs.push(tab);
                let index = active.tabs.len() - 1;
                active.set_pane_active(pane, index);
                // Scroll the freshly-focused tab into view on the next paint, in
                // case the strip was already scrolled or crowded.
                active.scroll_tab_into_view(index);
                index
            }
            _ => return 0,
        };
        // New editor needs the current schema's completion candidates installed.
        self.refresh_completions(cx);
        index
    }

    /// Push `tab` into the connection that owns `session`, even when that
    /// connection is parked (not on screen). Used for a background job's result
    /// (a schema diff) that must land in the connection that *asked* for it, not
    /// whatever happens to be foreground when the reply arrives — otherwise its
    /// "Open script" would generate DDL against the wrong server. Foreground
    /// session falls through to the focus-changing [`push_tab`](Self::push_tab).
    pub(crate) fn push_tab_to(
        &mut self,
        session: Option<SessionId>,
        mut tab: QueryTab,
        cx: &mut Context<Self>,
    ) {
        if session.is_none() || session == self.foreground_session {
            self.push_tab(tab, cx);
            return;
        }
        // A parked connection: append without touching focus. The user sees it
        // when they switch back; `set_pane_active` binds it as that half's tab.
        if let Some(active) = self.conn_mut(session) {
            let pane = active.focused_pane();
            tab.pane = pane;
            active.tabs.push(tab);
            let index = active.tabs.len() - 1;
            active.set_pane_active(pane, index);
        }
        cx.notify();
    }

    /// Focus the next query tab, wrapping past the end. No-op with one tab.
    pub(crate) fn next_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.cycle_tab(true, window, cx);
    }

    /// Focus the previous query tab, wrapping past the start. No-op with one tab.
    pub(crate) fn prev_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.cycle_tab(false, window, cx);
    }

    /// Step the active tab one slot forward (`forward`) or back, wrapping at the
    /// ends. No-op with one tab. Each tab owns its own editor entity, so keyboard
    /// focus must follow the switch: when the outgoing tab's editor held focus we
    /// hand it to the incoming one, otherwise cycling from a focused editor would
    /// strand focus on the now-hidden editor and the next keystroke would go
    /// nowhere. The palette path has no `Window` to move focus, but it cycles
    /// from the palette (not the editor), so there's nothing to follow.
    fn cycle_tab(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        // A Redis connection has no editor to chase focus for; step its tabs.
        if let Phase::Connected(a) = &self.phase
            && a.kv_view.is_some()
        {
            let session = a.session;
            self.kv_step_tab(session, forward, cx);
            return;
        }
        // A Mongo connection likewise steps its own tabs (no SQL editor focus).
        if let Phase::Connected(a) = &self.phase
            && a.doc_view.is_some()
        {
            let session = a.session;
            self.doc_step_tab(session, forward, cx);
            return;
        }
        let editor_focused = matches!(
            &self.phase,
            Phase::Connected(active)
                if active.active().is_some_and(|t| t.editor.focus_handle(cx).contains_focused(window, cx))
        );
        if !self.step_active_tab(forward, cx) || !editor_focused {
            return;
        }
        if let Phase::Connected(active) = &self.phase
            && let Some(handle) = active.active().map(|t| t.editor.focus_handle(cx))
        {
            window.focus(&handle, cx);
        }
    }

    /// Advance the active tab one slot (`forward` else back, wrapping); the pure
    /// selection move shared by the keyboard and palette paths. Returns whether a
    /// switch happened (false with ≤1 tab or outside the connected shell), so the
    /// keyboard path knows when to chase focus.
    pub(crate) fn step_active_tab(&mut self, forward: bool, cx: &mut Context<Self>) -> bool {
        if let Phase::Connected(active) = &mut self.phase {
            // Cycle within the focused pane's own tabs (each pane has its own set).
            let pane = active.focused_pane();
            let pane_tabs = active.pane_tab_indices(pane);
            let Some(cur) = active.focused_tab_index() else {
                return false;
            };
            let Some(next) = cycle_tab_index(&pane_tabs, cur, forward) else {
                return false;
            };
            active.set_focused_tab(next);
            active.scroll_tab_into_view(next);
            cx.notify();
            return true;
        }
        false
    }

    /// Close the focused tab (the ⌘W binding); routes through the same
    /// pristine-or-confirm path as the tab's × button. No-op with no open tab.
    pub(crate) fn close_active_tab(&mut self, cx: &mut Context<Self>) {
        // Redis: close the focused tab of the focused half.
        if let Phase::Connected(a) = &self.phase
            && let Some(v) = &a.kv_view
        {
            let Some(idx) = v.focused_tab_index() else {
                return;
            };
            let session = a.session;
            self.kv_close_tab(session, idx, cx);
            return;
        }
        // Mongo: close the focused collection/blank tab of the focused half.
        if let Phase::Connected(a) = &self.phase
            && let Some(v) = &a.doc_view
        {
            let Some(idx) = v.focused_tab_index() else {
                return;
            };
            let session = a.session;
            self.doc_close_tab(session, idx, cx);
            return;
        }
        let index = match &self.phase {
            Phase::Connected(active) => match active.focused_tab_index() {
                Some(i) => i,
                None => return,
            },
            _ => return,
        };
        self.request_close_tab(index, cx);
    }

    /// Reload the schema tree from the backend (the ⌘R binding / palette command).
    ///
    /// Also drops the lazily-loaded object groups, so a refresh means the whole
    /// tree and not just its skeleton. They re-fetch on the next expand; any
    /// group currently open re-requests itself through `flatten`'s loading path.
    pub(crate) fn refresh_schema(&mut self, cx: &mut Context<Self>) {
        let mut reload: Vec<(String, red_core::ObjectKind)> = Vec::new();
        if let Phase::Connected(active) = &self.phase {
            let tree = active.schema.clone();
            tree.update(cx, |s, _| {
                s.groups.clear();
                s.groups_loading.clear();
                for node in &s.expanded {
                    if let crate::schema::NodeId::Group { schema, kind } = node
                        && kind.is_lazy()
                    {
                        reload.push((schema.clone(), *kind));
                    }
                }
                s.groups_loading.extend(reload.iter().cloned());
            });
        }
        self.send_active(Command::LoadObjects);
        self.send_active(Command::LoadObjectCounts);
        for (namespace, kind) in reload {
            self.send_active(Command::LoadObjectGroup { namespace, kind });
        }
    }

    /// ⌘R: refresh whatever the active connection shows — a Redis key browse
    /// re-scans its keyspace, a SQL connection reloads its schema objects.
    pub(crate) fn refresh_active(&mut self, cx: &mut Context<Self>) {
        if let Phase::Connected(a) = &self.phase
            && a.kv_view.is_some()
        {
            let session = a.session;
            self.kv_refresh_keys(session, cx);
            return;
        }
        if let Phase::Connected(a) = &self.phase
            && a.doc_view.is_some()
        {
            let session = a.session;
            self.doc_refresh(session, cx);
            return;
        }
        self.refresh_schema(cx);
    }

    /// Open a blank query tab (the tab-strip "＋" action).
    pub(crate) fn new_query(&mut self, cx: &mut Context<Self>) {
        // A Redis connection has no SQL editor; ⌘T opens a blank Redis tab.
        if let Phase::Connected(a) = &self.phase
            && a.kv_view.is_some()
        {
            let session = a.session;
            self.kv_new_empty_tab(session, cx);
            return;
        }
        // A Mongo connection likewise has no SQL editor; ⌘T opens a blank tab.
        if let Phase::Connected(a) = &self.phase
            && a.doc_view.is_some()
        {
            let session = a.session;
            self.doc_new_empty_tab(session, cx);
            return;
        }
        let dialect = self.active_dialect();
        let tab = match &mut self.phase {
            Phase::Connected(active) => {
                active.query_seq += 1;
                QueryTab::new(format!("query {}", active.query_seq), dialect, cx)
            }
            _ => return,
        };
        self.push_tab(tab, cx);
        // Focus the new tab's editor on the next paint (this path has no `Window`,
        // and the palette path likewise routes focus through render).
        self.pending_focus = Some(Pane::Editor);
        cx.notify();
    }

    /// The tab-strip "×" (and middle-click): close immediately if the tab holds
    /// nothing to lose (pristine, or a diagram) or the user opted out of the
    /// confirmation, else ask first.
    pub(crate) fn request_close_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        let confirm = match &self.phase {
            Phase::Connected(active) => active
                .tabs
                .get(index)
                .is_some_and(|t| t.needs_close_confirm(cx)),
            _ => return,
        };
        if !confirm || !self.settings.safety.confirm_close_tab {
            self.close_many(vec![index], cx);
        } else {
            self.confirm_close_tab = Some(index);
            // Focus the modal so its own Enter/Esc handling is heard.
            self.focus_modal = true;
            cx.notify();
        }
    }

    /// Confirmation accepted: close the tab that was awaiting it.
    pub(crate) fn confirm_close(&mut self, cx: &mut Context<Self>) {
        if let Some(index) = self.confirm_close_tab.take() {
            self.close_many(vec![index], cx);
        }
        self.refocus_root = true;
        cx.notify();
    }

    pub(crate) fn cancel_close(&mut self, cx: &mut Context<Self>) {
        self.confirm_close_tab = None;
        self.refocus_root = true;
        cx.notify();
    }

    /// Request closing several tabs at once (the context menu's Close Others /
    /// Close All / Close Left / Close Right). Closes immediately when every
    /// target is pristine or the user opted out of the confirmation; otherwise
    /// asks once for the whole batch.
    pub(crate) fn request_close_many(&mut self, indices: Vec<usize>, cx: &mut Context<Self>) {
        if indices.is_empty() {
            return;
        }
        let any_dirty = match &self.phase {
            Phase::Connected(active) => indices.iter().any(|&i| {
                active
                    .tabs
                    .get(i)
                    .is_some_and(|t| t.needs_close_confirm(cx))
            }),
            _ => return,
        };
        if any_dirty && self.settings.safety.confirm_close_tab {
            self.confirm_close_batch = Some(indices);
            self.focus_modal = true;
            cx.notify();
        } else {
            self.close_many(indices, cx);
        }
    }

    /// Batch confirmation accepted: close the tabs that were awaiting it.
    pub(crate) fn confirm_close_batch_accept(&mut self, cx: &mut Context<Self>) {
        if let Some(indices) = self.confirm_close_batch.take() {
            self.close_many(indices, cx);
        }
        self.refocus_root = true;
        cx.notify();
    }

    pub(crate) fn cancel_close_batch(&mut self, cx: &mut Context<Self>) {
        self.confirm_close_batch = None;
        self.refocus_root = true;
        cx.notify();
    }

    /// The tab-strip right-click menu's Close / Close Others / Close All / Close
    /// Left / Close Right, resolved against `index`'s own pane and skipping
    /// pinned tabs (pinned tabs only close via the explicit "Close" item).
    pub(crate) fn close_tab_group(
        &mut self,
        index: usize,
        scope: TabCloseScope,
        cx: &mut Context<Self>,
    ) {
        let Phase::Connected(active) = &self.phase else {
            return;
        };
        if scope == TabCloseScope::One {
            self.request_close_tab(index, cx);
            return;
        }
        let Some(pane) = active.tabs.get(index).map(|t| t.pane) else {
            return;
        };
        let siblings = active.pane_tab_indices(pane);
        let Some(pos) = siblings.iter().position(|&i| i == index) else {
            return;
        };
        let targets: Vec<usize> = match scope {
            // `One` is handled by the early return above, so it never reaches
            // this match.
            TabCloseScope::One => unreachable!("TabCloseScope::One handled before match"),
            TabCloseScope::All => siblings.clone(),
            TabCloseScope::Others => siblings.iter().copied().filter(|&i| i != index).collect(),
            TabCloseScope::Left => siblings[..pos].to_vec(),
            TabCloseScope::Right => siblings[pos + 1..].to_vec(),
        };
        let targets: Vec<usize> = targets
            .into_iter()
            .filter(|&i| !active.tabs[i].pinned)
            .collect();
        self.request_close_many(targets, cx);
    }

    /// Pin/unpin tab `index` (the tab-strip context menu's Pin item). A pinned
    /// tab renders in a fixed section at the start of the strip, always visible
    /// regardless of scroll, and is skipped by the bulk close actions.
    pub(crate) fn toggle_tab_pin(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Phase::Connected(active) = &mut self.phase {
            active.toggle_pin_at(index);
        }
        cx.notify();
    }

    /// Drop the tabs at `indices`, freeing each closed tab's backend result.
    /// Closing every open tab is allowed: the strip goes empty and the shell
    /// shows a placeholder pane (the connection stays open, and the strip's ＋
    /// opens a fresh query).
    fn close_many(&mut self, mut indices: Vec<usize>, cx: &mut Context<Self>) {
        self.confirm_close_tab = None;
        self.confirm_close_batch = None;
        // Remove back-to-front so earlier indices in the batch stay valid.
        indices.sort_unstable();
        indices.dedup();
        indices.reverse();
        let mut free_epochs = Vec::new();
        if let Phase::Connected(active) = &mut self.phase {
            for index in indices {
                if index >= active.tabs.len() {
                    continue;
                }
                let removed = active.tabs.remove(index);
                // Shift every pane's active index left when it sat after the
                // removed tab; `normalize_panes` then drops panes this emptied and
                // re-points each remaining one at a tab it owns.
                active
                    .layout
                    .remap_active_tabs(|i| if i >= index && i > 0 { i - 1 } else { i });
                if let Some(g) = removed.result {
                    free_epochs.push(g.epoch);
                }
            }
            active.normalize_panes();
        } else {
            return;
        }
        // Free the backend results that backed the closed tabs' grids.
        for epoch in free_epochs {
            self.send_active(Command::CloseResult { epoch });
        }
        cx.notify();
    }
}
