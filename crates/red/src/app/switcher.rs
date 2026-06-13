//! The connection switcher (Cmd-P): rebuilding its sectioned popover from the
//! saved connections + warm parked sessions, opening/closing it, routing a
//! chosen row to an instant warm-switch or a cold connect, and the pure
//! section/footer builders. Split out of `mod.rs`.

use super::*;

impl AppState {
    /// Rebuild the switcher's trigger + sections from the current connections and
    /// phase. Called after every connect/disconnect and before the popover opens.
    pub(crate) fn rebuild_switcher(&mut self, cx: &mut Context<Self>) {
        let (label, dot, sections) =
            switcher_sections(&self.connections, &self.phase, &self.parked, cx.theme());
        self.switcher.update(cx, |s, cx| {
            s.set_trigger(label, dot, cx);
            s.set_sections(sections, cx);
        });
    }

    /// Toggle the connection switcher popover (⌘P, or a click on its topbar
    /// trigger). Refresh its contents first so recents/active are current.
    pub(crate) fn toggle_switcher(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.switcher.read(cx).is_open() {
            // Closing: the popover's search field held focus, so dropping it leaves
            // `window.focused()` dangling — reclaim the root (like `close_palette`)
            // or the *next* ⌘P finds no dispatch target and won't reopen.
            self.switcher.update(cx, |s, cx| s.close(cx));
            self.refocus_root = true;
            cx.notify();
            return;
        }
        // Opening only makes sense where the trigger is mounted (the connected
        // topbar anchors the popover). On the welcome screen there's no anchor, so
        // opening would just steal focus into an unrendered field — no-op instead.
        if !matches!(self.phase, Phase::Connected(_)) {
            return;
        }
        self.rebuild_switcher(cx);
        self.switcher.update(cx, |s, cx| s.open(window, cx));
    }

    /// Handle a row chosen in the switcher popover. Row ids are `conn:<index>` (a
    /// saved connection) or the two action rows. Switching to a warm connection is
    /// instant (foreground its parked workspace); a cold one connects.
    pub(crate) fn on_switcher_event(
        &mut self,
        _switcher: Entity<Switcher>,
        event: &SwitcherEvent,
        cx: &mut Context<Self>,
    ) {
        // Both Activate and Dismiss close the popover, dropping its focused search
        // field. Reclaim the root so the next ⌘P dispatches; any follow-on focus
        // (a modal, a switched-in pane) overrides this within the same render.
        self.refocus_root = true;
        cx.notify();
        let SwitcherEvent::Activate(ElementId::Name(name)) = event else {
            return;
        };
        if name.as_ref() == "action:new" {
            self.open_new_form(cx);
        } else if name.as_ref() == "action:manage" {
            // The full manager *is* the disconnected landing screen.
            if !matches!(self.phase, Phase::Disconnected) {
                self.disconnect(cx);
            }
        } else if let Some(index) = name
            .strip_prefix("conn:")
            .and_then(|n| n.parse::<usize>().ok())
        {
            self.switch_to_connection(index, cx);
        }
    }

    /// Switch the window to saved connection `index`. Already foreground → no-op;
    /// already warm (parked) → foreground it instantly (no reconnect); otherwise
    /// connect cold.
    pub(crate) fn switch_to_connection(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(target) = self.connections.get(index).map(|c| c.id.clone()) else {
            return;
        };
        // Already the on-screen connection — nothing to do. Match on the stable id,
        // not the display name (two saved connections may share a name).
        if matches!(&self.phase, Phase::Connected(a) if a.conn_id == target) {
            return;
        }
        // A warm parked session for this connection — the instant-switch path.
        if let Some(&warm) = self
            .parked
            .iter()
            .find(|(_, a)| a.conn_id == target)
            .map(|(id, _)| id)
        {
            // Refresh recency for the switched-to connection.
            if let Some(stored) = self.connections.get_mut(index) {
                stored.last_accessed = Some(config::now());
            }
            self.persist(cx);
            self.park_foreground();
            self.foreground_parked(warm, cx);
            self.rebuild_switcher(cx);
            return;
        }
        // Cold — open a new session (parking the current one warm).
        self.connect(index, cx);
    }
}

/// How many recent connections to surface in the switcher popover.
const SWITCHER_RECENT_CAP: usize = 8;

/// Build the connection switcher's trigger (label + leading dot) and its
/// sections from the saved connections, the current phase, and the warm parked
/// sessions. The foreground connection heads a "This window" section (warm/
/// connecting badge) and drives the trigger; other warm sessions fill an
/// "Open" section (instant switch); the rest are capped "Recent" recents. The
/// "New…" / "Manage…" actions live in the always-visible footer.
pub(super) fn switcher_sections(
    connections: &[StoredConnection],
    phase: &Phase,
    parked: &HashMap<SessionId, Box<ActiveConn>>,
    theme: &Theme,
) -> (SharedString, Option<Hsla>, Vec<SwitcherSection>) {
    use crate::connect::{fmt_ago, label_color};

    // The on-screen connection's stable id (for identity matching) and display
    // name (for the trigger label when it's not in the saved list), plus whether
    // it's still dialing — drives the "This window" badge and the trigger.
    let (active_id, active_name, connecting) = match phase {
        Phase::Connected(active) => (
            Some(active.conn_id.clone()),
            Some(active.config.name.clone()),
            false,
        ),
        Phase::Connecting(conn) => (
            Some(conn.conn_id.clone()),
            Some(conn.config.name.clone()),
            true,
        ),
        Phase::Disconnected => (None, None, false),
    };
    let active_index = active_id
        .as_ref()
        .and_then(|cid| connections.iter().position(|c| c.id == *cid));
    // Ids of connections backed by a warm parked session (instant to switch to).
    let warm_ids: std::collections::HashSet<&str> =
        parked.values().map(|a| a.conn_id.as_str()).collect();

    let warm_badge = SwitcherBadge::new("warm", theme.green);
    let row = |index: usize| -> SwitcherItem {
        let c = &connections[index];
        SwitcherItem::new(
            SharedString::from(format!("conn:{index}")),
            c.config.name.clone(),
        )
        .dot(label_color(c.config.color, theme))
    };

    let mut sections = Vec::new();

    // "This window" — the foreground connection, checkmarked, warm or connecting.
    if let Some(ai) = active_index {
        let badge = if connecting {
            SwitcherBadge::new("connecting", theme.yellow)
        } else {
            warm_badge.clone()
        };
        sections.push(SwitcherSection::new(
            "This window",
            vec![row(ai)
                .detail(connections[ai].config.display_target())
                .badge(badge)],
        ));
    }

    // "Open" — other warm connections, instant to switch to.
    let mut open: Vec<usize> = connections
        .iter()
        .enumerate()
        .filter(|(i, c)| Some(*i) != active_index && warm_ids.contains(c.id.as_str()))
        .map(|(i, _)| i)
        .collect();
    open.sort_by_key(|&i| std::cmp::Reverse(connections[i].last_accessed));
    if !open.is_empty() {
        let items = open
            .iter()
            .map(|&i| {
                row(i)
                    .detail(connections[i].config.display_target())
                    .badge(warm_badge.clone())
            })
            .collect();
        sections.push(SwitcherSection::new("Open", items));
    }

    // "Recent" — cold recents (no live session), newest first, capped.
    let mut recent: Vec<usize> = connections
        .iter()
        .enumerate()
        .filter(|(i, c)| {
            Some(*i) != active_index
                && !warm_ids.contains(c.id.as_str())
                && c.last_accessed.is_some()
        })
        .map(|(i, _)| i)
        .collect();
    recent.sort_by_key(|&i| std::cmp::Reverse(connections[i].last_accessed));
    recent.truncate(SWITCHER_RECENT_CAP);
    if !recent.is_empty() {
        let items = recent
            .into_iter()
            .map(|i| row(i).detail(fmt_ago(connections[i].last_accessed)))
            .collect();
        sections.push(SwitcherSection::new("Recent", items));
    }

    let (label, dot) = match active_index {
        Some(ai) => (
            connections[ai].config.name.clone(),
            Some(label_color(connections[ai].config.color, theme)),
        ),
        None => match active_name {
            Some(name) => (name, None),
            None => ("Connect…".into(), None),
        },
    };
    (label.into(), dot, sections)
}

/// The switcher's pinned footer actions — always visible beneath the
/// scrollable connection list.
pub(super) fn switcher_footer() -> Vec<SwitcherItem> {
    vec![
        SwitcherItem::new("action:new", "New connection…"),
        SwitcherItem::new("action:manage", "Manage connections…"),
    ]
}
