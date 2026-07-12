//! The Redis command console: a `redis-cli`-style REPL panel (see
//! docs/plans/redis.md). A reply is matched to the most recent history entry
//! still awaiting one with the same `argv`, on the assumption a human types
//! one command, waits, reads the reply, then types the next — good enough
//! without per-command ids; a burst of rapid-fire submissions could in
//! principle match a reply to the wrong entry.

use gpui::{div, prelude::*, px, Context, Entity, ScrollHandle, Window};
use red_core::kv::{classify_command, tokenize_command, CommandClass, RespValue};
use red_service::{Command, SessionId};

use flint::prelude::*;

use crate::app::{ActiveConn, AppState, TabWorkspace};

/// Cap on retained console entries. The panel re-renders every entry each frame
/// and keeps them all resident, so an unbounded log would grow render cost and
/// memory with the whole session's command count; the oldest are dropped past
/// this (like the other live panels' `MAX_LINES`/`MAX_MESSAGES` caps).
const MAX_CONSOLE_HISTORY: usize = 500;

/// One submitted command line and its reply, once it arrives.
pub(crate) struct KvConsoleEntry {
    pub(crate) argv: Vec<String>,
    /// `None` while in flight.
    pub(crate) result: Option<Result<RespValue, String>>,
}

pub(crate) struct KvConsole {
    pub(crate) epoch: u64,
    pub(crate) input: Entity<TextInput>,
    pub(crate) history: Vec<KvConsoleEntry>,
    /// A destructive command (`classify_command` says so) waits here for an
    /// explicit confirm before it's actually sent.
    pub(crate) pending_confirm: Option<Vec<String>>,
    /// Up/Down command recall: an index into `history` while browsing past
    /// commands, `None` when editing a fresh line. Reset when a command runs.
    pub(crate) recall: Option<usize>,
    pub(crate) scroll: ScrollHandle,
}

impl KvConsole {
    pub(crate) fn new(session: SessionId, cx: &mut Context<AppState>) -> Self {
        let input = cx.new(|cx| {
            TextInput::new(cx).with_placeholder("Type a command, e.g. GET mykey, then Enter…")
        });
        cx.subscribe(&input, move |this, input, event: &TextInputEvent, cx| {
            if matches!(event, TextInputEvent::Submit) {
                let line = input.read(cx).content().to_string();
                input.update(cx, |ti, cx| ti.set_content("", cx));
                this.kv_console_submit(session, line, cx);
            }
        })
        .detach();
        Self {
            epoch: crate::result::next_kv_epoch(),
            input,
            history: Vec::new(),
            pending_confirm: None,
            recall: None,
            scroll: ScrollHandle::new(),
        }
    }
}

impl AppState {
    /// A line was submitted: tokenize it, and either send it straight away
    /// (read/write) or park it behind a confirm (destructive).
    pub(crate) fn kv_console_submit(
        &mut self,
        session: SessionId,
        line: String,
        cx: &mut Context<Self>,
    ) {
        let argv = tokenize_command(&line);
        if argv.is_empty() {
            return;
        }
        let Some(console) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_console_mut())
        else {
            return;
        };
        if classify_command(&argv) == CommandClass::Destructive {
            console.pending_confirm = Some(argv);
            cx.notify();
            return;
        }
        self.kv_console_send(session, argv, cx);
    }

    pub(crate) fn kv_console_confirm(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(console) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_console_mut())
        else {
            return;
        };
        let Some(argv) = console.pending_confirm.take() else {
            return;
        };
        self.kv_console_send(session, argv, cx);
    }

    pub(crate) fn kv_console_cancel_confirm(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(console) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_console_mut())
        else {
            return;
        };
        console.pending_confirm = None;
        cx.notify();
    }

    fn kv_console_send(&mut self, session: SessionId, argv: Vec<String>, cx: &mut Context<Self>) {
        // Log the executed command in the shared query-history store (the same
        // store SQL uses — it's just text keyed by `conn_id`). The History dock's
        // Commands section reads it back.
        let conn_id = self
            .conn_mut(Some(session))
            .map(|a| a.conn_id.clone())
            .unwrap_or_default();
        if !conn_id.is_empty() {
            self.query_history.record(&conn_id, &argv.join(" "));
        }
        let Some(console) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_console_mut())
        else {
            return;
        };
        let epoch = console.epoch;
        console.recall = None;
        console.history.push(KvConsoleEntry {
            argv: argv.clone(),
            result: None,
        });
        if console.history.len() > MAX_CONSOLE_HISTORY {
            let drop = console.history.len() - MAX_CONSOLE_HISTORY;
            console.history.drain(0..drop);
        }
        self.service
            .send_to(session, Command::KvCommand { epoch, argv });
        cx.notify();
    }

    /// Walk command history in the input line: `prev` (Up) steps to older
    /// commands, `!prev` (Down) to newer, past the newest clearing the line.
    /// Seeds but never runs — Enter runs it, matching shell recall.
    pub(crate) fn kv_console_recall(
        &mut self,
        session: SessionId,
        prev: bool,
        cx: &mut Context<Self>,
    ) {
        let Some((content, input)) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_console_mut())
            .and_then(|console| {
                let n = console.history.len();
                if n == 0 {
                    return None;
                }
                let new_idx = match (console.recall, prev) {
                    (None, true) => Some(n - 1),
                    (None, false) => return None, // Down with a fresh line: nothing to do
                    (Some(i), true) => Some(i.saturating_sub(1)),
                    (Some(i), false) if i + 1 >= n => None,
                    (Some(i), false) => Some(i + 1),
                };
                console.recall = new_idx;
                let content = new_idx
                    .map(|i| console.history[i].argv.join(" "))
                    .unwrap_or_default();
                Some((content, console.input.clone()))
            })
        else {
            return;
        };
        input.update(cx, |ti, cx| ti.set_content(&content, cx));
        cx.notify();
    }

    /// Seed `text` into a Console tab's input (the History dock's Commands
    /// click). Ensures the focused half shows a Console tab, sets the input, and
    /// leaves it for the user to run — never auto-executes (a logged command may
    /// be destructive), mirroring the SQL history's seed-don't-run behaviour.
    pub(crate) fn kv_seed_console(
        &mut self,
        session: SessionId,
        text: String,
        cx: &mut Context<Self>,
    ) {
        use crate::kvbrowse::{KvPanel, RedisTabState};
        let is_console = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_ref())
            .is_some_and(|v| matches!(v.active_state(), Some(RedisTabState::Console(_))));
        if !is_console {
            self.kv_new_empty_tab(session, cx);
            let id = self
                .conn_mut(Some(session))
                .and_then(|a| a.kv_view.as_ref())
                .and_then(|v| v.tabs.get(v.focused_tab_index()))
                .map(|t| t.id);
            if let Some(id) = id {
                self.kv_set_tab_kind(session, id, KvPanel::Console, cx);
            }
        }
        let input = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_console_mut())
            .map(|c| c.input.clone());
        if let Some(input) = input {
            input.update(cx, |ti, cx| ti.set_content(&text, cx));
        }
        cx.notify();
    }

    /// `Event::KvCommandResult`: fill in the most recent still-pending
    /// history entry with a matching `argv` (see the module doc comment on
    /// why this is enough without per-command ids).
    pub(crate) fn on_kv_command_result(
        &mut self,
        session: Option<SessionId>,
        epoch: u64,
        argv: Vec<String>,
        result: RespValue,
        cx: &mut Context<Self>,
    ) {
        // Route by epoch: the reply may target a background console tab.
        let Some(console) = self
            .conn_mut(session)
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.console_by_epoch_mut(epoch))
        else {
            return;
        };
        if let Some(entry) = console
            .history
            .iter_mut()
            .rev()
            .find(|e| e.result.is_none() && e.argv == argv)
        {
            entry.result = Some(Ok(result));
        }
        cx.notify();
    }
}

/// Render one RESP reply as console text, `redis-cli`-ish: arrays indent
/// their elements one per line, everything else is one line.
fn fmt_resp(value: &RespValue, depth: usize) -> String {
    let pad = "  ".repeat(depth);
    match value {
        RespValue::Nil => format!("{pad}(nil)"),
        RespValue::Ok => format!("{pad}OK"),
        RespValue::Int(i) => format!("{pad}(integer) {i}"),
        RespValue::Double(d) => format!("{pad}(double) {d}"),
        RespValue::Bool(b) => format!("{pad}(boolean) {b}"),
        RespValue::Simple(s) => format!("{pad}{s}"),
        RespValue::Bulk(s) => format!("{pad}\"{s}\""),
        RespValue::Error(e) => format!("{pad}(error) {e}"),
        RespValue::Array(items) if items.is_empty() => format!("{pad}(empty array)"),
        RespValue::Array(items) => items
            .iter()
            .enumerate()
            .map(|(i, v)| format!("{pad}{}) {}", i + 1, fmt_resp(v, 0).trim_start()))
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

impl AppState {
    /// The console panel: history log + input line, with an inline confirm
    /// for a destructive command. Called from `render_redis_shell`.
    pub(crate) fn render_kv_console(
        &self,
        active: &ActiveConn,
        tab_idx: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let session = active.session;
        let view = cx.entity().downgrade();
        let Some(console) = active.kv_view.as_ref().and_then(|v| v.console_at(tab_idx)) else {
            return div().flex_1();
        };

        let mono = theme.mono_family.clone();
        let text_size = theme.scale(11.5);
        let dim = theme.text_muted;

        let mut entries: Vec<gpui::AnyElement> = Vec::new();
        for entry in &console.history {
            let line = entry.argv.join(" ");
            entries.push(
                div()
                    .font_family(mono.clone())
                    .text_size(text_size)
                    .text_color(theme.text)
                    .child(format!("> {line}"))
                    .into_any_element(),
            );
            let body = match &entry.result {
                None => "…".to_string(),
                Some(Ok(v)) => fmt_resp(v, 0),
                Some(Err(e)) => format!("(error) {e}"),
            };
            entries.push(
                div()
                    .font_family(mono.clone())
                    .text_size(text_size)
                    .text_color(dim)
                    .pb_2()
                    .child(body)
                    .into_any_element(),
            );
        }

        let history = div()
            .id("kv-console-history")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .track_scroll(&console.scroll)
            .p_2()
            .flex()
            .flex_col()
            .children(entries);

        let confirm_bar = console.pending_confirm.as_ref().map(|argv| {
            let confirm_view = view.clone();
            let cancel_view = view.clone();
            div()
                .flex_shrink_0()
                .flex()
                .items_center()
                .gap_2()
                .px_2()
                .py_1p5()
                .bg(theme.red.opacity(0.1))
                .border_t_1()
                .border_color(theme.red)
                .child(
                    div()
                        .flex_1()
                        .text_size(theme.scale(11.5))
                        .text_color(theme.text)
                        .child(format!("Run \"{}\"? This can't be undone.", argv.join(" "))),
                )
                .child(
                    Button::new("kv-console-confirm", "Run")
                        .variant(ButtonVariant::Danger)
                        .size(ButtonSize::Sm)
                        .on_click(move |_, _, cx| {
                            confirm_view
                                .update(cx, |this, cx| this.kv_console_confirm(session, cx))
                                .ok();
                        }),
                )
                .child(
                    Button::new("kv-console-cancel", "Cancel")
                        .size(ButtonSize::Sm)
                        .on_click(move |_, _, cx| {
                            cancel_view
                                .update(cx, |this, cx| this.kv_console_cancel_confirm(session, cx))
                                .ok();
                        }),
                )
        });

        div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .child(history)
            .children(confirm_bar)
            .child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .px_2()
                    .py_1p5()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(120.))
                            // Up/Down recall past commands into the input; the
                            // single-line input doesn't use the vertical arrows.
                            .on_key_down(cx.listener(
                                move |this, ev: &gpui::KeyDownEvent, _w, cx| {
                                    match ev.keystroke.key.as_str() {
                                        "up" => this.kv_console_recall(session, true, cx),
                                        "down" => this.kv_console_recall(session, false, cx),
                                        _ => {}
                                    }
                                },
                            ))
                            .child(console.input.clone()),
                    ),
            )
    }
}
