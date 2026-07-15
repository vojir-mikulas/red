//! Connection + session lifecycle: minting and routing backend sessions,
//! parking warm connections, the connect/retry/backoff state machine, the
//! `Connected`/`Disconnected` event handlers, and the connection-manager
//! delete actions. Split out of `mod.rs` to keep the root state machine lean.

use super::*;
use red_core::SshAuth;

impl AppState {
    // --- sessions (keep-alive workspaces) ---

    /// Mint a fresh `SessionId` for a new connect.
    pub(crate) fn mint_session(&mut self) -> SessionId {
        self.next_session_id += 1;
        SessionId::new(self.next_session_id)
    }

    /// The live `ActiveConn` for `session`: the foreground one (in `phase`) or a
    /// parked warm one. Used to route a backend event to its workspace even when
    /// that workspace is backgrounded (its query is still populating).
    pub(crate) fn conn_mut(&mut self, session: Option<SessionId>) -> Option<&mut ActiveConn> {
        let id = session?;
        if self.foreground_session == Some(id)
            && let Phase::Connected(active) = &mut self.phase
        {
            return Some(active);
        }
        self.parked.get_mut(&id).map(|b| b.as_mut())
    }

    /// The most-recently-foregrounded warm parked session, if any: the
    /// "previous connection" the ⌘⇧P toggle returns to, and the fall-back when the
    /// foreground session disconnects. (`parked` is a `HashMap`, so this picks by
    /// `last_active_seq` rather than iteration order.)
    pub(crate) fn parked_mru(&self) -> Option<SessionId> {
        self.parked
            .iter()
            .max_by_key(|(_, a)| a.last_active_seq)
            .map(|(id, _)| *id)
    }

    /// Fire a command at the foreground session (the on-screen connection). A
    /// no-op when nothing is foregrounded.
    pub(crate) fn send_active(&self, command: Command) {
        if let Some(id) = self.foreground_session {
            self.service.send_to(id, command);
        }
    }

    /// Move the foreground live connection (if any) into the warm-session map so
    /// switching back to it is instant. Leaves `phase` `Disconnected`; the caller
    /// installs the next phase. A connecting/disconnected foreground parks nothing.
    pub(crate) fn park_foreground(&mut self) -> Option<SessionId> {
        if matches!(self.phase, Phase::Connected(_))
            && let Phase::Connected(mut active) =
                std::mem::replace(&mut self.phase, Phase::Disconnected)
        {
            let id = active.session;
            // Stamp the just-foregrounded conn as the most-recently-used, then
            // make room: if parking would exceed the cap, evict the LRU parked
            // session first (never this one; it has the freshest stamp).
            self.next_active_seq += 1;
            active.last_active_seq = self.next_active_seq;
            self.evict_lru_parked();
            self.parked.insert(id, active);
            return Some(id);
        }
        None
    }

    /// Drop parked warm sessions until the map has room for one more, evicting the
    /// least-recently-foregrounded each time. The evicted workspace's heavy
    /// `ActiveConn` is freed immediately; `CloseSession` tells the backend to tear
    /// down its driver (its later `Disconnected` is a no-op, already gone here).
    fn evict_lru_parked(&mut self) {
        while self.parked.len() >= MAX_PARKED_SESSIONS {
            let Some(lru) = self
                .parked
                .iter()
                .min_by_key(|(_, a)| a.last_active_seq)
                .map(|(id, _)| *id)
            else {
                break;
            };
            self.parked.remove(&lru);
            self.service.send_to(lru, Command::CloseSession);
        }
    }

    /// Bring a parked warm session to the foreground, the instant-switch payoff:
    /// no reconnect, the grid/tabs/scroll exactly as left. Tells the backend it's
    /// now active (eviction-exempt).
    pub(crate) fn foreground_parked(&mut self, id: SessionId, cx: &mut Context<Self>) -> bool {
        let Some(active) = self.parked.remove(&id) else {
            return false;
        };
        self.foreground_session = Some(id);
        self.service
            .send_global(Command::SetActiveSession(Some(id)));
        self.phase = Phase::Connected(active);
        cx.notify();
        true
    }

    /// A `Connected` event: promote the connecting splash to a live workspace, if
    /// it's still the one the user wants. An orphan (they switched away mid-dial)
    /// is closed.
    pub(crate) fn on_connected(
        &mut self,
        session: Option<SessionId>,
        version: String,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = session else { return };
        let promote = matches!(&self.phase, Phase::Connecting(c) if c.session == id);
        if !promote {
            // We've moved on (switched away / cancelled); drop the stray session.
            self.service.send_to(id, Command::CloseSession);
            return;
        }
        if let Phase::Connecting(conn) = std::mem::replace(&mut self.phase, Phase::Disconnected) {
            // Invalidate any pending backoff timer from a prior attempt.
            self.connect_gen += 1;
            let is_redis = conn.config.kind == red_core::DbKind::Redis;
            let conn_id = conn.conn_id.clone();
            self.phase = Phase::Connected(Box::new(ActiveConn::new(
                id,
                conn.conn_id,
                conn.config,
                version,
                cx,
            )));
            self.foreground_session = Some(id);
            if is_redis {
                // Restore the persisted recently-viewed keys for this
                // connection before the first render reads them.
                self.kv_seed_recent_keys(id, &conn_id);
                // Redis has no schema/FK concept; kick off the keyspace
                // browser's first scan + header stat instead (R1, see
                // docs/plans/redis.md).
                self.kv_start_browse(id, cx);
            } else {
                // Kick off the schema-tree skeleton load for the sidebar, and
                // background-prefetch the FK graph (Track B7) so grid FK columns can be
                // marked before any click. Both run off the connect path.
                self.service.send_to(id, Command::LoadObjects);
                self.service.send_to(id, Command::LoadForeignKeys);
            }
            self.rebuild_switcher(cx);
        }
    }

    /// A `Disconnected` event: the session went away (manual disconnect, or
    /// backend idle eviction of a parked one). Drop it from wherever it lives; if
    /// it was foreground, fall back to a warm session or the welcome screen.
    pub(crate) fn on_disconnected(&mut self, session: Option<SessionId>, cx: &mut Context<Self>) {
        let Some(id) = session else { return };
        self.parked.remove(&id);
        if self.foreground_session == Some(id) {
            self.foreground_session = None;
            // Prefer an already-warm connection over the welcome screen: the most
            // recently foregrounded one (`parked` is a HashMap, so an arbitrary
            // pick would drop the user into a random session).
            if let Some(other) = self.parked_mru() {
                self.foreground_parked(other, cx);
            } else {
                self.service.send_global(Command::SetActiveSession(None));
                self.phase = Phase::Disconnected;
                self.connect_sel = 0;
                // Reclaim root focus so the welcome screen's cards navigate.
                self.refocus_root = true;
            }
        }
        self.rebuild_switcher(cx);
    }

    // --- connection-manager actions ---

    /// Arm the delete-confirmation modal for connection `index`. Deletion is
    /// destructive (drops the keychain credential too), so we never remove a
    /// connection on a single click; the modal's `confirm_delete` does the work.
    pub(crate) fn request_delete_connection(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.connections.len() {
            self.confirm_delete_conn = Some(index);
            // Focus the modal so its Enter/Esc handling is heard (and so focus
            // doesn't fall to the close ✕), matching the other confirmations.
            self.focus_modal = true;
            cx.notify();
        }
    }

    /// Confirmation accepted: remove the connection that was awaiting it.
    pub(crate) fn confirm_delete_connection(&mut self, cx: &mut Context<Self>) {
        if let Some(index) = self.confirm_delete_conn.take() {
            self.delete_connection(index, cx);
        }
        self.refocus_root = true;
        cx.notify();
    }

    pub(crate) fn cancel_delete_connection(&mut self, cx: &mut Context<Self>) {
        self.confirm_delete_conn = None;
        self.refocus_root = true;
        cx.notify();
    }

    /// Pin or unpin saved connection `index` (Phase 3). Pinned connections float
    /// to the top of the welcome list and the switcher and claim the low ⌘-digit
    /// slots, independent of recency. Persists and refreshes the switcher.
    pub(crate) fn toggle_pin(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(stored) = self.connections.get_mut(index) {
            stored.pinned = !stored.pinned;
            self.persist(cx);
            self.rebuild_switcher(cx);
            cx.notify();
        }
    }

    /// Duplicate saved connection `index`: append a clone under a "… copy" name
    /// with a fresh id, copying its keychain secrets (DB password, SSH secrets)
    /// across to the new id so the copy is a full working duplicate. The copy
    /// starts unpinned and never-accessed, like a fresh save.
    pub(crate) fn duplicate_connection(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(stored) = self.connections.get(index) else {
            return;
        };
        let source_id = stored.id.clone();
        let mut config = stored.config.clone();
        config.name = format!("{} copy", config.name);
        let is_file = config.kind.is_file();

        // Secrets live in the keychain keyed by the *source* id (the config carries
        // them empty), so hydrate them here to re-store under the new id.
        let mut password = config.password.clone();
        if password.is_empty()
            && !is_file
            && let Ok(Some(pw)) = crate::secrets::get_password(&source_id)
        {
            password = pw;
        }
        let ssh_secrets = config.ssh.as_ref().map(|ssh| {
            let mut pw = ssh.password.clone();
            let mut passphrase = ssh.passphrase.clone();
            match &ssh.auth {
                SshAuth::Password if pw.is_empty() => {
                    if let Ok(Some(s)) = crate::secrets::get_ssh_password(&source_id) {
                        pw = s;
                    }
                }
                SshAuth::Key { .. } if passphrase.is_empty() => {
                    if let Ok(Some(s)) = crate::secrets::get_ssh_passphrase(&source_id) {
                        passphrase = s;
                    }
                }
                _ => {}
            }
            (pw, passphrase)
        });

        self.connections.push(StoredConnection {
            id: crate::config::new_id(),
            config,
            last_accessed: None,
            pinned: false,
        });
        let new_index = self.connections.len() - 1;
        self.store_credential(new_index, &password, is_file, cx);
        self.store_ssh_credentials(new_index, ssh_secrets, cx);
        self.persist(cx);
        self.rebuild_switcher(cx);
        cx.notify();
    }

    pub(crate) fn delete_connection(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.connections.len() {
            let removed = self.connections.remove(index);
            // Keep the selection highlight in range after the row vanishes.
            self.connect_sel = self
                .connect_sel
                .min(self.connections.len().saturating_sub(1));
            // Drop the connection's keychain credentials too (DB password and any
            // SSH secrets), so deleting a connection doesn't orphan them.
            if let Err(e) = crate::secrets::delete_all(&removed.id) {
                tracing::warn!("failed to remove keychain credentials: {e}");
            }
            self.persist(cx);
            cx.notify();
        }
    }

    pub(crate) fn connect(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(stored) = self.connections.get_mut(index) else {
            return;
        };
        stored.last_accessed = Some(config::now());
        let id = stored.id.clone();
        let mut config = stored.config.clone();
        self.persist(cx);
        // Materialize the password from the keychain unless we already hold it in
        // memory (a keychain write that failed earlier this session keeps it there).
        if config.password.is_empty() && !config.kind.is_file() {
            match crate::secrets::get_password(&id) {
                Ok(Some(pw)) => config.password = pw,
                Ok(None) => {}
                Err(e) => tracing::warn!("failed to read credential from keychain: {e}"),
            }
        }
        // SSH secrets live in the keychain too (the saved config carries them empty),
        // so hydrate the tunnel's secret for the active auth mode before dialing;
        // otherwise a saved connection authenticates with a blank password/passphrase
        // and fails, even though the Test probe (which reads the form) succeeded.
        if let Some(ssh) = config.ssh.as_mut() {
            match &ssh.auth {
                SshAuth::Password if ssh.password.is_empty() => {
                    match crate::secrets::get_ssh_password(&id) {
                        Ok(Some(pw)) => ssh.password = pw,
                        Ok(None) => {}
                        Err(e) => tracing::warn!("failed to read SSH password from keychain: {e}"),
                    }
                }
                SshAuth::Key { .. } if ssh.passphrase.is_empty() => {
                    match crate::secrets::get_ssh_passphrase(&id) {
                        Ok(Some(pp)) => ssh.passphrase = pp,
                        Ok(None) => {}
                        Err(e) => {
                            tracing::warn!("failed to read SSH passphrase from keychain: {e}")
                        }
                    }
                }
                _ => {}
            }
        }
        // The proxy auth password lives in the keychain too; hydrate it before
        // dialing for the same reason as the SSH secret above.
        if let Some(proxy) = config.proxy.as_mut()
            && proxy.password.is_empty()
        {
            match crate::secrets::get_proxy_password(&id) {
                Ok(Some(pw)) => proxy.password = pw,
                Ok(None) => {}
                Err(e) => tracing::warn!("failed to read proxy password from keychain: {e}"),
            }
        }
        self.start_connect(id, config, cx);
    }

    /// Open a fresh connect session for saved connection `conn_id`: park whatever
    /// was foreground (kept warm), mint a session id, bump the generation
    /// (abandoning any pending retry), show the splash, and fire the first attempt.
    pub(crate) fn start_connect(
        &mut self,
        conn_id: String,
        config: ConnectionConfig,
        cx: &mut Context<Self>,
    ) {
        let previous = self.park_foreground();
        let session = self.mint_session();
        self.foreground_session = Some(session);
        self.connect_gen += 1;
        self.service
            .send_to(session, Command::Connect(config.clone()));
        self.service
            .send_global(Command::SetActiveSession(Some(session)));
        self.phase = Phase::Connecting(Box::new(Connecting {
            session,
            conn_id,
            previous,
            config,
            attempt: 1,
            status: ConnectStatus::InProgress,
        }));
        cx.notify();
    }

    /// Exponential backoff between connect retries: 1s, 2s, 4s, 8s, 16s, then
    /// capped at 30s. `attempt` is the number of the attempt that just failed.
    pub(crate) fn backoff_delay(attempt: u32) -> Duration {
        let secs = 1u64 << attempt.saturating_sub(1).min(5);
        Duration::from_secs(secs.min(30))
    }

    /// A connect attempt failed. A `fatal` failure (bad credentials, missing
    /// database) is terminal: show the error and an Edit affordance, no retry. A
    /// transient one records the error and schedules a backoff retry. No-op if
    /// we've left the connecting phase meanwhile.
    pub(crate) fn on_connect_failed(
        &mut self,
        message: String,
        fatal: bool,
        cx: &mut Context<Self>,
    ) {
        let delay = match &mut self.phase {
            Phase::Connecting(conn) if fatal => {
                // Bump the generation so any backoff timer from a prior transient
                // attempt can't fire and override this terminal state.
                conn.status = ConnectStatus::Failed {
                    error: message.into(),
                };
                self.connect_gen += 1;
                cx.notify();
                return;
            }
            Phase::Connecting(conn) => {
                let delay = Self::backoff_delay(conn.attempt);
                conn.status = ConnectStatus::Backoff {
                    error: message.into(),
                    delay,
                };
                delay
            }
            _ => return,
        };
        self.schedule_retry(delay, cx);
    }

    /// The in-flight connect hit an untrusted SSH jump host. Show the fingerprint
    /// and offer to trust it: terminal until the user acts, so (like a fatal
    /// failure) bump the generation to cancel any pending backoff timer.
    pub(crate) fn on_ssh_host_unknown(
        &mut self,
        host: String,
        port: u16,
        fingerprint: String,
        key: String,
        cx: &mut Context<Self>,
    ) {
        if let Phase::Connecting(conn) = &mut self.phase {
            conn.status = ConnectStatus::NeedsHostTrust {
                host,
                port,
                fingerprint: fingerprint.into(),
                key,
            };
            self.connect_gen += 1;
            cx.notify();
        }
    }

    /// "Trust & connect": append the jump host's key to `~/.ssh/known_hosts` (via
    /// the service) and retry the same connection; the retry verifies against the
    /// freshly written entry and proceeds.
    pub(crate) fn trust_host_and_retry(&mut self, cx: &mut Context<Self>) {
        let trust = match &self.phase {
            Phase::Connecting(conn) => match &conn.status {
                ConnectStatus::NeedsHostTrust {
                    host, port, key, ..
                } => Some((host.clone(), *port, key.clone())),
                _ => None,
            },
            _ => None,
        };
        let Some((host, port, key)) = trust else {
            return;
        };
        self.service
            .send_global(Command::TrustSshHost { host, port, key });
        // Re-dial on the same session; the command channel is ordered, so the
        // known_hosts write lands before this Connect is processed.
        self.begin_attempt(cx);
    }

    /// "Edit connection" on a fatal connect splash: leave the splash (restoring
    /// whatever was foreground before, like Cancel) and open the edit form for the
    /// connection that failed, so the user can fix the credentials and reconnect.
    pub(crate) fn edit_failed_connection(&mut self, cx: &mut Context<Self>) {
        let conn_id = match &self.phase {
            Phase::Connecting(conn) => conn.conn_id.clone(),
            _ => return,
        };
        let index = self.connections.iter().position(|c| c.id == conn_id);
        self.cancel_connect(cx);
        if let Some(index) = index {
            self.open_edit_form(index, cx);
        }
    }

    /// Arm a one-shot timer that retries the connection after `delay`, unless a
    /// newer generation (cancel, manual retry, or a fresh connect) supersedes it.
    pub(crate) fn schedule_retry(&mut self, delay: Duration, cx: &mut Context<Self>) {
        let generation = self.connect_gen;
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            cx.background_executor().timer(delay).await;
            this.update(cx, |this, cx| this.retry_connect(generation, cx))
                .ok();
        })
        .detach();
    }

    /// A backoff timer fired: start the next attempt if its generation is still
    /// current (i.e. not cancelled or already retried via "Retry now").
    pub(crate) fn retry_connect(&mut self, generation: u64, cx: &mut Context<Self>) {
        if generation == self.connect_gen {
            self.begin_attempt(cx);
        }
    }

    /// "Retry now" on the splash: skip the remaining backoff wait.
    pub(crate) fn retry_now(&mut self, cx: &mut Context<Self>) {
        if matches!(self.phase, Phase::Connecting(_)) {
            self.begin_attempt(cx);
        }
    }

    /// Fire the next attempt for the in-flight connection: bump the generation
    /// (abandoning any pending backoff timer), advance the counter, and re-send
    /// the Connect command.
    pub(crate) fn begin_attempt(&mut self, cx: &mut Context<Self>) {
        let (config, session) = match &mut self.phase {
            Phase::Connecting(conn) => {
                conn.attempt += 1;
                conn.status = ConnectStatus::InProgress;
                (conn.config.clone(), conn.session)
            }
            _ => return,
        };
        self.connect_gen += 1;
        self.service.send_to(session, Command::Connect(config));
        cx.notify();
    }

    /// Abandon an in-progress connection (the splash "Cancel" button): bump the
    /// generation so any pending retry is dropped, tell the backend to discard the
    /// session it may still be opening, and restore the connection that was
    /// foreground before this connect (or the welcome screen if there was none).
    pub(crate) fn cancel_connect(&mut self, cx: &mut Context<Self>) {
        self.connect_gen += 1;
        let previous = match &self.phase {
            Phase::Connecting(conn) => {
                self.service.send_to(conn.session, Command::CloseSession);
                conn.previous
            }
            _ => None,
        };
        self.foreground_session = None;
        match previous {
            Some(id) if self.foreground_parked(id, cx) => {}
            _ => {
                self.service.send_global(Command::SetActiveSession(None));
                self.phase = Phase::Disconnected;
            }
        }
        self.rebuild_switcher(cx);
        cx.notify();
    }

    /// Leave the connected view for the manager (welcome) screen: drop the
    /// foreground connection *and* every warm parked one; "Manage connections…"
    /// means a clean slate, not a pile of orphaned warm sessions the welcome
    /// screen can't reach.
    pub(crate) fn disconnect(&mut self, cx: &mut Context<Self>) {
        for id in self.parked.keys().copied().collect::<Vec<_>>() {
            self.service.send_to(id, Command::CloseSession);
        }
        self.parked.clear();
        if let Some(id) = self.foreground_session {
            self.service.send_to(id, Command::Disconnect);
        }
        self.foreground_session = None;
        self.service.send_global(Command::SetActiveSession(None));
        self.phase = Phase::Disconnected;
        self.connect_sel = 0;
        self.refocus_root = true;
        self.rebuild_switcher(cx);
        cx.notify();
    }
}
