//! The SQL shell's History dock, as a view.
//!
//! The *store* lives in `red-config`, below the UI, because the assistant
//! retrieves from it on the service thread (`search_query_history`). What stays
//! here is presentation: the Today/Yesterday/Earlier bucketing, the search
//! filter, and the keyboard nav. The Redis shell has its own adapter over the
//! same store in `shell.rs`, sharing the chrome in [`crate::history_panel`].
//!
//! # This is a view
//!
//! [`HistoryPanel`] is an `Entity<HistoryPanel>` (the second surface extracted
//! under Tier 1 of `docs/plans/todo/zed-architecture-inspiration.md`, after
//! `KnowledgeEditor`). It owns everything about the dock that is genuinely its
//! own — the search box, the keyboard selection, which buckets are collapsed,
//! its focus handle — and holds `Entity<QueryHistory>` so it can re-derive its
//! rows from the store on every frame. Nothing pushes rows into it, so the dock
//! cannot go stale.
//!
//! Because it holds the store, it performs its own deletes and clears. It emits
//! [`HistoryPanelEvent`] only for the things that are genuinely the shell's
//! business: seeding SQL into an editor, moving focus, and hiding the dock.
//! That is what retired `AppState::{history_toggle_bucket, history_move,
//! history_accept, delete_history, clear_history}`.

use std::collections::HashSet;
use std::rc::Rc;

use flint::prelude::*;
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, KeyDownEvent, Window, prelude::*,
};
use red_config::history::{QueryHistory, relative_time};

use crate::history_panel::{HistoryNav, HistoryPanelSpec, HistoryRow, HistorySection};

/// The rolling time bucket a history entry falls into: index 0 = Today
/// (< 24h ago, plus any clock-skewed future/zero stamp), 1 = Yesterday
/// (24–48h), 2 = Earlier. Uses the same `now - ran` clock as [`relative_time`].
fn bucket_index(now: u64, ran: u64) -> usize {
    if ran == 0 || now < ran {
        return 0;
    }
    match now - ran {
        0..86_400 => 0,
        86_400..172_800 => 1,
        _ => 2,
    }
}

const LABELS: [(&str, &str); 3] = [
    ("today", "Today"),
    ("yesterday", "Yesterday"),
    ("earlier", "Earlier"),
];

/// What the History dock needs the shell to do. Everything else (search,
/// collapse, selection, delete, clear) the panel does itself.
#[derive(Debug, Clone)]
pub(crate) enum HistoryPanelEvent {
    /// A row was clicked: seed this SQL into an editor. `replace` is the ⌘/Ctrl
    /// modifier — replace the current tab in place rather than opening a new one.
    Open { sql: String, replace: bool },
    /// Enter on the highlighted row: seed it and hand focus back to the editor.
    Accept { sql: String },
    /// The header ✕: hide the dock.
    Close,
    /// Esc (or vim `h`): leave the dock for the editor without closing it.
    LeaveToEditor,
}

/// The SQL History dock.
pub(crate) struct HistoryPanel {
    /// Which connection's entries to show. The store is shared across all of
    /// them, so every read is scoped by this.
    conn_id: String,
    /// The shared history log. Held as a handle, so rows are re-derived per
    /// frame and a delete here is visible everywhere at once.
    store: Entity<QueryHistory>,
    search: Entity<TextInput>,
    focus: FocusHandle,
    /// Flat index of the keyboard-highlighted row, across bucket headers.
    sel: usize,
    /// Which time buckets are collapsed, by [`LABELS`] key. In-memory only
    /// (reset per session), as before.
    collapsed: HashSet<&'static str>,
    /// RAII: re-render when the search box changes or the store mutates.
    _subs: Vec<gpui::Subscription>,
}

impl EventEmitter<HistoryPanelEvent> for HistoryPanel {}

impl Focusable for HistoryPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl HistoryPanel {
    pub(crate) fn new(
        conn_id: String,
        store: Entity<QueryHistory>,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| {
            TextInput::new(cx)
                .with_placeholder(crate::i18n::tr!("history.search", "Search history…"))
        });
        let subs = vec![
            // Typing narrows the list, so the dock has to redraw. The filter
            // itself is applied in `render`, straight off the input's content.
            cx.subscribe(&search, |_this, _input, _evt: &TextInputEvent, cx| {
                cx.notify();
            }),
            // Someone else recorded or removed a query (a run, the palette's
            // clear). Observing the store is what keeps this dock honest without
            // anything having to remember to push into it.
            cx.observe(&store, |_this, _store, cx| cx.notify()),
        ];
        Self {
            conn_id,
            store,
            search,
            focus: cx.focus_handle(),
            sel: 0,
            collapsed: HashSet::new(),
            _subs: subs,
        }
    }

    /// Reset the keyboard highlight to the top (the dock just opened).
    pub(crate) fn reset_selection(&mut self, cx: &mut Context<Self>) {
        self.sel = 0;
        cx.notify();
    }

    /// How many entries this connection has, unfiltered. The nav range.
    fn len(&self, cx: &App) -> usize {
        self.store.read(cx).count_for_conn(&self.conn_id)
    }

    /// Move the keyboard highlight (↑/↓). No-op with empty history.
    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let len = self.len(cx);
        if len == 0 {
            return;
        }
        let sel = self.sel as isize + delta;
        self.sel = sel.clamp(0, len as isize - 1) as usize;
        cx.notify();
    }

    /// Load the highlighted entry into the editor (Enter in the panel).
    fn accept(&mut self, cx: &mut Context<Self>) {
        let sql = self
            .store
            .read(cx)
            .for_conn(&self.conn_id)
            .get(self.sel)
            .map(|e| e.sql.clone());
        if let Some(sql) = sql {
            cx.emit(HistoryPanelEvent::Accept { sql });
        }
    }

    /// Remove one entry by id (the per-row ✕), keeping the keyboard highlight in
    /// range after the row vanishes.
    fn delete(&mut self, id: u64, cx: &mut Context<Self>) {
        self.store.update(cx, |store, _| store.delete(id));
        let len = self.len(cx);
        if self.sel >= len {
            self.sel = len.saturating_sub(1);
        }
        cx.notify();
    }

    /// Clear this connection's entire history.
    fn clear(&mut self, cx: &mut Context<Self>) {
        let conn_id = self.conn_id.clone();
        self.store.update(cx, |store, _| store.clear_conn(&conn_id));
        self.sel = 0;
        cx.notify();
    }

    fn toggle_bucket(&mut self, key: &'static str, cx: &mut Context<Self>) {
        if !self.collapsed.remove(key) {
            self.collapsed.insert(key);
        }
        cx.notify();
    }
}

impl Render for HistoryPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entries = self.store.read(cx).for_conn(&self.conn_id);
        let total = entries.len();
        let query = self.search.read(cx).content().trim().to_lowercase();
        let searching = !query.is_empty();
        let now = crate::conversations::now_unix();

        // Bucket the (already newest-first) entries. `nav_index` stays the row's
        // flat position so ↑/↓ nav — which indexes the full list — keeps landing
        // on the right entry across the bucket headers.
        let mut buckets: [Vec<HistoryRow<Self>>; 3] = [Vec::new(), Vec::new(), Vec::new()];
        for (i, entry) in entries.into_iter().enumerate() {
            if searching && !entry.sql.to_lowercase().contains(&query) {
                continue;
            }
            let sql = entry.sql.clone();
            let id = entry.id;
            let idx = bucket_index(now, entry.ran_unix);
            buckets[idx].push(HistoryRow {
                primary: crate::editor::history_label(&entry.sql).into(),
                secondary: relative_time(entry.ran_unix).into(),
                badge: None,
                nav_index: Some(i),
                activate: Rc::new(move |_this: &mut Self, replace, cx| {
                    cx.emit(HistoryPanelEvent::Open {
                        sql: sql.clone(),
                        replace,
                    });
                }),
                delete: Some(Rc::new(move |this: &mut Self, cx| this.delete(id, cx))),
            });
        }

        let sections: Vec<HistorySection<Self>> = buckets
            .into_iter()
            .zip(LABELS)
            .filter(|(rows, _)| !rows.is_empty())
            .map(|(rows, (key, title))| HistorySection {
                key,
                title: Some(title.into()),
                // A live search force-expands every bucket so matches always show.
                collapsed: !searching && self.collapsed.contains(key),
                toggle: Some(Rc::new(move |this: &mut Self, cx| {
                    this.toggle_bucket(key, cx)
                })),
                rows,
            })
            .collect();

        // Keyboard nav: the same key map the dock has always had (arrows +
        // optional vim hjkl/g/G).
        // Read straight off the published global: no constructor parameter, no
        // setter, and no `apply_settings_effects` arm to keep in sync.
        let vim = crate::settings::Settings::global(cx).keymap.vim_mode;
        let on_key = Rc::new(
            move |this: &mut Self, event: &KeyDownEvent, cx: &mut Context<Self>| -> bool {
                match event.keystroke.key.as_str() {
                    "up" => this.move_selection(-1, cx),
                    "down" => this.move_selection(1, cx),
                    "k" if vim => this.move_selection(-1, cx),
                    "j" if vim => this.move_selection(1, cx),
                    // Half-range deltas jump to the ends without overflowing the
                    // `sel + delta` sum (`move_selection` clamps the rest).
                    "g" if vim => this.move_selection(isize::MIN / 2, cx),
                    "G" if vim => this.move_selection(isize::MAX / 2, cx),
                    "enter" => this.accept(cx),
                    "l" if vim => this.accept(cx),
                    "escape" => cx.emit(HistoryPanelEvent::LeaveToEditor),
                    "h" if vim => cx.emit(HistoryPanelEvent::LeaveToEditor),
                    _ => return false,
                }
                true
            },
        );

        let spec = HistoryPanelSpec {
            sections,
            empty_text: if searching {
                "No matches".into()
            } else {
                "No queries yet".into()
            },
            show_clear: total > 0,
            on_clear: Rc::new(|this: &mut Self, cx| this.clear(cx)),
            on_close: Rc::new(|_this: &mut Self, cx| cx.emit(HistoryPanelEvent::Close)),
            search: Some(self.search.clone()),
            nav: Some(HistoryNav {
                focus: self.focus.clone(),
                on_key,
            }),
            selected: Some(self.sel),
            // The hint badge is the app's to draw (only it knows whether hints
            // are showing); the shell passes it down when it has one.
            focus_badge: None,
        };
        crate::history_panel::render(spec, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a panel over a store seeded with `sqls` (recorded oldest-first, so
    /// the store's newest-first order reverses them).
    fn open_panel(
        cx: &mut gpui::TestAppContext,
        sqls: &[&str],
    ) -> (Entity<HistoryPanel>, Entity<QueryHistory>) {
        let sqls: Vec<String> = sqls.iter().map(|s| s.to_string()).collect();
        cx.update(|cx| {
            let store = cx.new(|_| {
                // Never the real history.json: these run against the user's
                // actual config dir.
                let mut h = QueryHistory::in_memory();
                for sql in &sqls {
                    h.record("conn-1", sql);
                }
                h
            });
            let panel = cx.new(|cx| HistoryPanel::new("conn-1".into(), store.clone(), cx));
            (panel, store)
        })
    }

    #[gpui::test]
    fn selection_clamps_to_the_ends_and_ignores_an_empty_history(cx: &mut gpui::TestAppContext) {
        let (panel, _store) = open_panel(cx, &["select 1", "select 2", "select 3"]);
        cx.update(|cx| {
            panel.update(cx, |this, cx| {
                this.move_selection(-1, cx);
                assert_eq!(this.sel, 0, "up from the top stays at the top");
                this.move_selection(isize::MAX / 2, cx);
                assert_eq!(this.sel, 2, "a huge jump clamps to the last row");
                this.move_selection(1, cx);
                assert_eq!(this.sel, 2, "down from the bottom stays at the bottom");
            });
        });

        let (empty, _) = open_panel(cx, &[]);
        cx.update(|cx| {
            empty.update(cx, |this, cx| {
                this.move_selection(1, cx);
                assert_eq!(this.sel, 0, "nav over an empty history is a no-op");
            });
        });
    }

    #[gpui::test]
    fn deleting_the_last_row_pulls_the_selection_back_into_range(cx: &mut gpui::TestAppContext) {
        let (panel, store) = open_panel(cx, &["select 1", "select 2"]);
        let last_id = cx.update(|cx| store.read(cx).for_conn("conn-1")[0].id);

        cx.update(|cx| {
            panel.update(cx, |this, cx| {
                this.move_selection(1, cx);
                assert_eq!(this.sel, 1);
                // Removing a row while the highlight sits past the new end must
                // pull it back, or the next Enter reads off the end of the list.
                this.delete(last_id, cx);
                assert_eq!(this.sel, 0, "the highlight followed the shrinking list");
            });
            assert_eq!(
                store.read(cx).count_for_conn("conn-1"),
                1,
                "the store shrank"
            );
        });
    }

    #[gpui::test]
    fn accept_emits_the_highlighted_sql_and_clear_empties_the_store(cx: &mut gpui::TestAppContext) {
        use std::cell::RefCell;
        use std::rc::Rc as StdRc;

        let (panel, store) = open_panel(cx, &["select 1", "select 2"]);
        let events = StdRc::new(RefCell::new(Vec::new()));
        let _sub = cx.update(|cx| {
            cx.subscribe(&panel, {
                let events = events.clone();
                move |_, event: &HistoryPanelEvent, _| events.borrow_mut().push(event.clone())
            })
        });

        cx.update(|cx| panel.update(cx, |this, cx| this.accept(cx)));
        // Newest-first, so index 0 is the last thing recorded.
        assert!(
            matches!(events.borrow().as_slice(), [HistoryPanelEvent::Accept { sql }] if sql == "select 2"),
            "Enter reports the highlighted row's SQL, newest first"
        );

        cx.update(|cx| panel.update(cx, |this, cx| this.clear(cx)));
        cx.update(|cx| {
            assert_eq!(
                store.read(cx).count_for_conn("conn-1"),
                0,
                "clear emptied it"
            );
        });
    }

    #[gpui::test]
    fn a_collapsed_bucket_toggles_back_open(cx: &mut gpui::TestAppContext) {
        let (panel, _store) = open_panel(cx, &["select 1"]);
        cx.update(|cx| {
            panel.update(cx, |this, cx| {
                assert!(!this.collapsed.contains("today"));
                this.toggle_bucket("today", cx);
                assert!(this.collapsed.contains("today"), "first toggle collapses");
                this.toggle_bucket("today", cx);
                assert!(!this.collapsed.contains("today"), "second toggle expands");
            });
        });
    }

    #[test]
    fn buckets_split_at_the_day_boundaries() {
        const DAY: u64 = 86_400;
        let now = 10 * DAY;
        assert_eq!(bucket_index(now, now), 0, "just now is Today");
        assert_eq!(bucket_index(now, now - DAY + 1), 0);
        assert_eq!(bucket_index(now, now - DAY), 1, "exactly 24h is Yesterday");
        assert_eq!(bucket_index(now, now - 2 * DAY + 1), 1);
        assert_eq!(bucket_index(now, now - 2 * DAY), 2, "48h is Earlier");
        // A zero or clock-skewed future stamp must not land in "Earlier".
        assert_eq!(bucket_index(now, 0), 0);
        assert_eq!(bucket_index(now, now + DAY), 0);
    }
}
