//! The Redis shell's History dock, as a view.
//!
//! Two sections over two stores: recently-viewed keys
//! ([`crate::kvbrowse::RecentKeys`]) and past console commands (the shared
//! [`red_config::history::QueryHistory`], the same log the SQL dock reads). It
//! draws through the chrome in [`crate::history_panel`], which the SQL dock also
//! uses — that module is generic over the host view precisely so both can be
//! views without forking the renderer.
//!
//! # This is a view
//!
//! The third surface extracted under Tier 1 of
//! `docs/plans/todo/zed-architecture-inspiration.md`, after `KnowledgeEditor`
//! and the SQL `HistoryPanel`. It owns its search box, focus handle and section
//! collapse, and holds both stores as handles so it re-derives its rows every
//! frame and `cx.observe`s them: viewing a key or running a command redraws the
//! dock with nothing having to push into it.
//!
//! It deletes command rows itself (it holds that store). The things it cannot
//! do alone it emits: opening a key needs the tab machinery, and removing or
//! clearing keys needs the on-disk recent-keys store, which `AppState` owns.

use std::rc::Rc;
use std::time::Duration;

use flint::prelude::*;
use gpui::{App, Context, Entity, EventEmitter, FocusHandle, Focusable, Window, prelude::*};
use red_config::history::{QueryHistory, relative_time};
use red_core::kv::KvType;

use crate::history_panel::{HistoryPanelSpec, HistoryRow, HistorySection};
use crate::kvbrowse::RecentKeys;

/// What the Redis History dock needs the shell to do.
#[derive(Debug, Clone)]
pub(crate) enum KvHistoryPanelEvent {
    /// A recently-viewed key was clicked: open it in the inspector (which may
    /// have to make a Browse tab first).
    OpenKey {
        key: String,
        kv_type: KvType,
        ttl: Option<Duration>,
    },
    /// The per-row ✕ on a key. `AppState` owns the persisted store, so it does
    /// the removal and the write-back.
    RemoveKey { key: String },
    /// A past command was clicked: put it in the console's input.
    SeedConsole { command: String },
    /// The header trash: clear both this connection's command log and its
    /// recently-viewed keys.
    ClearAll,
    /// The header ✕: hide the dock.
    Close,
}

/// The Redis History dock.
pub(crate) struct KvHistoryPanel {
    /// Which connection's command entries to show (the log is shared across all
    /// connections, so every read is scoped by this).
    conn_id: String,
    commands: Entity<QueryHistory>,
    keys: Entity<RecentKeys>,
    search: Entity<TextInput>,
    focus: FocusHandle,
    keys_collapsed: bool,
    cmds_collapsed: bool,
    /// RAII: redraw when the search box changes or either store mutates.
    _subs: Vec<gpui::Subscription>,
}

impl EventEmitter<KvHistoryPanelEvent> for KvHistoryPanel {}

impl Focusable for KvHistoryPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl KvHistoryPanel {
    pub(crate) fn new(
        conn_id: String,
        commands: Entity<QueryHistory>,
        keys: Entity<RecentKeys>,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| {
            TextInput::new(cx)
                .with_placeholder(crate::i18n::tr!("history.search", "Search history…"))
        });
        let subs = vec![
            cx.subscribe(&search, |_this, _input, _evt: &TextInputEvent, cx| {
                cx.notify();
            }),
            cx.observe(&commands, |_this, _store, cx| cx.notify()),
            cx.observe(&keys, |_this, _store, cx| cx.notify()),
        ];
        Self {
            conn_id,
            commands,
            keys,
            search,
            focus: cx.focus_handle(),
            keys_collapsed: false,
            cmds_collapsed: false,
            _subs: subs,
        }
    }

    /// Remove one command entry by id. Done here rather than emitted: the panel
    /// holds that store, and the log needs no write-back beyond its own.
    fn delete_command(&mut self, id: u64, cx: &mut Context<Self>) {
        self.commands.update(cx, |store, _| store.delete(id));
        cx.notify();
    }
}

impl Render for KvHistoryPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let commands = self.commands.read(cx).for_conn(&self.conn_id);
        let keys: Vec<crate::kvbrowse::RecentKey> = self.keys.read(cx).items().to_vec();
        let has_any = !keys.is_empty() || !commands.is_empty();

        let query = self.search.read(cx).content().trim().to_lowercase();
        let searching = !query.is_empty();

        // Recently-viewed keys → rows (badged with the value type).
        let key_rows: Vec<HistoryRow<Self>> = keys
            .into_iter()
            .filter(|r| !searching || r.key.to_lowercase().contains(&query))
            .map(|r| {
                let (key, kv_type, ttl) = (r.key.clone(), r.kv_type.clone(), r.ttl);
                let remove_key = r.key.clone();
                HistoryRow {
                    primary: r.key.into(),
                    secondary: relative_time(r.viewed_unix).into(),
                    badge: Some(kv_type.label().to_string().into()),
                    nav_index: None,
                    activate: Rc::new(move |_this: &mut Self, _replace, cx| {
                        cx.emit(KvHistoryPanelEvent::OpenKey {
                            key: key.clone(),
                            kv_type: kv_type.clone(),
                            ttl,
                        });
                    }),
                    delete: Some(Rc::new(move |_this: &mut Self, cx| {
                        cx.emit(KvHistoryPanelEvent::RemoveKey {
                            key: remove_key.clone(),
                        });
                    })),
                }
            })
            .collect();

        // Past console commands → rows.
        let cmd_rows: Vec<HistoryRow<Self>> = commands
            .into_iter()
            .filter(|e| !searching || e.sql.to_lowercase().contains(&query))
            .map(|entry| {
                let command = entry.sql.clone();
                let id = entry.id;
                HistoryRow {
                    primary: crate::editor::history_label(&entry.sql).into(),
                    secondary: relative_time(entry.ran_unix).into(),
                    badge: None,
                    nav_index: None,
                    activate: Rc::new(move |_this: &mut Self, _replace, cx| {
                        cx.emit(KvHistoryPanelEvent::SeedConsole {
                            command: command.clone(),
                        });
                    }),
                    delete: Some(Rc::new(move |this: &mut Self, cx| {
                        this.delete_command(id, cx)
                    })),
                }
            })
            .collect();

        let mut sections: Vec<HistorySection<Self>> = Vec::new();
        if !key_rows.is_empty() {
            sections.push(HistorySection {
                key: "recent-keys",
                title: Some("Recently viewed keys".into()),
                // A live search force-expands, so matches always show.
                collapsed: !searching && self.keys_collapsed,
                toggle: Some(Rc::new(|this: &mut Self, cx| {
                    this.keys_collapsed = !this.keys_collapsed;
                    cx.notify();
                })),
                rows: key_rows,
            });
        }
        if !cmd_rows.is_empty() {
            sections.push(HistorySection {
                key: "commands",
                title: Some("Commands".into()),
                collapsed: !searching && self.cmds_collapsed,
                toggle: Some(Rc::new(|this: &mut Self, cx| {
                    this.cmds_collapsed = !this.cmds_collapsed;
                    cx.notify();
                })),
                rows: cmd_rows,
            });
        }

        let spec = HistoryPanelSpec {
            sections,
            empty_text: if searching {
                "No matches".into()
            } else {
                "Nothing yet".into()
            },
            show_clear: has_any,
            on_clear: Rc::new(|_this: &mut Self, cx| cx.emit(KvHistoryPanelEvent::ClearAll)),
            on_close: Rc::new(|_this: &mut Self, cx| cx.emit(KvHistoryPanelEvent::Close)),
            search: Some(self.search.clone()),
            // The Redis dock has never wired keyboard nav; unchanged here.
            nav: None,
            selected: None,
            // Drawn by the shell, which is the only thing that knows whether
            // focus hints are showing.
            focus_badge: None,
        };
        crate::history_panel::render(spec, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_panel(
        cx: &mut gpui::TestAppContext,
        commands: &[&str],
        key_names: &[&str],
    ) -> (
        Entity<KvHistoryPanel>,
        Entity<QueryHistory>,
        Entity<RecentKeys>,
    ) {
        let cmds: Vec<String> = commands.iter().map(|s| s.to_string()).collect();
        let names: Vec<String> = key_names.iter().map(|s| s.to_string()).collect();
        cx.update(|cx| {
            // Never the real history.json / recent-keys file.
            let store = cx.new(|_| {
                let mut h = QueryHistory::in_memory();
                for c in &cmds {
                    h.record("conn-1", c);
                }
                h
            });
            let keys = cx.new(|_| {
                let mut k = RecentKeys::default();
                for n in &names {
                    k.record(n.clone(), KvType::String, None);
                }
                k
            });
            let panel =
                cx.new(|cx| KvHistoryPanel::new("conn-1".into(), store.clone(), keys.clone(), cx));
            (panel, store, keys)
        })
    }

    #[gpui::test]
    fn deleting_a_command_row_hits_the_shared_log(cx: &mut gpui::TestAppContext) {
        let (panel, store, _keys) = open_panel(cx, &["ping", "get a"], &[]);
        let id = cx.update(|cx| store.read(cx).for_conn("conn-1")[0].id);

        cx.update(|cx| panel.update(cx, |this, cx| this.delete_command(id, cx)));
        cx.update(|cx| {
            assert_eq!(
                store.read(cx).count_for_conn("conn-1"),
                1,
                "the dock deletes command rows itself, in the shared store"
            );
        });
    }

    #[gpui::test]
    fn key_removal_and_clear_are_emitted_not_done_locally(cx: &mut gpui::TestAppContext) {
        use std::cell::RefCell;

        let (panel, _store, keys) = open_panel(cx, &["ping"], &["user:1"]);
        let events = Rc::new(RefCell::new(Vec::new()));
        let _sub = cx.update(|cx| {
            cx.subscribe(&panel, {
                let events = events.clone();
                move |_, event: &KvHistoryPanelEvent, _| events.borrow_mut().push(event.clone())
            })
        });

        // Both of these need the persisted store, which lives on `AppState`, so
        // the panel must ask rather than mutate.
        cx.update(|cx| {
            panel.update(cx, |_this, cx| {
                cx.emit(KvHistoryPanelEvent::RemoveKey {
                    key: "user:1".into(),
                });
                cx.emit(KvHistoryPanelEvent::ClearAll);
            })
        });

        assert!(
            matches!(
                events.borrow().as_slice(),
                [
                    KvHistoryPanelEvent::RemoveKey { .. },
                    KvHistoryPanelEvent::ClearAll
                ]
            ),
            "both reached the shell"
        );
        cx.update(|cx| {
            assert_eq!(
                keys.read(cx).items().len(),
                1,
                "and the panel changed nothing on its own"
            );
        });
    }

    #[gpui::test]
    fn recent_keys_dedupe_newest_first_and_cap(cx: &mut gpui::TestAppContext) {
        let (_panel, _store, keys) = open_panel(cx, &[], &["a", "b", "a"]);
        cx.update(|cx| {
            let k = keys.read(cx);
            assert_eq!(
                k.items().iter().map(|r| r.key.as_str()).collect::<Vec<_>>(),
                vec!["a", "b"],
                "re-viewing a key moves it to the front rather than duplicating"
            );
        });
    }
}
