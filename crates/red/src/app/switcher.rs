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
        // Opening the popover only makes sense where its trigger is mounted (the
        // connected topbar anchors it). The welcome screen has no anchor — but it
        // *is* a richer, searchable connection manager, so ⌘P keeps one meaning
        // ("quickly pick a connection") by focusing its search box. The transient
        // connecting splash has neither, so there it's a no-op.
        if !matches!(self.phase, Phase::Connected(_)) {
            if matches!(self.phase, Phase::Disconnected) {
                let handle = self.connect_search.read(cx).focus_handle(cx);
                window.focus(&handle, cx);
            }
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
            self.switch_to_warm(warm, cx);
            return;
        }
        // Cold — open a new session (parking the current one warm).
        self.connect(index, cx);
    }

    /// Bring the warm parked session `id` to the foreground: refresh its
    /// connection's recency, park whatever was on screen, and swap it in instantly
    /// (no reconnect). Shared by the switcher's warm rows and the ⌘⇧P toggle.
    fn switch_to_warm(&mut self, id: SessionId, cx: &mut Context<Self>) {
        // Refresh recency for the switched-to connection (matched by stable id, so
        // it's robust to the saved list having been reordered).
        if let Some(conn_id) = self.parked.get(&id).map(|a| a.conn_id.clone()) {
            if let Some(stored) = self.connections.iter_mut().find(|c| c.id == conn_id) {
                stored.last_accessed = Some(config::now());
            }
            self.persist(cx);
        }
        self.park_foreground();
        self.foreground_parked(id, cx);
        self.rebuild_switcher(cx);
    }

    /// ⌘⇧P — flip to the previous connection: foreground the most-recently-used
    /// warm parked session. Because switching *away* stamps the outgoing
    /// connection as most-recent, this toggles A ⇄ B. A no-op when nothing's parked.
    pub(crate) fn switch_to_previous(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.parked_mru() {
            self.switch_to_warm(id, cx);
        }
    }

    /// ⌘1–⌘9 — jump to the `slot`-th (0-based) connection in the switcher's order
    /// (pinned, then this window, then open, then recent). Out-of-range is a no-op.
    pub(crate) fn switch_to_slot(&mut self, slot: usize, cx: &mut Context<Self>) {
        let order = switcher_order(&self.connections, &self.phase, &self.parked);
        if let Some(&index) = order.get(slot) {
            self.switch_to_connection(index, cx);
        }
    }
}

/// How many recent connections to surface in the switcher popover.
const SWITCHER_RECENT_CAP: usize = 8;

/// The saved connections sorted into the buckets the switcher draws, top to
/// bottom: `pinned` favourites (recency desc), the foreground `active` window,
/// other warm `open` sessions, and capped cold `recent` ones. One classification
/// shared by the section builder and the linear [`switcher_order`] the ⌘-digit
/// jumps index, so the digits always match what the popover shows.
struct SwitcherBuckets {
    /// Index of the foreground connection in `connections`, if it's saved.
    active: Option<usize>,
    /// Whether the foreground connection is still dialing (vs. live).
    connecting: bool,
    pinned: Vec<usize>,
    open: Vec<usize>,
    recent: Vec<usize>,
}

impl SwitcherBuckets {
    /// The connection indices in display / jump order: pinned, then the active
    /// window (when it isn't itself pinned), then open, then recent. Each once.
    fn order(&self) -> Vec<usize> {
        let mut order = self.pinned.clone();
        order.extend(self.this_window());
        order.extend(self.open.iter().copied());
        order.extend(self.recent.iter().copied());
        order
    }

    /// The active connection's index *if* it warrants its own "This window" row —
    /// it's saved and hasn't already floated into the pinned bucket.
    fn this_window(&self) -> Option<usize> {
        self.active.filter(|ai| !self.pinned.contains(ai))
    }
}

/// Sort the saved connections into [`SwitcherBuckets`] for the given phase + warm
/// parked sessions. Pure (no theme / UI), so the section builder and
/// [`switcher_order`] share one definition of "the order".
fn switcher_buckets(
    connections: &[StoredConnection],
    phase: &Phase,
    parked: &HashMap<SessionId, Box<ActiveConn>>,
) -> SwitcherBuckets {
    let (active_id, connecting) = match phase {
        Phase::Connected(active) => (Some(active.conn_id.as_str()), false),
        Phase::Connecting(conn) => (Some(conn.conn_id.as_str()), true),
        Phase::Disconnected => (None, false),
    };
    let active = active_id.and_then(|cid| connections.iter().position(|c| c.id == cid));
    // Ids of connections backed by a warm parked session (instant to switch to).
    let warm_ids: std::collections::HashSet<&str> =
        parked.values().map(|a| a.conn_id.as_str()).collect();

    // Pinned favourites float to the top regardless of state (recency desc).
    let mut pinned: Vec<usize> = connections
        .iter()
        .enumerate()
        .filter(|(_, c)| c.pinned)
        .map(|(i, _)| i)
        .collect();
    pinned.sort_by_key(|&i| std::cmp::Reverse(connections[i].last_accessed));

    // "Open" — non-pinned, non-active warm sessions (recency desc).
    let mut open: Vec<usize> = connections
        .iter()
        .enumerate()
        .filter(|(i, c)| !c.pinned && Some(*i) != active && warm_ids.contains(c.id.as_str()))
        .map(|(i, _)| i)
        .collect();
    open.sort_by_key(|&i| std::cmp::Reverse(connections[i].last_accessed));

    // "Recent" — non-pinned, non-active cold recents (recency desc, capped).
    let mut recent: Vec<usize> = connections
        .iter()
        .enumerate()
        .filter(|(i, c)| {
            !c.pinned
                && Some(*i) != active
                && !warm_ids.contains(c.id.as_str())
                && c.last_accessed.is_some()
        })
        .map(|(i, _)| i)
        .collect();
    recent.sort_by_key(|&i| std::cmp::Reverse(connections[i].last_accessed));
    recent.truncate(SWITCHER_RECENT_CAP);

    SwitcherBuckets {
        active,
        connecting,
        pinned,
        open,
        recent,
    }
}

/// The connection-index order the switcher shows top-to-bottom and the ⌘-digit
/// jumps map to. Stable across phases so a given digit keeps its target.
pub(super) fn switcher_order(
    connections: &[StoredConnection],
    phase: &Phase,
    parked: &HashMap<SessionId, Box<ActiveConn>>,
) -> Vec<usize> {
    switcher_buckets(connections, phase, parked).order()
}

/// Build the connection switcher's trigger (label + leading dot) and its sections
/// from the saved connections, the current phase, and the warm parked sessions.
/// Pinned favourites head the list, then the foreground "This window" connection,
/// then other warm "Open" sessions, then capped cold "Recent" ones. The first nine
/// rows carry their ⌘-digit quick-jump hint, making the popover the legend for
/// those shortcuts. The "New…" / "Manage…" actions live in the always-visible footer.
pub(super) fn switcher_sections(
    connections: &[StoredConnection],
    phase: &Phase,
    parked: &HashMap<SessionId, Box<ActiveConn>>,
    theme: &Theme,
) -> (SharedString, Option<Hsla>, Vec<SwitcherSection>) {
    use crate::connect::{fmt_ago, label_color};

    let buckets = switcher_buckets(connections, phase, parked);
    let warm_ids: std::collections::HashSet<&str> =
        parked.values().map(|a| a.conn_id.as_str()).collect();

    // The 1-based quick-jump slot of each of the first nine ordered connections,
    // so their rows can show the matching ⌘-digit hint.
    let slot_by_index: HashMap<usize, usize> = buckets
        .order()
        .into_iter()
        .take(9)
        .enumerate()
        .map(|(i, ix)| (ix, i + 1))
        .collect();

    let warm_badge = SwitcherBadge::new("warm", theme.green);
    let connecting_badge = SwitcherBadge::new("connecting", theme.yellow);

    // A row for connection `index`: its colour dot, name, and (for the first nine)
    // its quick-jump hint. The state cues are layered on by `decorate`.
    let row = |index: usize| -> SwitcherItem {
        let c = &connections[index];
        let mut item = SwitcherItem::new(
            SharedString::from(format!("conn:{index}")),
            c.config.name.clone(),
        )
        .dot(label_color(c.config.color, theme));
        if let Some(&n) = slot_by_index.get(&index) {
            item = item.kbd(crate::keymap::localize_hint(&format!("⌘{n}")));
        }
        item
    };

    // Layer the state cues onto a row — the active checkmark + warm/connecting
    // badge, a warm badge + target for an open session, else a recency line — so a
    // pinned row reads the same as it would under This window / Open / Recent.
    let decorate = |index: usize| -> SwitcherItem {
        let item = row(index);
        if buckets.active == Some(index) {
            item.detail(connections[index].config.display_target())
                .badge(if buckets.connecting {
                    connecting_badge.clone()
                } else {
                    warm_badge.clone()
                })
                .checked(true)
        } else if warm_ids.contains(connections[index].id.as_str()) {
            item.detail(connections[index].config.display_target())
                .badge(warm_badge.clone())
        } else {
            item.detail(fmt_ago(connections[index].last_accessed))
        }
    };

    let mut sections = Vec::new();
    if !buckets.pinned.is_empty() {
        let items = buckets.pinned.iter().map(|&i| decorate(i)).collect();
        sections.push(SwitcherSection::new("Pinned", items));
    }
    if let Some(ai) = buckets.this_window() {
        sections.push(SwitcherSection::new("This window", vec![decorate(ai)]));
    }
    if !buckets.open.is_empty() {
        let items = buckets.open.iter().map(|&i| decorate(i)).collect();
        sections.push(SwitcherSection::new("Open", items));
    }
    if !buckets.recent.is_empty() {
        let items = buckets.recent.iter().map(|&i| decorate(i)).collect();
        sections.push(SwitcherSection::new("Recent", items));
    }

    // Trigger label + dot: the foreground connection's name/colour, else its
    // display name while connecting/unsaved, else a neutral prompt.
    let (label, dot) = match buckets.active {
        Some(ai) => (
            connections[ai].config.name.clone(),
            Some(label_color(connections[ai].config.color, theme)),
        ),
        None => match phase {
            Phase::Connecting(conn) => (conn.config.name.clone(), None),
            Phase::Connected(active) => (active.config.name.clone(), None),
            Phase::Disconnected => ("Connect…".into(), None),
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
