//! Query-tab management: the tab strip's activate / drag-reorder / close
//! lifecycle, opening blank tabs, the close-with-unsaved-work confirmation,
//! and the per-tab table-detail prefetch. Split out of `mod.rs`.

use super::*;

impl AppState {
    // --- query tabs ---

    /// Focus tab `index`. Its editor and result become the visible ones.
    pub(crate) fn set_active_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Phase::Connected(active) = &mut self.phase {
            if index < active.tabs.len() {
                // Load the tab into the focused half; when split, picking the tab the
                // other half already shows swaps the two (see `set_focused_tab`).
                active.set_focused_tab(index);
                // Selecting a partly off-screen tab scrolls it fully into view.
                active.tab_scroll.scroll_to_item(index);
            }
        }
        cx.notify();
    }

    /// Point the drop indicator at `gap` (an insertion index `0..=tabs.len()`)
    /// while a tab drag hovers the strip. Notifies only on change to keep the
    /// per-move churn cheap.
    pub(crate) fn set_tab_drop_target(&mut self, gap: usize, cx: &mut Context<Self>) {
        if let Phase::Connected(active) = &mut self.phase {
            if active.tab_drop_target != Some(gap) {
                active.tab_drop_target = Some(gap);
                cx.notify();
            }
        }
    }

    /// Drop the drop indicator (cursor left the tab strip mid-drag). Notifies
    /// only when something was showing.
    pub(crate) fn clear_tab_drop_target(&mut self, cx: &mut Context<Self>) {
        if let Phase::Connected(active) = &mut self.phase {
            if active.tab_drop_target.take().is_some() {
                cx.notify();
            }
        }
    }

    /// Finish a tab-strip drag onto half `half`'s strip: assign the dragged tab
    /// (`from`) to that half and move it into the gap the indicator settled on. Lands
    /// the tab in `half` and focuses it; `normalize_panes` collapses the split if the
    /// drag emptied the source half. Clears the indicator regardless.
    pub(crate) fn drop_tab(&mut self, from: usize, half: SplitHalf, cx: &mut Context<Self>) {
        if let Phase::Connected(active) = &mut self.phase {
            if let Some(gap) = active.tab_drop_target.take() {
                if from < active.tabs.len() {
                    // The dragged tab now belongs to the strip it was dropped on.
                    active.tabs[from].pane = half;
                    // `gap` indexes the pre-removal strip; shift left when the
                    // dragged tab sat before the gap.
                    let dest = if from < gap { gap - 1 } else { gap };
                    let dest = dest.min(active.tabs.len() - 1);
                    let tab = active.tabs.remove(from);
                    active.tabs.insert(dest, tab);
                    // Remap the other pane's active index across remove(from)+insert(dest)
                    // so it keeps pointing at its tab; the dropped half is aimed at
                    // the moved tab (its new home, `dest`), and `normalize_panes` then
                    // repairs anything stale (and collapses an emptied half).
                    let remap = |idx: usize| -> usize {
                        if idx == from {
                            return dest;
                        }
                        let j = if idx > from { idx - 1 } else { idx };
                        if j >= dest {
                            j + 1
                        } else {
                            j
                        }
                    };
                    active.active_tab = remap(active.active_tab);
                    if let Some(s) = &mut active.split {
                        s.secondary = remap(s.secondary);
                        s.focus = half;
                    }
                    active.set_pane_active(half, dest);
                    active.normalize_panes();
                }
            }
        }
        cx.notify();
    }

    /// Move tab `from` into split half `half` and focus that half: the drop target
    /// for dragging a tab across the divider onto a half's body (no reorder). Lands
    /// the tab in `half`; `normalize_panes` collapses the split if it emptied the
    /// source half. No-op when not split or `from` is stale.
    pub(crate) fn move_tab_to_half(
        &mut self,
        from: usize,
        half: SplitHalf,
        cx: &mut Context<Self>,
    ) {
        if let Phase::Connected(active) = &mut self.phase {
            if active.split.is_none() || from >= active.tabs.len() {
                return;
            }
            active.tabs[from].pane = half;
            if let Some(s) = &mut active.split {
                s.focus = half;
            }
            active.set_pane_active(half, from);
            active.tab_scroll.scroll_to_item(from);
            active.tab_drop_target = None;
            active.normalize_panes();
        } else {
            return;
        }
        // Land focus in the half the tab moved to on the next paint.
        self.pending_focus = Some(Pane::Editor);
        cx.notify();
    }

    /// Push a freshly-built tab, focus it, and seed its completions. Returns the
    /// new index. Callers supply the tab (a blank query or a table preview).
    /// Eagerly describe every table once the skeleton lands, so column and
    /// `table.` completion covers the whole schema without the user expanding
    /// each node first. Details arrive as `TableDescribed` events that refresh the
    /// completion index. Capped so a pathological schema can't flood the backend;
    /// past the cap, tables still load lazily on tree expansion.
    pub(crate) fn prefetch_table_details(&mut self) {
        const MAX_PREFETCH: usize = 200;
        let pending: Vec<(String, String)> = match &self.phase {
            Phase::Connected(active) => {
                let s = &active.schema;
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
                // The new tab joins the focused half and becomes its active tab.
                let half = active.focused_half();
                tab.pane = half;
                active.tabs.push(tab);
                let index = active.tabs.len() - 1;
                active.set_pane_active(half, index);
                // Scroll the freshly-focused tab into view on the next paint, in
                // case the strip was already scrolled or crowded.
                active.tab_scroll.scroll_to_item(index);
                index
            }
            _ => return 0,
        };
        // New editor needs the current schema's completion candidates installed.
        self.refresh_completions(cx);
        index
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
        let editor_focused = matches!(
            &self.phase,
            Phase::Connected(active)
                if active.active().is_some_and(|t| t.editor.focus_handle(cx).contains_focused(window, cx))
        );
        if !self.step_active_tab(forward, cx) || !editor_focused {
            return;
        }
        if let Phase::Connected(active) = &self.phase {
            if let Some(handle) = active.active().map(|t| t.editor.focus_handle(cx)) {
                window.focus(&handle, cx);
            }
        }
    }

    /// Advance the active tab one slot (`forward` else back, wrapping); the pure
    /// selection move shared by the keyboard and palette paths. Returns whether a
    /// switch happened (false with ≤1 tab or outside the connected shell), so the
    /// keyboard path knows when to chase focus.
    pub(crate) fn step_active_tab(&mut self, forward: bool, cx: &mut Context<Self>) -> bool {
        if let Phase::Connected(active) = &mut self.phase {
            // Cycle within the focused pane's own tabs (each half has its own set).
            let half = active.focused_half();
            let pane_tabs = active.pane_tab_indices(half);
            if pane_tabs.len() <= 1 {
                return false;
            }
            let cur = active.focused_tab_index();
            let pos = pane_tabs.iter().position(|&g| g == cur).unwrap_or(0);
            let n = pane_tabs.len();
            let next = if forward {
                pane_tabs[(pos + 1) % n]
            } else {
                pane_tabs[(pos + n - 1) % n]
            };
            active.set_focused_tab(next);
            active.tab_scroll.scroll_to_item(next);
            cx.notify();
            return true;
        }
        false
    }

    /// Close the focused tab (the ⌘W binding); routes through the same
    /// pristine-or-confirm path as the tab's × button. No-op with no open tab.
    pub(crate) fn close_active_tab(&mut self, cx: &mut Context<Self>) {
        let index = match &self.phase {
            Phase::Connected(active) if active.active().is_some() => active.active_tab,
            _ => return,
        };
        self.request_close_tab(index, cx);
    }

    /// Reload the schema tree from the backend (the ⌘R binding / palette command).
    pub(crate) fn refresh_schema(&mut self) {
        self.send_active(Command::LoadObjects);
    }

    /// Open a blank query tab (the tab-strip "＋" action).
    pub(crate) fn new_query(&mut self, cx: &mut Context<Self>) {
        let tab = match &mut self.phase {
            Phase::Connected(active) => {
                active.query_seq += 1;
                QueryTab::new(format!("query {}", active.query_seq), cx)
            }
            _ => return,
        };
        self.push_tab(tab, cx);
        // Focus the new tab's editor on the next paint (this path has no `Window`,
        // and the palette path likewise routes focus through render).
        self.pending_focus = Some(Pane::Editor);
        cx.notify();
    }

    /// The tab-strip "×" (and middle-click): close immediately if pristine or the
    /// user opted out of the confirmation, else ask first.
    pub(crate) fn request_close_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        let pristine = match &self.phase {
            Phase::Connected(active) => active
                .tabs
                .get(index)
                .map(|t| t.is_pristine(cx))
                .unwrap_or(true),
            _ => return,
        };
        if pristine || !self.settings.query.confirm_close_tab {
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
            Phase::Connected(active) => indices
                .iter()
                .any(|&i| active.tabs.get(i).is_some_and(|t| !t.is_pristine(cx))),
            _ => return,
        };
        if any_dirty && self.settings.query.confirm_close_tab {
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
            TabCloseScope::One => unreachable!(),
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
            if let Some(tab) = active.tabs.get_mut(index) {
                tab.pinned = !tab.pinned;
            }
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
                // Shift both panes' active indices left when they sat after the
                // removed tab; `normalize_panes` then collapses the split if this
                // emptied a half and re-points each pane's active at a tab it owns.
                if active.active_tab >= index && active.active_tab > 0 {
                    active.active_tab -= 1;
                }
                if let Some(s) = &mut active.split {
                    if s.secondary >= index && s.secondary > 0 {
                        s.secondary -= 1;
                    }
                }
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
