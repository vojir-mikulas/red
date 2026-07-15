//! The Redis command console: a `redis-cli`-style REPL panel (see
//! docs/plans/redis.md). A reply is matched to the most recent history entry
//! still awaiting one with the same `argv`, on the assumption a human types
//! one command, waits, reads the reply, then types the next — good enough
//! without per-command ids; a burst of rapid-fire submissions could in
//! principle match a reply to the wrong entry.

use gpui::{Context, Entity, ScrollHandle, Window, div, prelude::*, px};
use red_core::kv::{CommandClass, RespValue, classify_command, tokenize_command};
use red_service::{Command, SessionId};

use flint::prelude::*;

use crate::app::{ActiveConn, AppState, TabWorkspace};

/// Cap on retained console entries. The panel re-renders every entry each frame
/// and keeps them all resident, so an unbounded log would grow render cost and
/// memory with the whole session's command count; the oldest are dropped past
/// this (like the other live panels' `MAX_LINES`/`MAX_MESSAGES` caps).
const MAX_CONSOLE_HISTORY: usize = 500;

/// Common Redis command names, for the console's first-token autocompletion.
/// Not exhaustive (Redis has hundreds), but covers the everyday keyspace,
/// type, server, and stream commands a browsing console reaches for. Kept
/// uppercase; matching is case-insensitive.
const REDIS_COMMANDS: &[&str] = &[
    "APPEND",
    "BITCOUNT",
    "BITPOS",
    "COPY",
    "DBSIZE",
    "DECR",
    "DECRBY",
    "DEL",
    "DUMP",
    "ECHO",
    "EXISTS",
    "EXPIRE",
    "EXPIREAT",
    "EXPIRETIME",
    "FLUSHALL",
    "FLUSHDB",
    "GEOADD",
    "GEODIST",
    "GEOPOS",
    "GEOSEARCH",
    "GET",
    "GETDEL",
    "GETEX",
    "GETRANGE",
    "GETSET",
    "HDEL",
    "HEXISTS",
    "HGET",
    "HGETALL",
    "HINCRBY",
    "HKEYS",
    "HLEN",
    "HMGET",
    "HRANDFIELD",
    "HSCAN",
    "HSET",
    "HSETNX",
    "HSTRLEN",
    "HVALS",
    "INCR",
    "INCRBY",
    "INCRBYFLOAT",
    "INFO",
    "KEYS",
    "LINDEX",
    "LINSERT",
    "LLEN",
    "LMOVE",
    "LPOP",
    "LPOS",
    "LPUSH",
    "LRANGE",
    "LREM",
    "LSET",
    "LTRIM",
    "MEMORY",
    "MGET",
    "MSET",
    "MSETNX",
    "OBJECT",
    "PERSIST",
    "PEXPIRE",
    "PEXPIREAT",
    "PFADD",
    "PFCOUNT",
    "PFMERGE",
    "PING",
    "PSETEX",
    "PTTL",
    "RANDOMKEY",
    "RENAME",
    "RENAMENX",
    "RESTORE",
    "RPOP",
    "RPUSH",
    "SADD",
    "SCAN",
    "SCARD",
    "SDIFF",
    "SET",
    "SETBIT",
    "SETEX",
    "SETNX",
    "SETRANGE",
    "SINTER",
    "SISMEMBER",
    "SMEMBERS",
    "SMOVE",
    "SORT",
    "SPOP",
    "SRANDMEMBER",
    "SREM",
    "SSCAN",
    "STRLEN",
    "SUNION",
    "TTL",
    "TYPE",
    "UNLINK",
    "XACK",
    "XADD",
    "XAUTOCLAIM",
    "XCLAIM",
    "XDEL",
    "XGROUP",
    "XINFO",
    "XLEN",
    "XPENDING",
    "XRANGE",
    "XREAD",
    "XREVRANGE",
    "XTRIM",
    "ZADD",
    "ZCARD",
    "ZCOUNT",
    "ZINCRBY",
    "ZRANGE",
    "ZRANGEBYSCORE",
    "ZRANK",
    "ZREM",
    "ZREVRANGE",
    "ZSCAN",
    "ZSCORE",
];

/// The command completions for a first-token `prefix` (case-insensitive prefix
/// match), capped for a tidy strip. Empty when `prefix` is empty or the line
/// already has more than one token (the user is past the command name).
fn command_completions(line: &str) -> Vec<&'static str> {
    let trimmed = line.trim_start();
    // Only complete the command name: no space typed yet, and something typed.
    if trimmed.is_empty() || trimmed.contains(char::is_whitespace) {
        return Vec::new();
    }
    let upper = trimmed.to_ascii_uppercase();
    // An exact, complete command is not a suggestion worth showing.
    if REDIS_COMMANDS.contains(&upper.as_str()) {
        return Vec::new();
    }
    REDIS_COMMANDS
        .iter()
        .filter(|c| c.starts_with(&upper))
        .take(8)
        .copied()
        .collect()
}

/// One submitted command line and its reply, once it arrives.
pub(crate) struct KvConsoleEntry {
    pub(crate) argv: Vec<String>,
    /// The request id this entry was sent with, matched exactly against the
    /// reply's `req` (no more best-effort `argv` matching).
    pub(crate) req: u64,
    /// `None` while in flight.
    pub(crate) result: Option<Result<RespValue, String>>,
}

/// The console's input surface: a single-line prompt, or a multi-line **Batch**
/// composer that runs many commands with per-line output and mid-run cancel.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConsoleMode {
    Line,
    Batch,
}

/// Progress of an in-flight `KvBatch` (the composer's "Run all"). Drives the
/// progress row and Stop button; `None` when no batch is running.
pub(crate) struct BatchRun {
    pub(crate) total: usize,
    pub(crate) done: usize,
    pub(crate) ok: usize,
    pub(crate) failed: usize,
}

pub(crate) struct KvConsole {
    pub(crate) epoch: red_service::Epoch,
    pub(crate) input: Entity<TextInput>,
    pub(crate) history: Vec<KvConsoleEntry>,
    /// Monotonic request-id source; each sent command takes the next value, and
    /// the reply carries it back so the exact entry is filled in.
    pub(crate) next_req: u64,
    /// A destructive command (`classify_command` says so) waits here for an
    /// explicit confirm before it's actually sent.
    pub(crate) pending_confirm: Option<Vec<String>>,
    /// Up/Down command recall: an index into `history` while browsing past
    /// commands, `None` when editing a fresh line. Reset when a command runs.
    pub(crate) recall: Option<usize>,
    pub(crate) scroll: ScrollHandle,
    /// Line vs Batch input surface (the segmented toggle above the input).
    pub(crate) mode: ConsoleMode,
    /// The multi-line Batch composer (the same Flint `CodeEditor` SQL uses —
    /// paste-of-many, gutter, selection). ⌘↵ runs the whole buffer.
    pub(crate) batch_input: Entity<CodeEditor>,
    /// `Some` while a batch runs: drives the progress row + Stop button.
    pub(crate) batch: Option<BatchRun>,
    /// A batch parked behind the destructive pre-scan confirm bar (its parsed
    /// commands), awaiting an explicit "Run" before it's sent.
    pub(crate) pending_batch: Option<Vec<Vec<String>>>,
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
        let batch_input = cx.new(|cx| {
            CodeEditor::new(cx)
                .soft_wrap(false)
                .placeholder(
                    "One command per line. # comments and blanks are skipped. ⌘↵ runs all.",
                )
                .a11y_label("Redis batch composer")
        });
        cx.subscribe(&batch_input, move |this, _, event: &CodeEditorEvent, cx| {
            if matches!(event, CodeEditorEvent::Run) {
                this.kv_console_run_batch(session, cx);
            }
        })
        .detach();
        Self {
            epoch: crate::result::next_kv_epoch(),
            input,
            history: Vec::new(),
            next_req: 0,
            pending_confirm: None,
            recall: None,
            scroll: ScrollHandle::new(),
            mode: ConsoleMode::Line,
            batch_input,
            batch: None,
            pending_batch: None,
        }
    }
}

/// Parse a Batch composer buffer into tokenized commands, reusing import's exact
/// rules: trim each line, drop blanks and `#` comments, tokenize the rest.
fn parse_batch(text: &str) -> Vec<Vec<String>> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(tokenize_command)
        .filter(|argv| !argv.is_empty())
        .collect()
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
        let req = console.next_req;
        console.next_req += 1;
        console.history.push(KvConsoleEntry {
            argv: argv.clone(),
            req,
            result: None,
        });
        if console.history.len() > MAX_CONSOLE_HISTORY {
            let drop = console.history.len() - MAX_CONSOLE_HISTORY;
            console.history.drain(0..drop);
        }
        self.service
            .send_to(session, Command::KvCommand { epoch, argv, req });
        cx.notify();
    }

    /// Switch the console between the single-line prompt and the Batch composer.
    pub(crate) fn kv_console_set_mode(
        &mut self,
        session: SessionId,
        mode: ConsoleMode,
        cx: &mut Context<Self>,
    ) {
        if let Some(console) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_console_mut())
        {
            console.mode = mode;
        }
        cx.notify();
    }

    /// "Run all" (⌘↵ in the composer): parse the buffer, and either park it
    /// behind the destructive pre-scan confirm bar or send it straight away.
    /// A pre-scan counts destructive lines once for the whole script instead of
    /// parking each command the way the interactive prompt does.
    pub(crate) fn kv_console_run_batch(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(console) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_console_mut())
        else {
            return;
        };
        if console.batch.is_some() {
            return; // already running
        }
        let commands = parse_batch(&console.batch_input.read(cx).content());
        if commands.is_empty() {
            return;
        }
        let destructive = commands
            .iter()
            .filter(|a| classify_command(a) == CommandClass::Destructive)
            .count();
        if destructive > 0 {
            console.pending_batch = Some(commands);
            cx.notify();
            return;
        }
        self.kv_console_send_batch(session, commands, cx);
    }

    /// Confirm a batch parked by the destructive pre-scan, and send it.
    pub(crate) fn kv_console_confirm_batch(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(commands) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_console_mut())
            .and_then(|c| c.pending_batch.take())
        else {
            return;
        };
        self.kv_console_send_batch(session, commands, cx);
    }

    /// Dismiss a batch parked by the destructive pre-scan without running it.
    pub(crate) fn kv_console_cancel_batch(&mut self, session: SessionId, cx: &mut Context<Self>) {
        if let Some(console) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_console_mut())
        {
            console.pending_batch = None;
        }
        cx.notify();
    }

    /// Pre-seed one log entry per command (so `KvBatchLine` results slot in by
    /// `req`), record the lines in the shared history store, then send the
    /// streaming `KvBatch`.
    fn kv_console_send_batch(
        &mut self,
        session: SessionId,
        commands: Vec<Vec<String>>,
        cx: &mut Context<Self>,
    ) {
        let conn_id = self
            .conn_mut(Some(session))
            .map(|a| a.conn_id.clone())
            .unwrap_or_default();
        if !conn_id.is_empty() {
            for argv in &commands {
                self.query_history.record(&conn_id, &argv.join(" "));
            }
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
        let req_base = console.next_req;
        for argv in &commands {
            let req = console.next_req;
            console.next_req += 1;
            console.history.push(KvConsoleEntry {
                argv: argv.clone(),
                req,
                result: None,
            });
        }
        if console.history.len() > MAX_CONSOLE_HISTORY {
            let drop = console.history.len() - MAX_CONSOLE_HISTORY;
            console.history.drain(0..drop);
        }
        console.batch = Some(BatchRun {
            total: commands.len(),
            done: 0,
            ok: 0,
            failed: 0,
        });
        self.service.send_to(
            session,
            Command::KvBatch {
                epoch,
                req_base,
                commands,
            },
        );
        cx.notify();
    }

    /// Stop a running batch (the Stop button): cancels it between commands
    /// server-side; a `KvBatchDone { aborted: true }` clears the running state.
    pub(crate) fn kv_console_stop_batch(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let epoch = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_ref())
            .and_then(|v| v.active_console())
            .filter(|c| c.batch.is_some())
            .map(|c| c.epoch);
        if let Some(epoch) = epoch {
            self.service
                .send_to(session, Command::KvBatchStop { epoch });
        }
        cx.notify();
    }

    /// Load a `.redis`/`.txt` file of commands into the Batch composer (the
    /// folded "Import keys" path — now with visible per-command output + cancel).
    pub(crate) fn kv_console_load_batch_file(
        &mut self,
        session: SessionId,
        cx: &mut Context<Self>,
    ) {
        let paths = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Load batch file".into()),
        });
        cx.spawn(
            async move |this: gpui::WeakEntity<Self>, cx: &mut gpui::AsyncApp| {
                let Ok(Ok(Some(paths))) = paths.await else {
                    return;
                };
                let Some(path) = paths.into_iter().next() else {
                    return;
                };
                let text = cx
                    .background_executor()
                    .spawn(async move { std::fs::read_to_string(&path).map_err(|e| e.to_string()) })
                    .await;
                this.update(cx, |this, cx| match text {
                    Ok(text) => {
                        if let Some(console) = this
                            .conn_mut(Some(session))
                            .and_then(|a| a.kv_view.as_mut())
                            .and_then(|v| v.active_console_mut())
                        {
                            console.mode = ConsoleMode::Batch;
                            let editor = console.batch_input.clone();
                            editor.update(cx, |ed, cx| ed.set_content(text, cx));
                        }
                        cx.notify();
                    }
                    Err(e) => {
                        this.notify(ToastVariant::Error, format!("Couldn't read file: {e}"), cx);
                    }
                })
                .ok();
            },
        )
        .detach();
    }

    /// Save the Batch composer buffer to a `.redis` file (round-trips with
    /// "Load batch file"). A no-op when the composer is empty.
    pub(crate) fn kv_console_save_batch_file(
        &mut self,
        session: SessionId,
        cx: &mut Context<Self>,
    ) {
        let Some(text) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_ref())
            .and_then(|v| v.active_console())
            .map(|c| c.batch_input.read(cx).content())
            .filter(|t| !t.trim().is_empty())
        else {
            return;
        };
        let dir = dirs::download_dir()
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let path = cx.prompt_for_new_path(&dir, Some("batch.redis"));
        cx.spawn(
            async move |this: gpui::WeakEntity<Self>, cx: &mut gpui::AsyncApp| {
                let Ok(Ok(Some(path))) = path.await else {
                    return;
                };
                let result = cx
                    .background_executor()
                    .spawn(async move { std::fs::write(&path, text).map_err(|e| e.to_string()) })
                    .await;
                this.update(cx, |this, cx| {
                    match result {
                        Ok(()) => this.notify(ToastVariant::Success, "Batch saved", cx),
                        Err(e) => {
                            this.notify(ToastVariant::Error, format!("Couldn't save: {e}"), cx)
                        }
                    };
                })
                .ok();
            },
        )
        .detach();
    }

    /// `Event::KvBatchLine`: fill in the pre-seeded entry whose `req` matches and
    /// advance the progress counters.
    pub(crate) fn on_kv_batch_line(
        &mut self,
        session: Option<SessionId>,
        epoch: red_service::Epoch,
        req: u64,
        result: RespValue,
        cx: &mut Context<Self>,
    ) {
        let Some(console) = self
            .conn_mut(session)
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.console_by_epoch_mut(epoch))
        else {
            return;
        };
        let is_err = matches!(result, RespValue::Error(_));
        if let Some(entry) = console.history.iter_mut().find(|e| e.req == req) {
            entry.result = Some(Ok(result));
        }
        if let Some(run) = console.batch.as_mut() {
            run.done += 1;
            if is_err {
                run.failed += 1;
            } else {
                run.ok += 1;
            }
        }
        cx.notify();
    }

    /// `Event::KvBatchDone`: clear the running state and toast a summary.
    pub(crate) fn on_kv_batch_done(
        &mut self,
        session: Option<SessionId>,
        epoch: red_service::Epoch,
        ok: usize,
        failed: usize,
        aborted: bool,
        cx: &mut Context<Self>,
    ) {
        if let Some(console) = self
            .conn_mut(session)
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.console_by_epoch_mut(epoch))
        {
            console.batch = None;
        }
        let (variant, msg) = match (aborted, failed) {
            (true, _) => (
                ToastVariant::Warning,
                format!("Batch stopped — {ok} ok, {failed} failed"),
            ),
            (false, 0) => (ToastVariant::Success, format!("Batch ran {ok} command(s)")),
            (false, _) => (
                ToastVariant::Warning,
                format!("Batch: {ok} ok, {failed} failed"),
            ),
        };
        self.notify(variant, msg, cx);
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

    /// Fill the console input with `cmd` and a trailing space (a completion
    /// chip click), ready for arguments.
    pub(crate) fn kv_console_complete(
        &mut self,
        session: SessionId,
        cmd: &str,
        cx: &mut Context<Self>,
    ) {
        let input = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_console_mut())
            .map(|c| c.input.clone());
        if let Some(input) = input {
            let seed = format!("{cmd} ");
            input.update(cx, |ti, cx| ti.set_content(&seed, cx));
        }
        cx.notify();
    }

    /// Tab in the console: accept the top command completion for the current
    /// first token, if any. A no-op when there's nothing to complete.
    pub(crate) fn kv_console_complete_top(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let line = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_console_mut())
            .map(|c| c.input.read(cx).content().to_string());
        let Some(top) = line.and_then(|l| command_completions(&l).first().copied()) else {
            return;
        };
        self.kv_console_complete(session, top, cx);
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

    /// `Event::KvCommandResult`: fill in the entry whose `req` matches — an exact
    /// request/response correlation, so rapid-fire submissions can't attribute a
    /// reply to the wrong command.
    pub(crate) fn on_kv_command_result(
        &mut self,
        session: Option<SessionId>,
        epoch: red_service::Epoch,
        req: u64,
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
        if let Some(entry) = console.history.iter_mut().find(|e| e.req == req) {
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
    ) -> impl IntoElement + use<> {
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

        let mode = console.mode;
        // Line / Batch toggle, plus (in Batch) file load/save. A running batch
        // pins the toggle so the composer can't be swapped away mid-run.
        let running = console.batch.is_some();
        let mode_view = view.clone();
        let mut header = div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .border_t_1()
            .border_color(theme.border)
            .child(
                Segmented::new("kv-console-mode")
                    .segment("Line")
                    .segment("Batch")
                    .selected(if mode == ConsoleMode::Batch { 1 } else { 0 })
                    .on_select(move |i, _w, cx| {
                        let m = if i == 1 {
                            ConsoleMode::Batch
                        } else {
                            ConsoleMode::Line
                        };
                        mode_view
                            .update(cx, |this, cx| this.kv_console_set_mode(session, m, cx))
                            .ok();
                    }),
            );
        if mode == ConsoleMode::Batch {
            let load_view = view.clone();
            let save_view = view.clone();
            header = header.child(div().flex_1()).child(
                div()
                    .flex()
                    .gap_1()
                    .child(
                        Button::new("kv-batch-load", "Load file…")
                            .size(ButtonSize::Sm)
                            .on_click(move |_, _, cx| {
                                load_view
                                    .update(cx, |this, cx| {
                                        this.kv_console_load_batch_file(session, cx)
                                    })
                                    .ok();
                            }),
                    )
                    .child(
                        Button::new("kv-batch-save", "Save…")
                            .size(ButtonSize::Sm)
                            .on_click(move |_, _, cx| {
                                save_view
                                    .update(cx, |this, cx| {
                                        this.kv_console_save_batch_file(session, cx)
                                    })
                                    .ok();
                            }),
                    ),
            );
        }

        // Batch destructive pre-scan confirm bar (counts destructive lines once
        // for the whole script rather than parking each command).
        let batch_confirm = console.pending_batch.as_ref().map(|commands| {
            let destructive = commands
                .iter()
                .filter(|a| classify_command(a) == CommandClass::Destructive)
                .count();
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
                        .child(format!(
                            "This batch contains {destructive} destructive command(s). Run all {} line(s)?",
                            commands.len()
                        )),
                )
                .child(
                    Button::new("kv-batch-confirm", "Run all")
                        .variant(ButtonVariant::Danger)
                        .size(ButtonSize::Sm)
                        .on_click(move |_, _, cx| {
                            confirm_view
                                .update(cx, |this, cx| this.kv_console_confirm_batch(session, cx))
                                .ok();
                        }),
                )
                .child(
                    Button::new("kv-batch-cancel", "Cancel")
                        .size(ButtonSize::Sm)
                        .on_click(move |_, _, cx| {
                            cancel_view
                                .update(cx, |this, cx| this.kv_console_cancel_batch(session, cx))
                                .ok();
                        }),
                )
        });

        // Batch composer footer (mode == Batch): the multi-line editor plus a
        // Run all / Stop row and a progress readout while running.
        let batch_footer = (mode == ConsoleMode::Batch).then(|| {
            let run_view = view.clone();
            let stop_view = view.clone();
            let progress = console.batch.as_ref().map(|b| {
                div()
                    .text_size(theme.scale(10.5))
                    .text_color(dim)
                    .child(format!("Running {} / {}", b.done, b.total))
            });
            let action = if running {
                Button::new("kv-batch-stop", "Stop")
                    .variant(ButtonVariant::Danger)
                    .size(ButtonSize::Sm)
                    .on_click(move |_, _, cx| {
                        stop_view
                            .update(cx, |this, cx| this.kv_console_stop_batch(session, cx))
                            .ok();
                    })
            } else {
                Button::new("kv-batch-run", "Run all")
                    .variant(ButtonVariant::Primary)
                    .size(ButtonSize::Sm)
                    .on_click(move |_, _, cx| {
                        run_view
                            .update(cx, |this, cx| this.kv_console_run_batch(session, cx))
                            .ok();
                    })
            };
            div()
                .flex_shrink_0()
                .flex()
                .flex_col()
                .border_t_1()
                .border_color(theme.border)
                .child(
                    div()
                        .h(px(160.))
                        .min_h(px(0.))
                        .child(console.batch_input.clone()),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .py_1p5()
                        .border_t_1()
                        .border_color(theme.border)
                        .children(progress)
                        .child(div().flex_1())
                        .child(action),
                )
        });

        // First-token command completions for the current input, as a strip of
        // clickable chips above the input (Tab accepts the first). Line mode only.
        let completions = if mode == ConsoleMode::Line {
            command_completions(&console.input.read(cx).content())
        } else {
            Vec::new()
        };
        let completion_strip = (!completions.is_empty()).then(|| {
            let mut strip = div()
                .flex_shrink_0()
                .flex()
                .flex_wrap()
                .items_center()
                .gap_1()
                .px_2()
                .py_1()
                .border_t_1()
                .border_color(theme.border);
            for cmd in completions {
                let chip_view = view.clone();
                strip = strip.child(
                    div()
                        .id(gpui::SharedString::from(format!("kv-complete-{cmd}")))
                        .px_1p5()
                        .rounded_sm()
                        .bg(theme.bg_elevated)
                        .border_1()
                        .border_color(theme.border)
                        .font_family(mono.clone())
                        .text_size(theme.scale(10.5))
                        .text_color(theme.text)
                        .hover(|s| s.border_color(theme.accent))
                        .cursor_pointer()
                        .child(cmd)
                        .on_click(move |_, _, cx| {
                            chip_view
                                .update(cx, |this, cx| this.kv_console_complete(session, cmd, cx))
                                .ok();
                        }),
                );
            }
            strip.child(
                div()
                    .text_size(theme.scale(10.))
                    .text_color(dim)
                    .child("Tab to complete"),
            )
        });

        // Line-mode single-line prompt (hidden in Batch mode).
        let line_footer = (mode == ConsoleMode::Line).then(|| {
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
                        // Up/Down recall past commands; Tab accepts the top
                        // completion. The single-line input uses neither.
                        .on_key_down(cx.listener(move |this, ev: &gpui::KeyDownEvent, _w, cx| {
                            match ev.keystroke.key.as_str() {
                                "up" => this.kv_console_recall(session, true, cx),
                                "down" => this.kv_console_recall(session, false, cx),
                                "tab" => this.kv_console_complete_top(session, cx),
                                _ => {}
                            }
                        }))
                        .child(console.input.clone()),
                )
        });

        div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .child(history)
            .children(confirm_bar)
            .child(header)
            .children(batch_confirm)
            .children(batch_footer)
            .children(completion_strip)
            .children(line_footer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completions_match_prefix_case_insensitively() {
        // Prefix match, uppercased.
        let c = command_completions("ge");
        assert!(c.contains(&"GET"));
        assert!(c.contains(&"GETRANGE"));
        assert!(c.iter().all(|c| c.starts_with("GE")));
        // A complete command isn't re-suggested.
        assert!(command_completions("GET").is_empty());
        // Past the command name (a space typed): no completions.
        assert!(command_completions("GET foo").is_empty());
        // Empty line: nothing.
        assert!(command_completions("   ").is_empty());
    }

    #[test]
    fn parse_batch_skips_blanks_and_comments_and_tokenizes() {
        let commands = parse_batch(
            "# a comment\n\
             SET foo bar\n\
             \n\
               # indented comment\n\
             GET \"quoted key\"\n\
             DEL x y z\n",
        );
        assert_eq!(
            commands,
            vec![
                vec!["SET".to_string(), "foo".into(), "bar".into()],
                vec!["GET".to_string(), "quoted key".into()],
                vec!["DEL".to_string(), "x".into(), "y".into(), "z".into()],
            ]
        );
    }

    #[test]
    fn destructive_pre_scan_counts_only_destructive_lines() {
        let commands = parse_batch("GET a\nDEL a\nSET b 1\nFLUSHDB\n");
        let destructive = commands
            .iter()
            .filter(|a| classify_command(a) == CommandClass::Destructive)
            .count();
        // DEL and FLUSHDB are destructive; GET and SET are not.
        assert_eq!(destructive, 2);
    }
}
