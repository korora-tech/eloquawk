use super::*;

pub(super) struct SignIn {
    pub(super) seq: u64,
    pub(super) cancel: Arc<AtomicBool>,
}

#[derive(Clone, Copy)]
pub(super) enum Revoke {
    WithGoogle,
    LocalOnly,
}

pub(super) struct Worker {
    pub(super) events: async_channel::Sender<SyncEvent>,
    pub(super) self_tx: mpsc::Sender<Command>,
    pub(super) agent: ureq::Agent,
    pub(super) endpoints: Endpoints,
    pub(super) secrets: Secrets,
    /// The compiled-in client, used when none is saved.
    pub(super) builtin: Option<ClientCredentials>,
    /// The client in use: the saved one, else the built-in one.
    pub(super) client: Option<ClientCredentials>,
    pub(super) auth: AuthState,
    pub(super) accounts: BTreeMap<AccountId, AccountState>,
    pub(super) sign_in: Option<SignIn>,
    pub(super) sign_in_seq: u64,
    /// Sticky messages for the banner, until dismissed or resolved. Keys: an account's email,
    /// "client", "tokens" or "network".
    pub(super) problems: BTreeMap<String, String>,
    /// The last keyring read failed (locked, no Secret Service); retried on Refresh.
    pub(super) keyring_failed: bool,
    pub(super) tuning: Tuning,
    pub(super) queue: Queue,
    /// The view the UI shows.
    pub(super) want: Option<ViewKey>,
    pub(super) last_poll: Instant,
    pub(super) next_poll: Instant,
    pub(super) busy: bool,
    /// Where saved attachments go; `None` resolves the user's download folder on each save.
    /// Tests point it at a temporary folder.
    pub(super) downloads: Option<PathBuf>,
    /// Where the summary cache lives; `None` (tests, unless set) keeps no cache.
    pub(super) cache_dir: Option<PathBuf>,
    /// Opened once the keyring yields its key.
    pub(super) cache: Option<MailCache>,
    /// The key could not be had; not asked for again this run.
    pub(super) cache_failed: bool,
}

impl Worker {
    pub(super) fn new(
        secrets: Secrets,
        events: async_channel::Sender<SyncEvent>,
        self_tx: mpsc::Sender<Command>,
        endpoints: Endpoints,
        builtin: Option<ClientCredentials>,
        tuning: Tuning,
    ) -> Self {
        let now = Instant::now();
        Self {
            events,
            self_tx,
            agent: api::agent(),
            endpoints,
            secrets,
            builtin,
            client: None,
            auth: AuthState::Starting,
            accounts: BTreeMap::new(),
            sign_in: None,
            sign_in_seq: 0,
            problems: BTreeMap::new(),
            keyring_failed: false,
            tuning,
            queue: Queue::default(),
            want: None,
            last_poll: now,
            next_poll: now + tuning.refresh,
            busy: false,
            downloads: None,
            cache_dir: None,
            cache: None,
            cache_failed: false,
        }
    }

    pub(super) fn emit(&self, ev: SyncEvent) {
        let _ = self.events.send_blocking(ev);
    }

    pub(super) fn set_auth(&mut self, auth: AuthState) {
        if self.auth != auth {
            self.auth = auth;
            self.emit(SyncEvent::Auth(auth));
        }
    }

    /// Sets the banner to the sticky problems.
    pub(super) fn report(&self) {
        let all: Vec<&str> = self.problems.values().map(String::as_str).collect();
        self.emit(SyncEvent::Problem(
            (!all.is_empty()).then(|| all.join(" · ")),
        ));
    }

    pub(super) fn emit_accounts(&self) {
        self.emit(SyncEvent::Accounts(self.accounts.keys().cloned().collect()));
    }

    pub(super) fn run(mut self, rx: mpsc::Receiver<Command>) {
        self.load_from_keyring();
        'outer: loop {
            let first = match rx.recv_timeout(self.wait(Instant::now())) {
                Ok(cmd) => Some(cmd),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };
            let rest = std::iter::from_fn(|| rx.try_recv().ok());
            for cmd in first.into_iter().chain(rest) {
                if matches!(cmd, Command::Shutdown) {
                    break 'outer;
                }
                self.handle(cmd);
            }
            self.tick(Instant::now());
        }
        self.persist();
        self.cancel_sign_in();
    }

    /// How long the loop may sleep: not at all while jobs wait, otherwise until the next poll
    /// or the earliest backoff with parked jobs, capped at [`MAX_WAIT`].
    pub(super) fn wait(&self, now: Instant) -> Duration {
        if !self.queue.is_empty() {
            return Duration::ZERO;
        }
        let until = self
            .accounts
            .values()
            .filter(|a| !a.deferred.is_empty())
            .filter_map(|a| a.backoff.retry_at)
            .fold(self.next_poll, Instant::min);
        until.saturating_duration_since(now).min(MAX_WAIT)
    }

    /// One turn of the loop after the commands: re-queue parked jobs whose wait is over, run
    /// one job, poll when due, and clear the busy dot once both lanes drain.
    pub(super) fn tick(&mut self, now: Instant) {
        self.requeue_deferred(now);
        self.step(now);
        if Instant::now() >= self.next_poll {
            self.poll(Instant::now());
        }
        if self.busy && self.queue.is_empty() {
            self.busy = false;
            self.emit(SyncEvent::Busy(false));
        }
        if self.queue.is_empty() {
            self.persist();
        }
    }

    pub(super) fn requeue_deferred(&mut self, now: Instant) {
        for account in self.accounts.values_mut() {
            if !account.backoff.waiting(now) {
                for job in account.deferred.drain(..) {
                    self.queue.push(job);
                }
            }
        }
    }

    /// The poll (SPEC 7.2 step 5): every account not in backoff gets an initial sync until
    /// one has completed, a history round after that, and a fetch of any skeleton rows left
    /// by a failed chunk.
    pub(super) fn poll(&mut self, now: Instant) {
        self.last_poll = now;
        self.next_poll = now + self.tuning.refresh;
        if self.auth != AuthState::Ready {
            return;
        }
        let mut jobs = Vec::new();
        for (email, account) in &self.accounts {
            if account.backoff.waiting(now) {
                continue;
            }
            if account.history_id.is_none() || !account.synced_once {
                jobs.push(Job::InitialSync(email.clone()));
            } else {
                jobs.push(Job::History(email.clone()));
            }
            // Only the skeleton rows no queued or parked chunk will fetch anyway.
            let mut pending = self.queue.queued_meta(email);
            pending.extend(meta_ids(account.deferred.iter(), email));
            let missing: Vec<String> = account
                .missing
                .keys()
                .filter(|id| !pending.contains(*id))
                .cloned()
                .collect();
            jobs.extend(chunked(&missing, |ids| Job::FetchMeta(email.clone(), ids)));
        }
        for job in jobs {
            self.queue.push(job);
        }
    }

    /// Pops one job and runs it. A background job of an account in backoff is parked
    /// instead; interactive jobs always run.
    pub(super) fn step(&mut self, now: Instant) {
        let Some(job) = self.queue.pop() else {
            return;
        };
        if !job.interactive() {
            match self.accounts.get_mut(job.account()) {
                None => return,
                Some(account) if account.backoff.waiting(now) => {
                    account.defer(job);
                    return;
                }
                Some(_) => {}
            }
        }
        if job.shows_busy() && !self.busy && self.queue.interactive.is_empty() {
            self.busy = true;
            self.emit(SyncEvent::Busy(true));
        }
        match job {
            Job::OpenThread(key) => self.open_thread(key, false),
            Job::Prefetch(key) => self.open_thread(key, true),
            Job::ListView(view, more) => self.list_view(view, more),
            Job::Mutate(m) => self.mutate(m),
            Job::Send(s) => self.send(s.op, s.outgoing),
            Job::Draft(d) => self.draft(*d),
            Job::Save(d) => self.save(*d),
            Job::InitialSync(email) => self.initial_sync(&email),
            Job::Labels(email, ids) => self.label_counts(&email, ids),
            Job::FetchMeta(email, ids) => self.fetch_meta(&email, ids),
            Job::History(email) => self.history(&email),
            Job::Refetch(email) => self.refetch(&email),
        }
    }

    pub(super) fn handle(&mut self, cmd: Command) {
        match cmd {
            Command::SaveClient {
                client_id,
                client_secret,
            } => self.save_client(client_id, client_secret),
            Command::ClearClient => self.clear_client(),
            Command::AddAccount => self.begin_sign_in(),
            Command::CancelSignIn => self.cancel_sign_in(),
            Command::SignInFinished(seq, result) => self.finish_sign_in(seq, result),
            Command::RemoveAccount(email) => {
                if self.remove_account(&email, Revoke::WithGoogle) {
                    self.emit(SyncEvent::Notice(format!("Signed out of {email}")));
                }
            }
            Command::Refresh => {
                if self.keyring_failed {
                    self.load_from_keyring();
                } else {
                    // A manual refresh retries now, whatever the backoff said.
                    for account in self.accounts.values_mut() {
                        account.backoff.reset();
                    }
                    self.requeue_deferred(Instant::now());
                    // Labels and page 1 again, then whatever changed since the cursor.
                    let accounts: Vec<(AccountId, bool)> = self
                        .accounts
                        .iter()
                        .map(|(e, a)| (e.clone(), a.synced_once && a.history_id.is_some()))
                        .collect();
                    for (email, synced) in accounts {
                        self.queue.push(Job::InitialSync(email.clone()));
                        if synced {
                            self.queue.push(Job::History(email));
                        }
                    }
                }
            }
            Command::DismissProblems => {
                self.problems.clear();
                self.emit(SyncEvent::Problem(None));
            }
            Command::Tune {
                refresh_secs,
                page_size,
            } => {
                self.tuning.refresh = Duration::from_secs(refresh_secs.max(1));
                self.tuning.page_size = page_size.max(1);
                self.next_poll = self.last_poll + self.tuning.refresh;
            }
            Command::Want { view, more } => {
                if !more {
                    // The next-page tokens of searches no longer shown are dropped, so a long
                    // session of searches does not keep one entry per query.
                    for account in self.accounts.values_mut() {
                        account.pages.retain(|kind, _| {
                            !matches!(kind, ViewKind::Search(_)) || *kind == view.kind
                        });
                    }
                    self.want = Some(view.clone());
                }
                if self.accounts.contains_key(&view.account) {
                    self.queue.push(Job::ListView(view, more));
                } else if !more {
                    // Not (or no longer) signed in: an empty page, so the view never waits.
                    self.emit(SyncEvent::ThreadPage {
                        view,
                        ids: vec![],
                        threads: vec![],
                        mode: PageMode::Replace,
                        has_more: false,
                    });
                }
            }
            Command::OpenThread(key) => self.queue.push(Job::OpenThread(key)),
            Command::Prefetch(key) => {
                if self.accounts.contains_key(&key.account) {
                    self.queue.push(Job::Prefetch(key));
                }
            }
            Command::Modify {
                op,
                account,
                thread_ids,
                delta,
            } => self.queue_mutation(Mutation {
                op,
                account,
                thread_ids,
                change: Change::Modify(delta),
            }),
            Command::Trash {
                op,
                account,
                thread_ids,
            } => self.queue_mutation(Mutation {
                op,
                account,
                thread_ids,
                change: Change::Trash,
            }),
            Command::Untrash {
                op,
                account,
                thread_ids,
                restore,
            } => self.queue_mutation(Mutation {
                op,
                account,
                thread_ids,
                change: Change::Untrash(restore),
            }),
            Command::Send { op, outgoing } => {
                if self.accounts.contains_key(&outgoing.account) {
                    self.queue
                        .push(Job::Send(Box::new(Outbox { op, outgoing })));
                } else {
                    self.emit(SyncEvent::SendFinished {
                        op,
                        error: Some(SendError {
                            message: SIGNED_OUT.into(),
                            maybe_sent: false,
                        }),
                    });
                }
            }
            Command::SaveDraft {
                op,
                outgoing,
                draft_id,
            } => {
                if self.accounts.contains_key(&outgoing.account) {
                    self.queue.push(Job::Draft(Box::new(DraftWork::Save {
                        op,
                        outgoing: Box::new(outgoing),
                        draft_id,
                    })));
                } else {
                    self.emit(SyncEvent::DraftFailed {
                        op,
                        message: SIGNED_OUT.into(),
                    });
                }
            }
            Command::DeleteDraft { account, draft_id } => {
                if self.accounts.contains_key(&account) {
                    self.queue.push(Job::Draft(Box::new(DraftWork::Delete {
                        account,
                        draft_id,
                    })));
                }
            }
            Command::LoadDraft {
                op,
                account,
                thread_id,
            } => {
                if self.accounts.contains_key(&account) {
                    self.queue.push(Job::Draft(Box::new(DraftWork::Load {
                        op,
                        account,
                        thread_id,
                    })));
                } else {
                    self.emit(SyncEvent::DraftLoaded {
                        op,
                        result: Err(SIGNED_OUT.into()),
                    });
                }
            }
            Command::SaveAttachment {
                account,
                message_id,
                attachment,
            } => {
                if self.accounts.contains_key(&account) {
                    self.queue.push(Job::Save(Box::new(Download {
                        account,
                        message_id,
                        attachment,
                    })));
                } else {
                    let name = files::sanitise(&attachment.filename);
                    self.emit(SyncEvent::Notice(format!(
                        "Couldn't save {name}: {SIGNED_OUT}"
                    )));
                }
            }
            Command::Shutdown => {}
        }
    }
}
