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
                active.active_tab = index;
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

    /// Finish a tab-strip drag: move the dragged tab (`from`) into the gap the
    /// indicator settled on. The dragged tab follows the cursor and stays
    /// focused. Clears the indicator regardless.
    pub(crate) fn drop_tab(&mut self, from: usize, cx: &mut Context<Self>) {
        if let Phase::Connected(active) = &mut self.phase {
            if let Some(gap) = active.tab_drop_target.take() {
                if from < active.tabs.len() {
                    // `gap` indexes the pre-removal strip; shift left when the
                    // dragged tab sat before the gap.
                    let dest = if from < gap { gap - 1 } else { gap };
                    let dest = dest.min(active.tabs.len() - 1);
                    let tab = active.tabs.remove(from);
                    active.tabs.insert(dest, tab);
                    active.active_tab = dest;
                }
            }
        }
        cx.notify();
    }

    /// Push a freshly-built tab, focus it, and seed its completions. Returns the
    /// new index. Callers supply the tab (a blank query or a table preview).
    /// Eagerly describe every table once the skeleton lands, so column and
    /// `table.` completion covers the whole schema without the user expanding
    /// each node first. Details arrive as `TableDescribed` events that refresh the
    /// completion index. Capped so a pathological schema can't flood the backend —
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

    pub(crate) fn push_tab(&mut self, tab: QueryTab, cx: &mut Context<Self>) -> usize {
        let index = match &mut self.phase {
            Phase::Connected(active) => {
                active.tabs.push(tab);
                active.active_tab = active.tabs.len() - 1;
                // Scroll the freshly-focused tab into view on the next paint, in
                // case the strip was already scrolled or crowded.
                active.tab_scroll.scroll_to_item(active.active_tab);
                active.active_tab
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
            let n = active.tabs.len();
            if n > 1 {
                active.active_tab = if forward {
                    (active.active_tab + 1) % n
                } else {
                    (active.active_tab + n - 1) % n
                };
                active.tab_scroll.scroll_to_item(active.active_tab);
                cx.notify();
                return true;
            }
        }
        false
    }

    /// Close the focused tab (the ⌘W binding) — routes through the same
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
        cx.notify();
    }

    /// The tab-strip "×": close immediately if pristine, else ask first.
    pub(crate) fn request_close_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        let pristine = match &self.phase {
            Phase::Connected(active) => active
                .tabs
                .get(index)
                .map(|t| t.is_pristine(cx))
                .unwrap_or(true),
            _ => return,
        };
        if pristine {
            self.close_tab(index, cx);
        } else {
            self.confirm_close_tab = Some(index);
            // Focus the modal so its own Enter/Esc handling is heard.
            self.focus_modal = true;
            cx.notify();
        }
    }

    /// Confirmation accepted — close the tab that was awaiting it.
    pub(crate) fn confirm_close(&mut self, cx: &mut Context<Self>) {
        if let Some(index) = self.confirm_close_tab.take() {
            self.close_tab(index, cx);
        }
        self.refocus_root = true;
        cx.notify();
    }

    pub(crate) fn cancel_close(&mut self, cx: &mut Context<Self>) {
        self.confirm_close_tab = None;
        self.refocus_root = true;
        cx.notify();
    }

    /// Drop tab `index`, freeing its backend result. Closing the *last* tab is
    /// allowed: the strip goes empty and the shell shows a placeholder pane (the
    /// connection stays open — the strip's ＋ opens a fresh query).
    fn close_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        self.confirm_close_tab = None;
        let free_epoch = match &mut self.phase {
            Phase::Connected(active) if index < active.tabs.len() => {
                let removed = active.tabs.remove(index);
                // Keep the focus stable: clamp, and shift left if we removed a
                // tab at or before the focused one. Harmless when the strip is
                // now empty — `active()` just returns `None`.
                if active.active_tab >= index && active.active_tab > 0 {
                    active.active_tab -= 1;
                }
                active.active_tab = active.active_tab.min(active.tabs.len().saturating_sub(1));
                removed.result.map(|g| g.epoch)
            }
            _ => return,
        };
        // Free the backend result that backed the closed tab's grid.
        if let Some(epoch) = free_epoch {
            self.send_active(Command::CloseResult { epoch });
        }
        cx.notify();
    }
}
