//! The SQL shell's History dock: the adapter that turns
//! [`red_config::history::QueryHistory`] into a [`crate::history_panel`] spec.
//!
//! The *store* lives in `red-config`, below the UI, because the assistant
//! retrieves from it on the service thread (`search_query_history`). What stays
//! here is presentation: the Today/Yesterday/Earlier bucketing, the search
//! filter, and the keyboard nav. The Redis shell has its own adapter over the
//! same store in `shell.rs`.

use std::rc::Rc;

use gpui::{Context, KeyDownEvent, prelude::*};
use red_config::history::relative_time;

use crate::app::{ActiveConn, AppState};

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

impl AppState {
    /// The History panel for the left dock: this connection's past queries,
    /// newest first, grouped into collapsible Today/Yesterday/Earlier buckets
    /// with a search box on top. Clicking a row loads it into the active editor;
    /// hovering a row reveals a ✕ to delete it. Pure adapter over the shared
    /// [`crate::history_panel`] renderer.
    pub(crate) fn render_history(
        &self,
        active: &ActiveConn,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        use crate::history_panel::{HistoryNav, HistoryPanelSpec, HistoryRow, HistorySection};

        let entries = self.query_history.for_conn(&active.conn_id);
        let total = entries.len();
        let query = active
            .history_search
            .read(cx)
            .content()
            .trim()
            .to_lowercase();
        let searching = !query.is_empty();
        let now = crate::conversations::now_unix();

        // Bucket the (already newest-first) entries. `nav_index` stays the row's
        // flat position so the existing ↑/↓ nav — which indexes the full list —
        // keeps landing on the right entry across the bucket headers.
        let mut buckets: [Vec<HistoryRow>; 3] = [Vec::new(), Vec::new(), Vec::new()];
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
                activate: Rc::new(move |this: &mut AppState, replace, cx| {
                    this.open_history(sql.clone(), replace, cx)
                }),
                delete: Some(Rc::new(move |this: &mut AppState, cx| {
                    this.delete_history(id, cx)
                })),
            });
        }

        const LABELS: [(&str, &str); 3] = [
            ("today", "Today"),
            ("yesterday", "Yesterday"),
            ("earlier", "Earlier"),
        ];
        let sections: Vec<HistorySection> = buckets
            .into_iter()
            .zip(LABELS)
            .filter(|(rows, _)| !rows.is_empty())
            .map(|(rows, (key, title))| HistorySection {
                key,
                title: Some(title.into()),
                // A live search force-expands every bucket so matches always show.
                collapsed: !searching && active.history_bucket_collapsed.contains(key),
                toggle: Some(Rc::new(move |this: &mut AppState, cx| {
                    this.history_toggle_bucket(key, cx)
                })),
                rows,
            })
            .collect();

        // Keyboard nav: same key map the dock has always had (arrows + optional
        // vim hjkl/g/G), now routed through the shared renderer.
        let on_key = Rc::new(
            |this: &mut AppState, event: &KeyDownEvent, cx: &mut Context<AppState>| -> bool {
                let vim = this.vim_mode();
                match event.keystroke.key.as_str() {
                    "up" => this.history_move(-1, cx),
                    "down" => this.history_move(1, cx),
                    "k" if vim => this.history_move(-1, cx),
                    "j" if vim => this.history_move(1, cx),
                    // Half-range deltas jump to the ends without overflowing the
                    // `history_sel + delta` sum (`history_move` clamps the rest).
                    "g" if vim => this.history_move(isize::MIN / 2, cx),
                    "G" if vim => this.history_move(isize::MAX / 2, cx),
                    "enter" => this.history_accept(cx),
                    "l" if vim => this.history_accept(cx),
                    "escape" => {
                        this.pending_focus = Some(crate::app::Pane::Editor);
                        cx.notify();
                    }
                    "h" if vim => {
                        this.pending_focus = Some(crate::app::Pane::Editor);
                        cx.notify();
                    }
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
            on_clear: Rc::new(|this: &mut AppState, cx| this.clear_history(cx)),
            search: Some(active.history_search.clone()),
            nav: Some(HistoryNav {
                focus: active.history_focus.clone(),
                on_key,
            }),
            selected: Some(active.history_sel),
        };
        self.render_history_panel(spec, cx)
    }
}
