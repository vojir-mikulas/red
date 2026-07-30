//! Watch mode: re-run this tab's query every N seconds, flash what changed, and
//! report the row delta (see `docs/plans/todo/watch-mode.md`).
//!
//! Redis has had auto-refresh since the browse-topbar overhaul; the SQL grid had
//! no refresh at all (⌘R is *Refresh schema*). The archetype is a job/queue
//! table: you want to watch rows drain without losing your scroll position and
//! without hammering ⌘↵.
//!
//! **A tick re-runs the query**, it does not re-fetch the resident window. The
//! cheaper option cannot see a row inserted outside the window, which is exactly
//! the change being watched for, and it would show a stale total while looking
//! authoritative. Re-running reuses the epoch swap that a sort or filter change
//! already performs ([`ResultGrid::reopen_spec`]), so this is a reuse rather than
//! a new path, and the old result is closed on every tick so a watch left on
//! overnight cannot park thousands of cursors.
//!
//! The timer is the generation-guarded `background_executor().timer` pattern from
//! `kv_arm_auto_refresh`, keyed by **epoch** rather than tab index: a tab index
//! shifts when a tab to its left closes, and a watch that fires against the wrong
//! tab is worse than one that stops.

use std::collections::HashMap;
use std::time::Duration;

use gpui::{AsyncApp, Context, WeakEntity};
use red_core::Value;
use red_service::Command;

use crate::app::{AppState, Phase};

use super::ResultGrid;

/// How long a changed cell stays tinted. Long enough to catch the eye on a 5s
/// watch, short enough that two ticks never overlap at the 2s floor.
pub(crate) const FLASH: Duration = Duration::from_millis(1_200);

/// Cap on tracked changed cells. A tick where everything changed is not
/// information, and an uncapped map on a wide grid is an allocation per cell per
/// tick. Past this the tick reports "fully refreshed" and highlights nothing.
const MAX_CHANGED: usize = 5_000;

/// The intervals the watch menu offers. `None` is off.
pub(crate) const CHOICES: [Option<u64>; 6] = [None, Some(2), Some(5), Some(10), Some(30), Some(60)];

/// One tab's live watch.
pub(crate) struct Watch {
    pub interval: Duration,
    /// Bumped on every interval change or stop, so a timer armed under the old
    /// setting exits instead of double-firing.
    pub generation: u64,
    /// True while a tick's re-open is in flight, so a query slower than the
    /// interval does not stack round trips.
    pub inflight: bool,
    /// Consecutive failed ticks. Three stops the watch with one toast; a watch on
    /// a just-dropped table must not toast every 2 seconds forever.
    pub errors: u32,
    /// Paused because the tab cannot currently be watched safely (unsaved edits)
    /// or usefully (window in the background). The timer keeps running; the tick
    /// is skipped, so resuming needs no re-arm.
    pub paused: Option<&'static str>,
    /// Row identity → per-cell digest, captured just before a tick's re-open and
    /// compared when the rows land.
    prev: HashMap<WatchKey, Vec<u64>>,
    /// `(row identity, data column)` → when it changed, for the fade.
    pub changed: HashMap<(WatchKey, usize), std::time::Instant>,
    /// True when the last tick changed more cells than [`MAX_CHANGED`].
    pub churned: bool,
    /// Total row count before the last tick, for the delta chip.
    prev_total: Option<usize>,
    pub last_delta: Option<i64>,
}

/// How a row is recognised across a re-run.
///
/// A keyed result compares by its seek key, so a row that merely *moved* is not
/// reported as changed. Without a key the only available identity is the
/// ordinal, which is honest but position-sensitive, and the UI says so.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) enum WatchKey {
    Keyed(String),
    Ordinal(usize),
}

impl Watch {
    fn new(interval: Duration) -> Self {
        Self {
            interval,
            generation: 0,
            inflight: false,
            errors: 0,
            paused: None,
            prev: HashMap::new(),
            changed: HashMap::new(),
            churned: false,
            prev_total: None,
            last_delta: None,
        }
    }

    /// Whether `(row, col)` is still inside its flash window.
    pub(crate) fn is_flashing(&self, key: &WatchKey, col: usize, now: std::time::Instant) -> bool {
        self.changed
            .get(&(key.clone(), col))
            .is_some_and(|at| now.duration_since(*at) < FLASH)
    }

    /// Drop flashes that have faded, so the map does not grow across ticks.
    fn expire(&mut self, now: std::time::Instant) {
        self.changed.retain(|_, at| now.duration_since(*at) < FLASH);
    }
}

/// A digest of one cell, cheap to store and compare. A hash rather than the value
/// because a resident window of wide rows is exactly the thing not to clone every
/// tick; a collision costs one missed highlight, never a wrong result.
fn digest(value: &Value) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    // `Value` is not `Hash` (it carries floats), so hash its rendered form; that
    // is also what "changed" means to the user reading the grid.
    format!("{value:?}").hash(&mut h);
    h.finish()
}

impl ResultGrid {
    /// Snapshot the resident rows as identity → per-cell digests, for the
    /// change comparison after a re-run.
    pub(in crate::result) fn watch_snapshot(&self) -> HashMap<WatchKey, Vec<u64>> {
        let buffer = self.buffer.borrow();
        let key_cols = self.watch_key_cols();
        let mut out = HashMap::new();
        // Only the resident rows, not `0..total`: `total` can be 50M while the
        // buffer holds at most a window, so the old loop stalled ~0.5-1s per
        // tick skipping the non-resident majority.
        buffer.for_each_resident(|ord, row| {
            let id = match &key_cols {
                Some(cols) => WatchKey::Keyed(
                    cols.iter()
                        .filter_map(|&c| row.values.get(c))
                        .map(|v| format!("{v:?}"))
                        .collect::<Vec<_>>()
                        .join("\u{1}"),
                ),
                None => WatchKey::Ordinal(ord),
            };
            out.insert(id, row.values.iter().map(digest).collect());
        });
        out
    }

    /// The data-column indices of this result's seek key, when it has one whose
    /// columns are all present in the projection.
    fn watch_key_cols(&self) -> Option<Vec<usize>> {
        let key = self.key.as_ref()?;
        // The seek key is a lead column plus an optional PK tiebreaker; together
        // they are the row identity the backend already guarantees is unique.
        let names: Vec<&String> = std::iter::once(&key.column)
            .chain(key.tiebreak.iter())
            .collect();
        let cols: Vec<usize> = names
            .iter()
            .filter_map(|name| self.columns.iter().position(|c| &&c.name == name))
            .collect();
        (cols.len() == names.len() && !cols.is_empty()).then_some(cols)
    }

    /// The identity of the resident row at `ord`, for the render's flash lookup.
    pub(in crate::result) fn watch_row_key(&self, ord: usize) -> WatchKey {
        match self.watch_key_cols() {
            Some(cols) => {
                let buffer = self.buffer.borrow();
                match buffer.row(ord) {
                    Some(row) => WatchKey::Keyed(
                        cols.iter()
                            .filter_map(|&c| row.values.get(c))
                            .map(|v| format!("{v:?}"))
                            .collect::<Vec<_>>()
                            .join("\u{1}"),
                    ),
                    None => WatchKey::Ordinal(ord),
                }
            }
            None => WatchKey::Ordinal(ord),
        }
    }
}

impl AppState {
    /// Whether the active tab can be watched at all: a single read-only statement
    /// with an open result behind it.
    ///
    /// A watch re-runs its statement unattended, so anything that writes is
    /// refused outright rather than confirmed. This is the one guard that is a
    /// hard refusal instead of a pause.
    pub(crate) fn watch_allowed(&self, cx: &Context<Self>) -> Result<(), &'static str> {
        let Phase::Connected(active) = &self.phase else {
            return Err("not connected");
        };
        let Some(tab) = active.active() else {
            return Err("no tab");
        };
        if tab.is_view() {
            return Err("this tab has no query to re-run");
        }
        if tab.result.is_none() {
            return Err("run the query once before watching it");
        }
        let sql = tab.editor.read(cx).content();
        let sql = sql.trim();
        // A table browse's editor holds the generated SELECT, so both paths land
        // on the same check.
        let dialect = crate::sql::Dialect::of(active.config.kind);
        if !sql.is_empty() && !crate::sql::is_read_only(sql, dialect) {
            return Err("watch only re-runs read-only queries");
        }
        if crate::sql::statement_count(sql, dialect) > 1 {
            return Err("watch needs a single statement");
        }
        Ok(())
    }

    /// Set (or clear) the active tab's watch interval and arm its timer.
    pub(crate) fn set_watch(&mut self, secs: Option<u64>, cx: &mut Context<Self>) {
        if let Some(secs) = secs
            && let Err(why) = self.watch_allowed(cx)
        {
            let _ = secs;
            self.notify(flint::ToastVariant::Warning, why, cx);
            return;
        }
        // The floor is higher on production: a 2s watch against prod is a
        // self-inflicted load generator, and the setting's own floor is the
        // general case of the same argument.
        let floor = match &self.phase {
            Phase::Connected(a) if a.config.env == red_core::ConnEnv::Prod => {
                self.settings.sql.watch_min_secs.max(10)
            }
            _ => self.settings.sql.watch_min_secs,
        };
        let interval = secs.map(|s| Duration::from_secs(s.max(floor)));

        let armed = if let Phase::Connected(active) = &mut self.phase
            && let Some(tab) = active.active_mut()
        {
            match interval {
                Some(interval) => {
                    let generation = tab.watch.as_ref().map_or(0, |w| w.generation) + 1;
                    let mut watch = Watch::new(interval);
                    watch.generation = generation;
                    let epoch = tab.result.as_ref().map(|g| g.epoch);
                    tab.watch = Some(watch);
                    epoch.map(|e| (e, interval, generation))
                }
                None => {
                    // Bumping the generation on the way out is what stops the
                    // already-armed timer.
                    if let Some(w) = &mut tab.watch {
                        w.generation = w.generation.wrapping_add(1);
                    }
                    tab.watch = None;
                    None
                }
            }
        } else {
            None
        };
        if let Some((epoch, interval, generation)) = armed {
            self.arm_watch(epoch, interval, generation, cx);
        }
        cx.notify();
    }

    /// Arm one tick for the result identified by `epoch`.
    ///
    /// Routed by epoch, not tab index: the tab may move (a tab to its left
    /// closes, a drag across the split), and a tick that lands on the wrong tab
    /// would re-run someone else's query. A missing epoch means the tab or its
    /// result is gone, and the timer simply exits.
    fn arm_watch(
        &mut self,
        epoch: red_service::Epoch,
        interval: Duration,
        generation: u64,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            cx.background_executor().timer(interval).await;
            this.update(cx, |this, cx| this.watch_tick(epoch, generation, cx))
                .ok();
        })
        .detach();
    }

    /// One tick: re-run the watched result, unless something says not to.
    fn watch_tick(&mut self, epoch: red_service::Epoch, generation: u64, cx: &mut Context<Self>) {
        // Window in the background: keep the timer, skip the work. A minimised
        // Red should not keep a production database busy.
        let visible = cx.windows().iter().any(|w| {
            w.update(cx, |_, window, _| window.is_window_active())
                .unwrap_or(false)
        });

        let Some((tab_ix, session)) = self.watch_tab_for(epoch) else {
            return;
        };
        let Phase::Connected(active) = &mut self.phase else {
            return;
        };
        let Some(tab) = active.tabs.get_mut(tab_ix) else {
            return;
        };
        let Some(watch) = &mut tab.watch else { return };
        // Superseded: the interval changed or the watch stopped.
        if watch.generation != generation {
            return;
        }
        let interval = watch.interval;
        watch.expire(std::time::Instant::now());

        // Staged edits would be destroyed by a re-open, so the watch holds rather
        // than quietly discarding the user's uncommitted work.
        let staged = tab.result.as_ref().is_some_and(|g| !g.pending.is_empty());
        watch.paused = if !visible {
            Some("paused: window in the background")
        } else if staged {
            Some("paused: unsaved edits")
        } else if watch.inflight {
            Some("waiting for the previous run")
        } else {
            None
        };

        let reopen = if watch.paused.is_none() {
            watch.inflight = true;
            let snapshot = tab.result.as_ref().map(|g| g.watch_snapshot());
            let total = tab.result.as_ref().map(|g| g.total);
            if let (Some(snapshot), Some(w)) = (snapshot, tab.watch.as_mut()) {
                w.prev = snapshot;
                w.prev_total = total;
            }
            tab.result.as_mut().map(|g| g.reopen_spec())
        } else {
            None
        };
        let next_epoch_for_arm = tab.result.as_ref().map_or(epoch, |g: &ResultGrid| g.epoch);
        // The *watched tab's* namespace, not `send_namespace()`'s focused-tab
        // one: tab 1 watching `sales` must re-open against `sales` even while
        // tab 2 (focused on `staging`) is on screen.
        let namespace = if active.config.kind.namespace_caps().settable {
            tab.namespace.clone().or_else(|| active.namespace.clone())
        } else {
            None
        };

        if let Some((sql, new_epoch, table, sort, filter, joins, old_epoch)) = reopen {
            // Close the superseded cursor every tick; without this a watch left
            // running overnight parks a result per tick and hits the open-result
            // cap.
            self.service
                .send_to(session, Command::CloseResult { epoch: old_epoch });
            self.service.send_to(
                session,
                Command::OpenResult {
                    sql,
                    epoch: new_epoch,
                    table,
                    sort,
                    filter,
                    joins,
                    namespace,
                },
            );
        }
        // Re-arm against whatever epoch the result now carries.
        self.arm_watch(next_epoch_for_arm, interval, generation, cx);
        cx.notify();
    }

    /// Find the tab holding the result `epoch`, across every connection, as
    /// `(tab index, session)`.
    fn watch_tab_for(&self, epoch: red_service::Epoch) -> Option<(usize, red_service::SessionId)> {
        let Phase::Connected(active) = &self.phase else {
            return None;
        };
        let ix = active
            .tabs
            .iter()
            .position(|t| t.result.as_ref().is_some_and(|g| g.epoch == epoch))?;
        Some((ix, active.session))
    }

    /// Rows arrived for a watched result: diff them against the pre-tick snapshot
    /// and record the flashes plus the row delta.
    pub(crate) fn watch_rows_landed(
        &mut self,
        session: Option<red_service::SessionId>,
        epoch: red_service::Epoch,
    ) {
        let Some(active) = self.conn_mut(session) else {
            return;
        };
        let Some(tab) = active
            .tabs
            .iter_mut()
            .find(|t| t.result.as_ref().is_some_and(|g| g.epoch == epoch))
        else {
            return;
        };
        let (Some(grid), Some(watch)) = (tab.result.as_ref(), tab.watch.as_mut()) else {
            return;
        };
        watch.inflight = false;
        watch.errors = 0;

        let now = std::time::Instant::now();
        let fresh = grid.watch_snapshot();
        let mut changed = 0usize;
        for (id, cells) in &fresh {
            let Some(before) = watch.prev.get(id) else {
                // A row that was not there before is an insert, not a cell edit;
                // the delta chip reports it and the grid does not flash it.
                continue;
            };
            for (col, hash) in cells.iter().enumerate() {
                if before.get(col) != Some(hash) {
                    changed += 1;
                    if changed <= MAX_CHANGED {
                        watch.changed.insert((id.clone(), col), now);
                    }
                }
            }
        }
        watch.churned = changed > MAX_CHANGED;
        if watch.churned {
            watch.changed.clear();
        }
        watch.last_delta = watch
            .prev_total
            .map(|before| grid.total as i64 - before as i64);
        watch.prev.clear();
    }

    /// A watched result failed to re-open: count it, and stop after three so a
    /// dropped table does not toast forever.
    pub(crate) fn watch_run_failed(
        &mut self,
        session: Option<red_service::SessionId>,
        epoch: red_service::Epoch,
        cx: &mut Context<Self>,
    ) {
        let Some(active) = self.conn_mut(session) else {
            return;
        };
        let Some(tab) = active
            .tabs
            .iter_mut()
            .find(|t| t.result.as_ref().is_some_and(|g| g.epoch == epoch))
        else {
            return;
        };
        let Some(watch) = tab.watch.as_mut() else {
            return;
        };
        watch.inflight = false;
        watch.errors += 1;
        if watch.errors >= 3 {
            watch.generation = watch.generation.wrapping_add(1);
            tab.watch = None;
            self.notify(
                flint::ToastVariant::Warning,
                "Watch stopped after three failed runs.",
                cx,
            );
        }
    }
}

// --- UI ----------------------------------------------------------------------

impl AppState {
    /// The run-bar watch pill: a toggle plus an interval caret, drawn to match the
    /// Redis browse's auto-refresh pill so the same control does not read as two
    /// different features on the two seams.
    ///
    /// Absent on a tab that cannot be watched (a diagram, a DDL view, a tab that
    /// has never run), so the control appears exactly where it would work.
    pub(crate) fn render_watch_pill(
        &self,
        active: &crate::app::ActiveConn,
        tab_idx: usize,
        half: crate::app::SplitHalf,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        use flint::Tooltip;
        use flint::prelude::*;
        use gpui::{MouseButton, div, prelude::*, px};

        let tab = active.tabs.get(tab_idx)?;
        if tab.is_view() || tab.result.is_none() {
            return None;
        }
        let theme = cx.theme().clone();
        let watch = tab.watch.as_ref();
        let on = watch.is_some();
        let secs = watch.map(|w| w.interval.as_secs());
        let hue = if on { theme.accent } else { theme.text_muted };
        let paused = watch.and_then(|w| w.paused);
        let delta = watch.and_then(|w| w.last_delta).filter(|d| *d != 0);
        let churned = watch.is_some_and(|w| w.churned);

        let toggle_view = cx.entity().downgrade();
        let caret_view = cx.entity().downgrade();
        let default_secs = self.settings.sql.watch_default_secs.max(5);

        let mut pill = div()
            .flex()
            .items_center()
            .h(px(24.))
            .rounded(px(5.))
            .border_1()
            .border_color(if on { theme.accent } else { theme.border })
            .bg(theme.bg_elevated)
            .text_size(theme.scale(12.))
            .child(
                div()
                    .id("sql-watch-toggle")
                    .flex()
                    .items_center()
                    .gap_1()
                    .h_full()
                    .px_1p5()
                    .cursor_pointer()
                    .text_color(hue)
                    .hover(|s| s.bg(theme.bg_hover))
                    .tooltip(Tooltip::text(match (on, paused) {
                        (true, Some(why)) => why,
                        (true, None) => "Watch on — click to stop",
                        (false, _) => "Watch off — re-run this query on an interval",
                    }))
                    .child(crate::icons::icon("refresh-cw", theme.scale(13.), hue))
                    .when_some(secs, |s, secs| s.child(format!("{secs}s")))
                    .on_click(move |_, _, cx| {
                        toggle_view
                            .update(cx, |this, cx| {
                                this.set_split_focus(half, cx);
                                let next = if on { None } else { Some(default_secs) };
                                this.set_watch(next, cx);
                            })
                            .ok();
                    }),
            )
            .child(div().w(px(1.)).h(px(14.)).bg(theme.border))
            .child(
                div()
                    .id("sql-watch-caret")
                    .flex()
                    .items_center()
                    .h_full()
                    .px_1()
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.bg_hover))
                    .tooltip(Tooltip::text("Watch interval"))
                    .child(crate::icons::icon(
                        "chevron-down",
                        theme.scale(12.),
                        theme.text_muted,
                    ))
                    .on_mouse_down(
                        MouseButton::Left,
                        move |event: &gpui::MouseDownEvent, _, cx| {
                            let pos = event.position;
                            caret_view
                                .update(cx, |this, cx| this.open_watch_menu(pos, cx))
                                .ok();
                        },
                    ),
            );

        // The row delta since the previous tick, next to the pill. Honest about
        // its own basis: a keyed result counts rows, an unkeyed one counts
        // positions, and a churned tick says so instead of showing a number.
        if churned {
            pill = pill.child(
                div()
                    .ml_1()
                    .text_size(theme.scale(10.5))
                    .text_color(theme.text_faint)
                    .child(crate::i18n::tr!(
                        "result.watch_fully_refreshed",
                        "fully refreshed"
                    )),
            );
        } else if let Some(d) = delta {
            let (label, colour) = if d > 0 {
                (format!("+{d}"), theme.green)
            } else {
                (d.to_string(), theme.red)
            };
            pill = pill.child(
                div()
                    .ml_1()
                    .text_size(theme.scale(10.5))
                    .text_color(colour)
                    .child(label),
            );
        }
        Some(
            div()
                .flex()
                .items_center()
                .gap_1()
                .child(pill)
                .into_any_element(),
        )
    }

    /// Open the watch-interval menu at `pos`. Mirrors the Redis auto-refresh
    /// interval popover, including the checked current choice.
    fn open_watch_menu(&mut self, pos: gpui::Point<gpui::Pixels>, cx: &mut Context<Self>) {
        self.watch_menu = Some(pos);
        cx.notify();
    }

    /// Render the open watch-interval menu, from the shell root.
    pub(crate) fn render_watch_menu(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        use flint::{ContextMenu, ContextMenuItem, floating};
        use gpui::{MouseButton, div, prelude::*};

        let pos = self.watch_menu?;
        let current = match &self.phase {
            Phase::Connected(a) => a
                .active()
                .and_then(|t| t.watch.as_ref())
                .map(|w| w.interval.as_secs()),
            _ => None,
        };
        let mut menu = ContextMenu::new("sql-watch-menu");
        for choice in CHOICES {
            let label = match choice {
                None => "Off".to_string(),
                Some(s) if s < 60 => format!("{s}s"),
                Some(s) => format!("{}m", s / 60),
            };
            let label = if choice == current {
                format!("✓ {label}")
            } else {
                label
            };
            let id = format!("sql-watch-{}", choice.unwrap_or(0));
            menu = menu.item(ContextMenuItem::new(id, label).on_click(cx.listener(
                move |this, _, _, cx| {
                    this.watch_menu = None;
                    this.set_watch(choice, cx);
                },
            )));
        }
        Some(
            div()
                .absolute()
                .inset_0()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.watch_menu = None;
                        cx.notify();
                    }),
                )
                .child(floating(div().occlude().child(menu)).at(pos))
                .into_any_element(),
        )
    }
}

impl AppState {
    /// Palette / keybinding entry point: start watching at the default interval,
    /// or stop if already watching.
    pub(crate) fn toggle_watch(&mut self, cx: &mut Context<Self>) {
        let on = match &self.phase {
            Phase::Connected(a) => a.active().is_some_and(|t| t.watch.is_some()),
            _ => false,
        };
        let next = (!on).then(|| self.settings.sql.watch_default_secs.max(5));
        self.set_watch(next, cx);
    }
}
