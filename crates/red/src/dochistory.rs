//! The MongoDB shell's History dock, as a view.
//!
//! The third adapter over the shared [`red_config::history::QueryHistory`], after
//! the SQL and Redis docks, drawing through the same chrome in
//! [`crate::history_panel`]. What is different here is addressing: a filter or a
//! pipeline means nothing without the collection it ran against, so document
//! entries carry a namespace and the dock shows it as the row's badge.
//!
//! Filters and pipelines are told apart by their own shape -- a filter is a
//! document (`{ … }`), a pipeline is an array of stages (`[ … ]`) -- which is not
//! a heuristic but the definition, and it is also how the shell knows which box to
//! seed the entry back into.

use std::rc::Rc;

use flint::prelude::*;
use gpui::{App, Context, Entity, EventEmitter, FocusHandle, Focusable, Window, prelude::*};
use red_config::history::{QueryHistory, relative_time};

use crate::history_panel::{HistoryPanelSpec, HistoryRow, HistorySection};

/// Whether a recorded document query is a pipeline rather than a filter. A
/// pipeline is a JSON array of stages and a filter is a JSON object, so the text
/// itself says which it is.
pub(crate) fn is_pipeline(text: &str) -> bool {
    text.trim_start().starts_with('[')
}

/// What the Mongo History dock needs the shell to do.
#[derive(Debug, Clone)]
pub(crate) enum DocHistoryPanelEvent {
    /// A past filter was clicked: point a tab at its collection and put the
    /// filter back in the box.
    SeedFilter {
        namespace: Option<String>,
        filter: String,
    },
    /// A past pipeline was clicked: open its collection's Query panel with it.
    SeedPipeline {
        namespace: Option<String>,
        pipeline: String,
    },
    /// The header trash: clear this connection's log.
    ClearAll,
    /// The header ✕: hide the dock.
    Close,
}

/// The MongoDB History dock.
pub(crate) struct DocHistoryPanel {
    /// Which connection's entries to show (the log is shared across all of them,
    /// so every read is scoped by this).
    conn_id: String,
    store: Entity<QueryHistory>,
    search: Entity<TextInput>,
    focus: FocusHandle,
    filters_collapsed: bool,
    pipelines_collapsed: bool,
    /// RAII: redraw when the search box changes or the store mutates.
    _subs: Vec<gpui::Subscription>,
}

impl EventEmitter<DocHistoryPanelEvent> for DocHistoryPanel {}

impl Focusable for DocHistoryPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl DocHistoryPanel {
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
            cx.subscribe(&search, |_this, _input, _evt: &TextInputEvent, cx| {
                cx.notify();
            }),
            cx.observe(&store, |_this, _store, cx| cx.notify()),
        ];
        Self {
            conn_id,
            store,
            search,
            focus: cx.focus_handle(),
            filters_collapsed: false,
            pipelines_collapsed: false,
            _subs: subs,
        }
    }

    fn delete_entry(&mut self, id: u64, cx: &mut Context<Self>) {
        self.store.update(cx, |store, _| store.delete(id));
        cx.notify();
    }
}

impl Render for DocHistoryPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entries = self.store.read(cx).for_conn(&self.conn_id);
        let has_any = !entries.is_empty();
        let query = self.search.read(cx).content().trim().to_lowercase();
        let searching = !query.is_empty();

        let matches = |e: &red_config::history::HistoryEntry| {
            !searching
                || e.sql.to_lowercase().contains(&query)
                || e.namespace
                    .as_deref()
                    .is_some_and(|ns| ns.to_lowercase().contains(&query))
        };

        let row = |entry: &red_config::history::HistoryEntry| {
            let (text, namespace, id) = (entry.sql.clone(), entry.namespace.clone(), entry.id);
            let pipeline = is_pipeline(&text);
            HistoryRow {
                primary: crate::editor::history_label(&text).into(),
                secondary: relative_time(entry.ran_unix).into(),
                badge: namespace.clone().map(Into::into),
                nav_index: None,
                activate: Rc::new(move |_this: &mut Self, _replace, cx| {
                    let namespace = namespace.clone();
                    if pipeline {
                        cx.emit(DocHistoryPanelEvent::SeedPipeline {
                            namespace,
                            pipeline: text.clone(),
                        });
                    } else {
                        cx.emit(DocHistoryPanelEvent::SeedFilter {
                            namespace,
                            filter: text.clone(),
                        });
                    }
                }),
                delete: Some(Rc::new(move |this: &mut Self, cx| {
                    this.delete_entry(id, cx)
                })),
            }
        };

        let filter_rows: Vec<HistoryRow<Self>> = entries
            .iter()
            .filter(|e| matches(e) && !is_pipeline(&e.sql))
            .map(row)
            .collect();
        let pipeline_rows: Vec<HistoryRow<Self>> = entries
            .iter()
            .filter(|e| matches(e) && is_pipeline(&e.sql))
            .map(row)
            .collect();

        let mut sections: Vec<HistorySection<Self>> = Vec::new();
        if !filter_rows.is_empty() {
            sections.push(HistorySection {
                key: "doc-filters",
                title: Some("Filters".into()),
                // A live search force-expands, so matches always show.
                collapsed: !searching && self.filters_collapsed,
                toggle: Some(Rc::new(|this: &mut Self, cx| {
                    this.filters_collapsed = !this.filters_collapsed;
                    cx.notify();
                })),
                rows: filter_rows,
            });
        }
        if !pipeline_rows.is_empty() {
            sections.push(HistorySection {
                key: "doc-pipelines",
                title: Some("Pipelines".into()),
                collapsed: !searching && self.pipelines_collapsed,
                toggle: Some(Rc::new(|this: &mut Self, cx| {
                    this.pipelines_collapsed = !this.pipelines_collapsed;
                    cx.notify();
                })),
                rows: pipeline_rows,
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
            on_clear: Rc::new(|_this: &mut Self, cx| cx.emit(DocHistoryPanelEvent::ClearAll)),
            on_close: Rc::new(|_this: &mut Self, cx| cx.emit(DocHistoryPanelEvent::Close)),
            search: Some(self.search.clone()),
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

    #[test]
    fn a_pipeline_is_told_from_a_filter_by_its_own_shape() {
        assert!(is_pipeline("[ { \"$match\": {} } ]"));
        assert!(is_pipeline("\n  [ ]"));
        assert!(!is_pipeline("{ \"status\": \"active\" }"));
        assert!(!is_pipeline(""));
    }
}
