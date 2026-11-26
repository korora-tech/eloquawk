use super::*;

impl Store {
    // ----- events -----

    /// Applies pending optimistic deltas over a fresh summary, in op order.
    pub(super) fn reapply_pending(&self, mut s: ThreadSummary) -> ThreadSummary {
        for p in self.pending.values() {
            if p.account == s.key.account && p.threads.iter().any(|t| t.key == s.key) {
                p.kind.effective().apply_to(&mut s.label_ids);
            }
        }
        s
    }

    /// Where pending ops leave a thread's membership of a list: `Some(false)` when the last
    /// op to affect it took the thread out, `Some(true)` when it brought it in, `None` when
    /// no pending op changes it.
    pub(super) fn pending_membership(&self, view: &ViewKey, key: &ThreadKey) -> Option<bool> {
        let current = self.threads.get(key);
        let mut member = None;
        for p in self.pending.values() {
            if p.account != view.account || !p.threads.iter().any(|t| t.key == *key) {
                continue;
            }
            let delta = p.kind.effective();
            let all_labels = self
                .labels
                .get(&p.account)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            if removed_by(&view.kind, &delta, all_labels) {
                member = Some(false);
            } else if current.is_some_and(|s| gained_by(&view.kind, &delta, s)) {
                member = Some(true);
            }
        }
        member
    }

    pub(super) fn upsert(&mut self, s: ThreadSummary) {
        let s = self.reapply_pending(s);
        self.threads.insert(s.key.clone(), s);
    }

    pub(super) fn on_page(
        &mut self,
        view: ViewKey,
        ids: Vec<String>,
        threads: Vec<ThreadSummary>,
        mode: PageMode,
        has_more: bool,
    ) {
        for s in threads {
            self.upsert(s);
        }
        let mut seen = HashSet::new();
        let ids: Vec<String> = ids
            .into_iter()
            .filter(|id| {
                seen.insert(id.clone())
                    && self.pending_membership(
                        &view,
                        &ThreadKey::new(view.account.as_str(), id.as_str()),
                    ) != Some(false)
            })
            .collect();
        let old_ids = self.current_ids();
        let list = self.lists.entry(view.clone()).or_default();
        match mode {
            PageMode::Replace => {
                list.ids = ids;
                list.pages = 1;
                list.has_more = has_more;
                list.loading = false;
            }
            PageMode::Append => {
                let mut present: HashSet<String> = list.ids.iter().cloned().collect();
                for id in ids {
                    if present.insert(id.clone()) {
                        list.ids.push(id);
                    }
                }
                list.pages += 1;
                list.has_more = has_more;
                list.loading = false;
            }
            PageMode::Refresh if list.pages == 0 => {
                list.ids = ids;
                list.pages = 1;
                list.has_more = has_more;
            }
            PageMode::Refresh => {
                list.ids = merge_refresh(&list.ids, &ids);
                if list.pages <= 1 {
                    list.has_more = has_more;
                }
            }
        }
        // A page listed before the worker applied a pending op lacks the threads that op
        // brings into this view; put them back as the store already showed them.
        let mut joining: Vec<ThreadKey> = self
            .pending
            .values()
            .filter(|p| p.account == view.account)
            .flat_map(|p| p.threads.iter().map(|t| t.key.clone()))
            .collect();
        joining.sort();
        joining.dedup();
        for key in joining {
            if self.pending_membership(&view, &key) == Some(true) {
                self.insert_sorted(&view, &key);
            }
        }
        if self.view.as_ref() == Some(&view) {
            self.settle(&old_ids, false);
        }
    }

    pub(super) fn on_mutation_finished(&mut self, op: OpId, error: Option<String>) {
        let Some(p) = self.pending.remove(&op) else {
            return;
        };
        let Some(error) = error else {
            return;
        };
        let old_ids = self.current_ids();
        // The thread goes back to its snapshot with the deltas of other still-pending ops
        // on it re-applied, and only the lists this op changed are put back.
        let mut back: Positions = Vec::new();
        let mut by_date: Vec<(ViewKey, ThreadKey)> = Vec::new();
        for snap in &p.threads {
            let restored = self.reapply_pending(snap.before.clone());
            if let Some(current) = self.threads.get(&snap.key).cloned() {
                self.adjust_counts(&current, &restored);
            }
            self.threads.insert(snap.key.clone(), restored);
            let mut places = Vec::new();
            for view in &snap.touched {
                let was = snap
                    .positions
                    .iter()
                    .find(|(v, _)| v == view)
                    .map(|(_, ix)| *ix);
                let want = self
                    .pending_membership(view, &snap.key)
                    .unwrap_or(was.is_some());
                let Some(list) = self.lists.get_mut(view) else {
                    continue;
                };
                let present = list.ids.contains(&snap.key.id);
                match (want, present, was) {
                    (true, false, Some(ix)) => places.push((view.clone(), ix)),
                    (true, false, None) => by_date.push((view.clone(), snap.key.clone())),
                    (false, true, _) => list.ids.retain(|id| *id != snap.key.id),
                    _ => {}
                }
            }
            back.push((snap.key.clone(), places));
        }
        self.reinsert(&back);
        for (view, key) in by_date {
            self.insert_sorted(&view, &key);
        }
        self.settle(&old_ids, false);
        // The failure toast replaces any Undo toast, and with it the undo slot.
        self.post_notice(format!("Couldn't {}: {error}", p.kind.verb()), None);
    }

    /// Drops everything cached for threads of `account` matching `gone`.
    pub(super) fn drop_threads(&mut self, gone: impl Fn(&ThreadKey) -> bool) {
        self.threads.retain(|k, _| !gone(k));
        self.details.retain(|k, _| !gone(k));
        self.detail_history.retain(|k, _| !gone(k));
        self.detail_order.retain(|k| !gone(k));
        self.selected.retain(|k| !gone(k));
        if self.open.as_ref().is_some_and(&gone) {
            self.open = None;
        }
        if self.cursor.as_ref().is_some_and(&gone) {
            self.cursor = None;
        }
    }

    pub(super) fn on_accounts(&mut self, accounts: Vec<AccountId>) {
        let known: HashSet<AccountId> = accounts.iter().cloned().collect();
        if accounts.is_empty() {
            self.last_sync = None;
        }
        self.labels.retain(|a, _| known.contains(a));
        self.send_as.retain(|a, _| known.contains(a));
        self.lists.retain(|v, _| known.contains(&v.account));
        self.drop_threads(|k| !known.contains(&k.account));
        self.new_mail.retain(|s| known.contains(&s.key.account));
        // The runtime AI state goes with the account; its consent stays (SPEC 16.9).
        for gone in self.accounts.iter().filter(|a| !known.contains(*a)) {
            self.ai.forget_account(gone);
        }
        self.accounts = accounts;
        let view_ok = self
            .view
            .as_ref()
            .is_some_and(|v| known.contains(&v.account));
        if view_ok {
            return;
        }
        let from_prefs = self
            .prefs
            .last_view
            .as_ref()
            .filter(|(a, _)| known.contains(a))
            .map(|(a, id)| ViewKey {
                account: a.clone(),
                kind: if id == ALL_MAIL_ID {
                    ViewKind::AllMail
                } else {
                    ViewKind::Label(id.clone())
                },
            });
        let next = from_prefs.or_else(|| {
            self.accounts.first().map(|a| ViewKey {
                account: a.clone(),
                kind: ViewKind::Label(labels::INBOX.into()),
            })
        });
        match next {
            Some(v) => self.set_view(v),
            None => {
                self.view = None;
                self.cursor = None;
                self.open = None;
            }
        }
    }

    /// Applies one worker event.
    pub fn apply(&mut self, ev: SyncEvent) {
        match ev {
            SyncEvent::Auth(a) => {
                self.auth = a;
                // The AI worker's first keyring read waits for the sync worker's (SPEC 16.4).
                if a != AuthState::Starting && !self.ai.reloaded {
                    self.ai.reloaded = true;
                    self.send_ai(AiCommand::Reload);
                }
            }
            SyncEvent::Client(c) => self.client_id = c,
            SyncEvent::Accounts(a) => self.on_accounts(a),
            SyncEvent::SigningIn(s) => self.signing_in = s,
            SyncEvent::Busy(b) => self.busy = b,
            SyncEvent::Synced(t) => {
                self.last_sync = Some(t);
                self.syncs += 1;
                self.ai
                    .prefetching
                    .retain(|_, at| at.elapsed() < PREFETCH_TTL);
            }
            SyncEvent::Problem(p) => self.problem = p,
            SyncEvent::Notice(text) => self.post_notice(text, None),
            SyncEvent::NudgeDismissals { account, dismissed } => {
                self.on_nudge_dismissals(account, dismissed)
            }
            SyncEvent::Saved(path) => {
                let text = saved_text(&path);
                self.post_notice(text, Some(NoticeAction::OpenFolder(path)));
            }
            SyncEvent::SendAs { account, aliases } => {
                self.send_as.insert(account, aliases);
            }
            SyncEvent::Labels { account, labels } => {
                self.labels.insert(account, labels);
            }
            SyncEvent::LabelCounts { account, labels } => {
                let existing = self.labels.entry(account).or_default();
                for l in labels {
                    match existing.iter_mut().find(|e| e.id == l.id) {
                        Some(e) => {
                            e.threads_unread = l.threads_unread;
                            e.threads_total = l.threads_total;
                            e.messages_total = l.messages_total;
                        }
                        None => existing.push(l),
                    }
                }
            }
            SyncEvent::ThreadPage {
                view,
                ids,
                threads,
                mode,
                has_more,
            } => self.on_page(view, ids, threads, mode, has_more),
            SyncEvent::ThreadsUpdated(threads) => {
                for s in threads {
                    self.upsert(s);
                }
            }
            SyncEvent::ThreadsGone { account, ids } => {
                let old_ids = self.current_ids();
                let gone: HashSet<String> = ids.into_iter().collect();
                for (view, list) in self.lists.iter_mut() {
                    if view.account == account {
                        list.ids.retain(|id| !gone.contains(id));
                    }
                }
                self.settle(&old_ids, true);
                self.drop_threads(|k| k.account == account && gone.contains(&k.id));
                self.settle(&old_ids, false);
            }
            SyncEvent::ThreadDetail(detail) => self.on_detail(detail),
            SyncEvent::ThreadFailed { key, error } => {
                self.details.insert(key, DetailState::Failed(error));
            }
            SyncEvent::MutationFinished { op, error } => self.on_mutation_finished(op, error),
            SyncEvent::DraftSaved { op, draft_id } => {
                if let Some((account, _)) = self.discard_saves.remove(&op) {
                    // Discarded while saving: the save made (or kept) a draft nobody wants.
                    self.drafts.remove(&op);
                    self.delete_draft(&account, &draft_id);
                } else {
                    self.drafts.insert(op, DraftOp::Saved(draft_id));
                }
            }
            SyncEvent::DraftFailed { op, message } => {
                if let Some((account, known)) = self.discard_saves.remove(&op) {
                    self.drafts.remove(&op);
                    if let Some(id) = known {
                        self.delete_draft(&account, &id);
                    }
                } else {
                    self.drafts.insert(op, DraftOp::Failed(message));
                }
            }
            SyncEvent::DraftLoaded { op, result } => {
                self.drafts.insert(op, DraftOp::Loaded(result));
            }
            SyncEvent::SendFinished { op, error } => {
                // The message is out: its draft is no longer wanted. A failed send keeps it.
                if let Some((account, draft_id)) = self.send_drafts.remove(&op)
                    && error.is_none()
                {
                    self.delete_draft(&account, &draft_id);
                }
                let thread = self.send_threads.remove(&op);
                let watched = !self.abandoned.remove(&op);
                match error {
                    None => {
                        if watched {
                            self.sends.insert(op, SendState::Sent);
                        }
                        self.post_notice("Message sent".into(), None);
                        // The cached detail lacks the new message: fetch it again, keeping
                        // the old one on screen until the new one arrives.
                        if let Some(key) = thread
                            && self.details.contains_key(&key)
                        {
                            self.send_command(Command::OpenThread(key));
                        }
                    }
                    Some(e) if watched => {
                        self.sends.insert(op, SendState::Failed(e));
                    }
                    // Its window is gone, so the failure would otherwise go unseen.
                    Some(e) => {
                        let text = if e.maybe_sent {
                            "The message may not have been sent"
                        } else {
                            "Couldn't send the message"
                        };
                        self.post_notice(text.into(), None);
                    }
                }
            }
            SyncEvent::NewMail { account, threads } => {
                // Only the account the event names: a summary filed under another is a
                // worker bug, and must not notify.
                let threads: Vec<ThreadSummary> = threads
                    .into_iter()
                    .filter(|t| t.key.account == account)
                    .collect();
                self.prefetch_new_mail(&account, &threads);
                if self.prefs.notifications {
                    self.new_mail.extend(threads);
                }
            }
            SyncEvent::AccountReset(account) => {
                self.lists.retain(|v, _| v.account != account);
                self.drop_threads(|k| k.account == account);
                self.send_ai(AiCommand::ForgetAccount(account.clone()));
                self.ai.forget_account(&account);
            }
        }
    }
}
