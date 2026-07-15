//! Redis "find biggest keys" sampling and the persisted keyspace-analysis walk,
//! split out of `kvbrowse/mod.rs` (guidelines D): both drive a dedicated scan
//! epoch that pages the keyspace to build a size/namespace rollup. A second
//! `impl AppState` block over the parent's state (`use super::*`).

use std::time::Duration;

use gpui::Context;
use red_core::kv::ScanCursor;
use red_service::{Command, SessionId};

use crate::app::AppState;

use super::*;

impl AppState {
    /// Kick off a "find biggest keys" sample (see `BigKeysState`'s doc
    /// comment): a fresh, dedicated scan epoch that keeps requesting pages
    /// until it's exhausted the keyspace or hit the sample's own bounds.
    pub(crate) fn kv_start_big_keys_sample(&mut self, session: SessionId, cx: &mut Context<Self>) {
        // Reachable from the browse actions menu now — dismiss it on the way in
        // (a no-op when launched from the palette).
        self.kv_close_actions_menu(session, cx);
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = active.kv_view.as_mut().and_then(|v| v.active_browse_mut()) else {
            return;
        };
        let epoch = crate::result::next_kv_epoch();
        browse.big_keys = Some(BigKeysState {
            epoch,
            cursor: ScanCursor::START,
            sampled: 0,
            running: true,
            started: std::time::Instant::now(),
            results: Vec::new(),
        });
        self.service.send_to(
            session,
            Command::KvFetchScan {
                epoch,
                pattern: None,
                type_filter: None,
                value_needle: None,
                cursor: ScanCursor::START,
                budget: scan_budget(),
            },
        );
        cx.notify();
    }

    pub(super) fn on_big_keys_page(
        &mut self,
        session: Option<SessionId>,
        epoch: red_service::Epoch,
        page: red_core::kv::KvScanPage,
        cx: &mut Context<Self>,
    ) {
        let Some(active) = self.conn_mut(session) else {
            return;
        };
        let Some(browse) = active.kv_view.as_mut().and_then(|v| v.active_browse_mut()) else {
            return;
        };
        let Some(bk) = &mut browse.big_keys else {
            return;
        };
        if bk.epoch != epoch {
            return;
        }
        bk.sampled += page.keys.len();
        bk.results.extend(page.keys);
        bk.cursor = page.next_cursor;
        let over_budget = bk.sampled >= BIG_KEYS_SAMPLE_CAP
            || bk.started.elapsed() >= Duration::from_millis(BIG_KEYS_SAMPLE_MS);
        if page.exhausted || over_budget {
            bk.running = false;
            bk.results
                .sort_by_key(|k| std::cmp::Reverse(k.approx_bytes));
            bk.results.truncate(BIG_KEYS_TOP_N);
            cx.notify();
            return;
        }
        let cursor = bk.cursor;
        let Some(session) = session else { return };
        self.service.send_to(
            session,
            Command::KvFetchScan {
                epoch,
                pattern: None,
                type_filter: None,
                value_needle: None,
                cursor,
                budget: scan_budget(),
            },
        );
        cx.notify();
    }

    /// Dismiss the big-keys sample (running or finished) and return to the
    /// live browse.
    pub(crate) fn kv_close_big_keys(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(active) = self.conn_mut(Some(session)) else {
            return;
        };
        let Some(browse) = active.kv_view.as_mut().and_then(|v| v.active_browse_mut()) else {
            return;
        };
        let Some(bk) = browse.big_keys.take() else {
            return;
        };
        self.service
            .send_to(session, Command::CloseResult { epoch: bk.epoch });
        cx.notify();
    }

    /// Load the persisted analysis report for this connection into the panel,
    /// the first time it's opened this session (see `redis_analysis.rs`). A
    /// no-op if a run has already produced a fresher report.
    pub(crate) fn kv_load_saved_analysis(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let conn_id = self
            .conn_mut(Some(session))
            .map(|a| a.conn_id.clone())
            .unwrap_or_default();
        let saved = self.redis_analysis.get(&conn_id).cloned();
        let Some(analysis) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_analysis_mut())
        else {
            return;
        };
        if analysis.loaded {
            return;
        }
        analysis.loaded = true;
        if analysis.report.is_none() {
            analysis.report = saved;
        }
        cx.notify();
    }

    /// Start a fresh keyspace-analysis run: a dedicated scan epoch that chains
    /// pages (like the biggest-keys sampler) until the keyspace is exhausted or
    /// the analysis budget is hit, then rolls the sample up and persists it.
    pub(crate) fn kv_run_analysis(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(analysis) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_analysis_mut())
        else {
            return;
        };
        if analysis.running {
            return;
        }
        let epoch = crate::result::next_kv_epoch();
        analysis.epoch = epoch;
        analysis.cursor = ScanCursor::START;
        analysis.running = true;
        analysis.started = std::time::Instant::now();
        analysis.collected.clear();
        analysis.loaded = true;
        self.service.send_to(
            session,
            Command::KvFetchScan {
                epoch,
                pattern: None,
                type_filter: None,
                value_needle: None,
                cursor: ScanCursor::START,
                budget: scan_budget(),
            },
        );
        cx.notify();
    }

    pub(super) fn on_analysis_page(
        &mut self,
        session: Option<SessionId>,
        epoch: red_service::Epoch,
        page: red_core::kv::KvScanPage,
        cx: &mut Context<Self>,
    ) {
        // First mutate the run state under the browse borrow; decide whether it
        // finished, and if so compute the report (needs `db_size` too).
        let (finished, report, conn_id) = {
            let Some(active) = self.conn_mut(session) else {
                return;
            };
            let conn_id = active.conn_id.clone();
            let Some(view) = &mut active.kv_view else {
                return;
            };
            // `DBSIZE` is connection-level; read it before borrowing the tab.
            let total_keys = view.db_size.unwrap_or(0);
            let Some(analysis) = view.analysis_by_epoch_mut(epoch) else {
                return;
            };
            if !analysis.running {
                return;
            }
            analysis.collected.extend(page.keys);
            analysis.cursor = page.next_cursor;
            let over_budget = analysis.collected.len() >= ANALYSIS_SAMPLE_CAP
                || analysis.started.elapsed() >= Duration::from_millis(ANALYSIS_SAMPLE_MS);
            if page.exhausted || over_budget {
                analysis.running = false;
                let truncated = !page.exhausted;
                let report = red_core::kv::analyze_keyspace(
                    &analysis.collected,
                    total_keys,
                    truncated,
                    crate::conversations::now_unix() as i64,
                );
                analysis.report = Some(report.clone());
                // Drop the raw sample now that it's rolled up.
                analysis.collected = Vec::new();
                (true, Some(report), conn_id)
            } else {
                (false, None, conn_id)
            }
        };

        if finished {
            if let Some(report) = report {
                // Persist the fresh report so it survives a restart.
                self.redis_analysis.set(&conn_id, report);
            }
            cx.notify();
            return;
        }

        // Not finished: chain the next page (outside the browse borrow).
        let Some(session) = session else { return };
        let cursor = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.analysis_by_epoch_mut(epoch))
            .map(|a| a.cursor);
        if let Some(cursor) = cursor {
            self.service.send_to(
                session,
                Command::KvFetchScan {
                    epoch,
                    pattern: None,
                    type_filter: None,
                    value_needle: None,
                    cursor,
                    budget: scan_budget(),
                },
            );
        }
        cx.notify();
    }

    /// Stop an in-progress analysis run (leaves any already-shown report).
    pub(crate) fn kv_cancel_analysis(&mut self, session: SessionId, cx: &mut Context<Self>) {
        let Some(analysis) = self
            .conn_mut(Some(session))
            .and_then(|a| a.kv_view.as_mut())
            .and_then(|v| v.active_analysis_mut())
        else {
            return;
        };
        if !analysis.running {
            return;
        }
        analysis.running = false;
        analysis.collected = Vec::new();
        let epoch = analysis.epoch;
        self.service
            .send_to(session, Command::CloseResult { epoch });
        cx.notify();
    }
}
