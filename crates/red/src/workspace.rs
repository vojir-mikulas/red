//! Workspace persistence: capturing a connection's open tabs to `state.json`
//! and rebuilding them on the next connect.
//!
//! The motivating loss is the **editor buffer**. Tabs and pane geometry are
//! cheap to redo by hand; half-written SQL is not, and until now quitting threw
//! it away with no prompt (unlike closing a single tab, which
//! `safety.confirm_close_tab` has always guarded). GPUI's `on_app_quit` cannot
//! veto a quit, so the answer is to make quitting non-destructive rather than to
//! nag on the way out.
//!
//! What is *not* persisted is as deliberate as what is: no result rows, no
//! staged edits, no watch, no scroll offset. Rows belong to the database, and
//! writing them to `state.json` would put query output on disk outside the
//! export path; staged edits are uncommitted intent that must not survive
//! silently into a session the user may point at a different server.
//!
//! Capture is driven from `AppState` on tab lifecycle events plus a debounced
//! editor tick, and [`crate::local_state::LocalState::set_workspace`] no-ops
//! when nothing changed, so an idle app does no disk I/O.

use gpui::{App, Context};

use crate::app::{ActiveConn, AppState, EMPTY_QUERY, Phase, QueryTab, TabWorkspace};
use crate::local_state::{StoredChild, StoredLayout, StoredNode, StoredTab, StoredWorkspace};
use crate::panes::tree::{Child, PaneTree};
use crate::panes::{Node, PaneId, PaneLayout, SplitAxis};

impl AppState {
    /// Snapshot the foreground connection's workspace to `state.json`.
    ///
    /// Cheap and idempotent: safe to call from any event that could have moved a
    /// tab. Does nothing while disconnected, since there is no workspace to save
    /// and a blank one would overwrite the last good snapshot.
    pub(crate) fn save_workspace(&mut self, cx: &mut Context<Self>) {
        let Phase::Connected(active) = &self.phase else {
            return;
        };
        // See `restore_workspace`: those seams own their tabs, and capturing the
        // SQL shell's phantom tab would write a workspace that means nothing.
        if active.kv_view.is_some() || active.doc_view.is_some() {
            return;
        }
        let conn_id = active.conn_id.clone();
        let workspace = capture(active, cx);
        self.local_state.set_workspace(&conn_id, workspace);
    }

    /// Rebuild `active`'s tabs from the last saved workspace, replacing the
    /// single blank tab a fresh connection opens with.
    ///
    /// A no-op unless the user opted in (`behavior.restore_last_session`), so the
    /// default remains the old behaviour: connect, get one empty tab.
    pub(crate) fn restore_workspace(&mut self, cx: &mut Context<Self>) {
        if !self.settings.behavior.restore_last_session {
            return;
        }
        let Phase::Connected(active) = &self.phase else {
            return;
        };
        // Redis and MongoDB keep their own tab models (`kv_view` / `doc_view`);
        // `active.tabs` on those is a single phantom the SQL shell never renders.
        // Restoring SQL tabs into one would put a query editor behind a key
        // browser. Their workspaces are a separate piece of work.
        if active.kv_view.is_some() || active.doc_view.is_some() {
            return;
        }
        let Some(stored) = self.local_state.workspace(&active.conn_id).cloned() else {
            return;
        };
        if stored.tabs.is_empty() {
            return;
        }
        // Only over a pristine workspace. Reconnecting to a session the user has
        // already typed into (a re-dial after a dropped connection) must not
        // throw that away for a snapshot from an earlier run.
        let pristine = active.tabs.len() == 1
            && active
                .tabs
                .first()
                .is_some_and(|tab| tab.is_pristine(cx) && !tab.pinned);
        if !pristine {
            return;
        }

        let dialect = self.active_dialect();
        let layout = stored.layout.as_ref().map(restore_layout);
        let mut tabs = Vec::with_capacity(stored.tabs.len());
        for stored_tab in &stored.tabs {
            let mut tab = QueryTab::new(stored_tab.title.clone(), dialect, cx);
            tab.editor.update(cx, |editor, cx| {
                editor.set_content(stored_tab.sql.clone(), cx)
            });
            tab.pinned = stored_tab.pinned;
            tab.namespace = stored_tab.namespace.clone();
            tab.pane = PaneId(stored_tab.pane);
            tabs.push(tab);
        }

        let browses: Vec<(usize, (String, String))> = stored
            .tabs
            .iter()
            .enumerate()
            .filter_map(|(i, t)| t.browse.clone().map(|b| (i, b)))
            .collect();
        let active_index = stored.active.min(tabs.len() - 1);

        if let Phase::Connected(active) = &mut self.phase {
            if let Some(layout) = layout {
                active.layout = layout;
            }
            // Any tab whose pane did not survive the layout's normalization is
            // pulled to the focused one rather than being stranded in a pane that
            // no strip draws.
            let live = active.layout.tree().panes();
            for tab in &mut tabs {
                if !live.contains(&tab.pane) {
                    tab.pane = active.layout.focus();
                }
            }
            active.tabs = tabs;
            active.query_seq = active.tabs.len();
            active.set_focused_tab(active_index);
        }
        // Re-open each restored browse so it comes back as a real browse (FK
        // affordances, keyset paging) rather than a tab holding the equivalent
        // SELECT text. `open_result` targets the *focused* tab, so focus walks
        // the restored browses and lands back on the stored active tab.
        let kind = match &self.phase {
            Phase::Connected(active) => active.config.kind,
            _ => return,
        };
        for (index, (schema, table)) in browses {
            if let Phase::Connected(active) = &mut self.phase {
                active.set_focused_tab(index);
            }
            let sql = format!(
                "SELECT * FROM {}.{}",
                crate::schema::quote_ident(&schema, kind),
                crate::schema::quote_ident(&table, kind)
            );
            let label = format!("{schema}.{table}");
            self.open_result(label, sql, Some((schema, table)), cx);
        }
        if let Phase::Connected(active) = &mut self.phase {
            active.set_focused_tab(active_index);
        }
        cx.notify();
    }
}

/// Capture one connection's workspace.
///
/// Tabs that show a whole-half body (an ER diagram, a DDL view) are skipped:
/// they hold no user text, and re-deriving one costs a click from the tree.
fn capture(active: &ActiveConn, cx: &App) -> StoredWorkspace {
    let mut tabs = Vec::with_capacity(active.tabs.len());
    let mut active_index = 0;
    for (i, tab) in active.tabs.iter().enumerate() {
        if tab.is_view() {
            continue;
        }
        let sql = tab.editor.read(cx).content();
        let browse = tab
            .result
            .as_ref()
            .and_then(|grid| grid.read(cx).browse_spec());
        // A blank, unpinned, unbrowsed tab carries nothing worth a restore; a
        // workspace of only those saves as empty and so clears the entry.
        if browse.is_none() && !tab.pinned && sql == EMPTY_QUERY {
            continue;
        }
        if active.focused_tab_index() == Some(i) {
            active_index = tabs.len();
        }
        tabs.push(StoredTab {
            title: tab.title.clone(),
            sql,
            pinned: tab.pinned,
            namespace: tab.namespace.clone(),
            pane: tab.pane.0,
            browse,
        });
    }
    let layout = active.layout.tree().is_split().then(|| StoredLayout {
        root: capture_node(active.layout.tree().root()),
        focus: active.layout.focus().0,
        next: active.layout.tree().next_id(),
    });
    StoredWorkspace {
        tabs,
        active: active_index,
        layout,
    }
}

fn capture_node(node: &Node) -> StoredNode {
    match node {
        Node::Leaf(id) => StoredNode::Leaf(id.0),
        Node::Split { axis, children } => StoredNode::Split {
            vertical: matches!(axis, SplitAxis::Vertical),
            children: children
                .iter()
                .map(|c| StoredChild::new(c.weight, capture_node(&c.node)))
                .collect(),
        },
    }
}

fn restore_layout(stored: &StoredLayout) -> PaneLayout {
    let tree = PaneTree::restore(
        restore_node(&stored.root),
        PaneId(stored.focus),
        stored.next,
    );
    PaneLayout::restore(tree)
}

fn restore_node(stored: &StoredNode) -> Node {
    match stored {
        StoredNode::Leaf(id) => Node::Leaf(PaneId(*id)),
        StoredNode::Split { vertical, children } => Node::Split {
            axis: if *vertical {
                SplitAxis::Vertical
            } else {
                SplitAxis::Horizontal
            },
            children: children
                .iter()
                .map(|c| Child {
                    weight: c.weight(),
                    node: restore_node(&c.node),
                })
                .collect(),
        },
    }
}
