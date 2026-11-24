use super::*;

impl Worker {
    /// SPEC 7.3: the history cursor first, then the labels (counts follow in chunks), then
    /// page 1 of the inbox and of the shown view. The first sync never reports new mail. A
    /// repeat after a completed sync (Refresh) keeps the cursor it already has;
    /// a retried first sync takes the new one.
    pub(super) fn initial_sync(&mut self, email: &str) {
        let profile = match self.with_token(email, gmail::profile) {
            Ok(p) => p,
            Err(e) => {
                // Not parked: `synced_once` stays false, so the next poll tries again.
                self.failed(email, &e);
                return;
            }
        };
        self.succeeded(email);
        let synced_before = match self.accounts.get_mut(email) {
            Some(account) => {
                // A repeat after a completed sync keeps the cursor it has: moving it here
                // would skip the changes since then outside page 1, which the next history
                // round still covers. A first sync (or a retried one) takes the profile's, so
                // mail from before it completes is never reported as new.
                let fresh = parse::parse_u64(&profile.history_id);
                if !account.synced_once || account.history_id.is_none() {
                    account.history_id = Some(fresh);
                }
                account.synced_once
            }
            None => return,
        };
        if !self.refresh_labels(email) {
            return;
        }
        // A repeat (Refresh, a retried poll) merges into what the list shows.
        let mode = if synced_before {
            PageMode::Refresh
        } else {
            PageMode::Replace
        };
        for view in self.page_one_views(email) {
            if let Err(e) = self.list(&view, false, mode) {
                self.failed(email, &e);
                return;
            }
        }
        if let Some(account) = self.accounts.get_mut(email) {
            account.synced_once = true;
        }
        if self.problems.remove(email).is_some() {
            self.report();
        }
        self.emit(SyncEvent::Synced(Timestamp::now()));
    }

    /// Fetches the full label list, emits it with the last known counts, and queues the
    /// count refresh for every label the sidebar shows. Returns whether it worked.
    pub(super) fn refresh_labels(&mut self, email: &str) -> bool {
        let list = match self.with_token(email, gmail::labels) {
            Ok(l) => l,
            Err(e) => {
                self.failed(email, &e);
                return false;
            }
        };
        let Some(account) = self.accounts.get_mut(email) else {
            return false;
        };
        let account_id = email.to_string();
        let labels: Vec<Label> = list
            .labels
            .iter()
            .map(|dto| {
                let mut label = parse::label(&account_id, dto);
                if let Some(old) = account.labels.get(&label.id) {
                    label.threads_unread = old.threads_unread;
                    label.threads_total = old.threads_total;
                    label.messages_total = old.messages_total;
                }
                label
            })
            .collect();
        account.labels = labels.iter().map(|l| (l.id.clone(), l.clone())).collect();
        account.dirty = true;
        let shown: Vec<String> = labels
            .iter()
            .filter(|l| shown(l))
            .map(|l| l.id.clone())
            .collect();
        self.emit(SyncEvent::Labels {
            account: account_id.clone(),
            labels,
        });
        for job in chunked(&shown, |ids| Job::Labels(account_id.clone(), ids)) {
            self.queue.push(job);
        }
        true
    }

    /// The inbox, and the shown view when it is another view of this account.
    pub(super) fn page_one_views(&self, email: &str) -> Vec<ViewKey> {
        let inbox = ViewKey {
            account: email.to_string(),
            kind: ViewKind::Label(labels::INBOX.into()),
        };
        let mut views = vec![inbox];
        if let Some(want) = &self.want
            && want.account == email
            && *want != views[0]
        {
            views.push(want.clone());
        }
        views
    }

    /// SPEC 7.5 `ListView`: page 1 replaces the list, a next page appends to it. A
    /// retryable failure parks the job; any other ends the loading state with what is there.
    pub(super) fn list_view(&mut self, view: ViewKey, more: bool) {
        let mode = if more {
            PageMode::Append
        } else {
            PageMode::Replace
        };
        if let Err(e) = self.list(&view, more, mode) {
            let email = view.account.clone();
            if self.failed(&email, &e) {
                self.defer(Job::ListView(view, more));
            } else {
                // A search Gmail refused shows as empty, with the reason in the banner.
                self.emit(SyncEvent::ThreadPage {
                    view,
                    ids: vec![],
                    threads: vec![],
                    mode,
                    has_more: false,
                });
            }
        }
    }

    /// Lists one page of `view` and emits it straight away: cached summaries whose history id
    /// still matches are reused, the other ids show as skeleton rows and are fetched in
    /// chunks ahead of other background work. A `Refresh` never touches the page token.
    pub(super) fn list(
        &mut self,
        view: &ViewKey,
        more: bool,
        mode: PageMode,
    ) -> Result<(), CallError> {
        let email = view.account.as_str();
        let page_token = if more {
            match self
                .accounts
                .get(email)
                .and_then(|a| a.pages.get(&view.kind))
            {
                Some(Some(token)) => Some(token.clone()),
                // No next page: nothing to ask for (the store only asks while `has_more`).
                _ => return Ok(()),
            }
        } else {
            None
        };
        let max = self.tuning.page_size;
        let page = self.with_token(email, |agent, ep, token| {
            gmail::threads(agent, ep, token, &view.kind, page_token.as_deref(), max)
        })?;
        self.succeeded(email);
        let Some(account) = self.accounts.get_mut(email) else {
            return Ok(());
        };
        if mode != PageMode::Refresh {
            account
                .pages
                .insert(view.kind.clone(), page.next_page_token.clone());
        }
        if !more && !matches!(view.kind, ViewKind::Search(_)) {
            let ids = page.threads.iter().map(|r| r.id.clone()).collect();
            account.listed.insert(view.kind.clone(), ids);
            account.dirty = true;
        }
        let mut ids = Vec::with_capacity(page.threads.len());
        let mut threads = Vec::new();
        let mut fetch = Vec::new();
        for r in &page.threads {
            ids.push(r.id.clone());
            let history_id = parse::parse_u64(&r.history_id);
            match account.known.get(&r.id) {
                Some(k) if k.history_id == history_id => {
                    threads.push(k.summary.clone());
                }
                // Refused at this very history id: it stays a skeleton row.
                _ if account.refused.get(&r.id) == Some(&history_id) => {}
                _ => {
                    account.refused.remove(&r.id);
                    account.missing.insert(r.id.clone(), history_id);
                    fetch.push(r.id.clone());
                }
            }
        }
        // A parked chunk fetches its ids once the backoff passes; ids in queued chunks move
        // to the front with this page's.
        let parked = meta_ids(account.deferred.iter(), email);
        fetch.retain(|id| !parked.contains(id));
        self.queue.strip_meta(email, &fetch);
        self.emit(SyncEvent::ThreadPage {
            view: view.clone(),
            ids,
            threads,
            mode,
            has_more: page.next_page_token.is_some(),
        });
        let account_id = email.to_string();
        self.queue.push_soon(chunked(&fetch, |ids| {
            Job::FetchMeta(account_id.clone(), ids)
        }));
        Ok(())
    }

    /// Records a fetched thread and returns its summary (`None` for a thread without
    /// messages).
    pub(super) fn remember(
        &mut self,
        email: &str,
        dto: &gmail::ThreadDto,
    ) -> Option<ThreadSummary> {
        let summary = parse::thread_summary(&email.to_string(), dto)?;
        let account = self.accounts.get_mut(email)?;
        account.missing.remove(&dto.id);
        account.refused.remove(&dto.id);
        account.dirty = true;
        account.known.insert(
            dto.id.clone(),
            Known {
                history_id: summary.history_id,
                star_target: star_target(dto),
                summary: summary.clone(),
            },
        );
        Some(summary)
    }

    /// One `FetchMeta` chunk: each summary lands as soon as the chunk ends. A thread that is
    /// gone is reported gone; a retryable failure parks the rest of the chunk; a refused
    /// thread stays a skeleton, is not fetched again until a listing shows it changed, and
    /// the chunk carries on.
    pub(super) fn fetch_meta(&mut self, email: &str, ids: Vec<String>) {
        let mut updated = Vec::new();
        let mut gone = Vec::new();
        for (i, id) in ids.iter().enumerate() {
            match self.with_token(email, |agent, ep, token| {
                gmail::thread_meta(agent, ep, token, id)
            }) {
                Ok(dto) => {
                    self.succeeded(email);
                    match self.remember(email, &dto) {
                        Some(summary) => updated.push(summary),
                        None => gone.push(id.clone()),
                    }
                }
                Err(CallError::Api(ApiError::NotFound)) => gone.push(id.clone()),
                Err(e) => {
                    if self.failed(email, &e) {
                        self.defer(Job::FetchMeta(email.to_string(), ids[i..].to_vec()));
                        break;
                    }
                    if matches!(e, CallError::Auth | CallError::Gone) {
                        break;
                    }
                    // Refused for good at this history id: not asked again on every poll.
                    if let Some(account) = self.accounts.get_mut(email)
                        && let Some(history_id) = account.missing.remove(id)
                    {
                        account.refused.insert(id.clone(), history_id);
                    }
                }
            }
        }
        if let Some(account) = self.accounts.get_mut(email) {
            for id in &gone {
                account.missing.remove(id);
                account.refused.remove(id);
                account.known.remove(id);
                account.dirty = true;
            }
        }
        if !updated.is_empty() {
            self.emit(SyncEvent::ThreadsUpdated(updated));
        }
        if !gone.is_empty() {
            self.forget_threads(email, &gone);
            self.emit(SyncEvent::ThreadsGone {
                account: email.to_string(),
                ids: gone,
            });
        }
    }

    /// One label-count chunk; the counts land as one `LabelCounts` when it ends.
    pub(super) fn label_counts(&mut self, email: &str, ids: Vec<String>) {
        let mut counted = Vec::new();
        for (i, id) in ids.iter().enumerate() {
            match self.with_token(email, |agent, ep, token| gmail::label(agent, ep, token, id)) {
                Ok(dto) => {
                    self.succeeded(email);
                    let label = parse::label(&email.to_string(), &dto);
                    if let Some(account) = self.accounts.get_mut(email) {
                        account.labels.insert(label.id.clone(), label.clone());
                        account.dirty = true;
                    }
                    counted.push(label);
                }
                // A label deleted since the list: the next full list drops it.
                Err(CallError::Api(ApiError::NotFound)) => {}
                Err(e) => {
                    if self.failed(email, &e) {
                        self.defer(Job::Labels(email.to_string(), ids[i..].to_vec()));
                        break;
                    }
                    if matches!(e, CallError::Auth | CallError::Gone) {
                        break;
                    }
                }
            }
        }
        if !counted.is_empty() {
            self.emit(SyncEvent::LabelCounts {
                account: email.to_string(),
                labels: counted,
            });
        }
    }

    /// SPEC 7.5 `OpenThread`: the full thread for the reader. It never marks the thread
    /// read; the store decides that. The fresh summary (attachments are certain here) goes
    /// to the list too when it changed. A `prefetch` (SPEC 16.11) does the same from the
    /// background lane, but a failure emits nothing, so the reader is unaffected.
    ///
    /// SPEC 7.8: a thread saved on disk at the history id the list knows for it is served
    /// without a request. When a request fails (offline), whatever copy there is is shown
    /// instead of the error.
    pub(super) fn open_thread(&mut self, key: ThreadKey, prefetch: bool) {
        let email = key.account.clone();
        let saved = if self.open_cache().is_some() {
            self.threads.as_ref().and_then(|t| t.load(&key))
        } else {
            None
        };
        let known = self
            .accounts
            .get(&email)
            .and_then(|a| a.known.get(&key.id))
            .map(|k| k.history_id);
        if let Some(saved) = &saved
            && known == Some(saved.history_id)
        {
            self.emit(SyncEvent::ThreadDetail(saved.detail.clone()));
            return;
        }
        match self.with_token(&email, |agent, ep, token| {
            gmail::thread_full(agent, ep, token, &key.id)
        }) {
            Ok(dto) => {
                self.succeeded(&email);
                let before = self
                    .accounts
                    .get(&email)
                    .and_then(|a| a.known.get(&key.id))
                    .map(|k| k.summary.clone());
                let summary = self.remember(&email, &dto);
                if let Some(summary) = &summary
                    && before.as_ref() != Some(summary)
                {
                    self.emit(SyncEvent::ThreadsUpdated(vec![summary.clone()]));
                }
                let mut detail = parse::thread_detail(&email, &dto);
                detail.key = key;
                if let Some(summary) = &summary {
                    self.save_thread(summary.history_id, &detail);
                }
                self.emit(SyncEvent::ThreadDetail(detail));
            }
            Err(e) => {
                let error = e.reader_message();
                self.failed_as(&email, &e, !prefetch);
                match saved {
                    // Old but better than nothing; the list's own refresh corrects it.
                    Some(saved) if !matches!(e, CallError::Api(ApiError::NotFound)) => {
                        self.emit(SyncEvent::ThreadDetail(saved.detail));
                    }
                    _ => {
                        if matches!(e, CallError::Api(ApiError::NotFound)) {
                            self.forget_threads(&email, std::slice::from_ref(&key.id));
                        }
                        if !prefetch {
                            self.emit(SyncEvent::ThreadFailed { key, error });
                        }
                    }
                }
            }
        }
    }

    /// Queues the count refresh for `ids`, keeping only labels the sidebar shows (a count
    /// for any other would add it to the sidebar), in chunks.
    pub(super) fn queue_counts(&mut self, email: &str, ids: impl IntoIterator<Item = String>) {
        let Some(account) = self.accounts.get(email) else {
            return;
        };
        let ids: BTreeSet<String> = ids
            .into_iter()
            .filter(|id| account.labels.get(id).is_some_and(shown))
            .collect();
        let ids: Vec<String> = ids.into_iter().collect();
        for job in chunked(&ids, |ids| Job::Labels(email.to_string(), ids)) {
            self.queue.push(job);
        }
    }

    /// SPEC 7.4: one incremental round. Every history page is read before anything is
    /// committed; then the cursor moves together with the changed threads it implies, and
    /// those are refetched in chunks.
    pub(super) fn history(&mut self, email: &str) {
        let Some(start) = self.accounts.get(email).and_then(|a| a.history_id) else {
            return;
        };
        let mut pages: Vec<gmail::HistoryList> = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            if pages.len() >= MAX_HISTORY_PAGES {
                // Too much to follow change by change: start afresh, as for an expired cursor.
                self.history_expired(email);
                return;
            }
            let token = page_token.clone();
            match self.with_token(email, |agent, ep, t| {
                gmail::history(agent, ep, t, start, token.as_deref())
            }) {
                Ok(page) => {
                    page_token = page.next_page_token.clone();
                    pages.push(page);
                    if page_token.is_none() {
                        break;
                    }
                }
                Err(CallError::Api(ApiError::NotFound)) => {
                    self.history_expired(email);
                    return;
                }
                Err(e) => {
                    // Nothing committed: the next poll reads the same history again.
                    self.failed(email, &e);
                    return;
                }
            }
        }
        self.succeeded(email);
        let Some(account) = self.accounts.get_mut(email) else {
            return;
        };
        account.commit_round(collect_changes(&pages));
        if account.pending_changed.is_empty() {
            self.round_done(email);
            self.emit(SyncEvent::Synced(Timestamp::now()));
        } else {
            self.queue.push(Job::Refetch(email.to_string()));
        }
    }

    /// The end of a history round with nothing left pending. After the first one of a
    /// session that resumed from the saved cursor it refreshes the label list, which the
    /// rounds never do (a label made or renamed meanwhile), and ends the quiet period.
    fn round_done(&mut self, email: &str) {
        let resumed = self
            .accounts
            .get_mut(email)
            .is_some_and(|a| std::mem::take(&mut a.resumed));
        if resumed {
            self.refresh_labels(email);
        }
    }

    /// SPEC 7.4 step 8: Gmail no longer has the history since our cursor.
    pub(super) fn history_expired(&mut self, email: &str) {
        let Some(account) = self.accounts.get_mut(email) else {
            return;
        };
        account.reset();
        if self.open_cache().is_some()
            && let Some(threads) = &self.threads
        {
            threads.clear(email);
        }
        self.queue
            .background
            .retain(|j| j.account() != email || !matches!(j, Job::FetchMeta(..) | Job::Refetch(_)));
        self.emit(SyncEvent::AccountReset(email.to_string()));
        self.queue.push(Job::InitialSync(email.to_string()));
    }

    /// SPEC 7.4 step 4: one chunk of the round's changed threads. A thread that is gone is
    /// reported gone; a retryable failure stops the chunk and leaves the rest pending for the
    /// next round; a thread Gmail refuses for good is dropped from the round. When nothing is
    /// pending any more the round finishes.
    pub(super) fn refetch(&mut self, email: &str) {
        let Some(account) = self.accounts.get_mut(email) else {
            return;
        };
        let ids = account.next_refetch();
        let mut updated = Vec::new();
        let mut gone = Vec::new();
        let mut stopped = false;
        for id in &ids {
            let before: Vec<String> = self
                .accounts
                .get(email)
                .and_then(|a| a.known.get(id))
                .map(|k| k.summary.label_ids.iter().cloned().collect())
                .unwrap_or_default();
            let result = self.with_token(email, |agent, ep, token| {
                gmail::thread_meta(agent, ep, token, id)
            });
            let summary = match result {
                Ok(dto) => {
                    self.succeeded(email);
                    self.remember(email, &dto)
                }
                Err(CallError::Api(ApiError::NotFound)) => None,
                Err(e) => {
                    let retry = self.failed(email, &e);
                    if retry || matches!(e, CallError::Auth | CallError::Gone) {
                        stopped = true;
                        break;
                    }
                    // Refused for good (403, too large): not worth asking every round.
                    if let Some(account) = self.accounts.get_mut(email) {
                        account.refetch_succeeded(id);
                    }
                    continue;
                }
            };
            let Some(account) = self.accounts.get_mut(email) else {
                return;
            };
            account.round_labels.extend(before);
            match summary {
                Some(summary) => {
                    account
                        .round_labels
                        .extend(summary.label_ids.iter().cloned());
                    account.refetch_succeeded(id);
                    updated.push(summary);
                }
                None => {
                    account.refetch_gone(id);
                    gone.push(id.clone());
                }
            }
        }
        if !updated.is_empty() {
            self.emit(SyncEvent::ThreadsUpdated(updated));
        }
        if !gone.is_empty() {
            self.forget_threads(email, &gone);
            self.emit(SyncEvent::ThreadsGone {
                account: email.to_string(),
                ids: gone,
            });
        }
        if stopped {
            return;
        }
        match self.accounts.get(email) {
            Some(a) if !a.pending_changed.is_empty() => {
                self.queue.push(Job::Refetch(email.to_string()));
            }
            Some(_) => self.finish_round(email),
            None => {}
        }
    }

    /// SPEC 7.4 steps 5 to 7, once every changed thread is in: page 1 of the inbox and the
    /// shown view are re-listed, the moved labels' counts refreshed, and new inbox mail
    /// reported (even if the re-list fails, since the candidates are cleared here).
    pub(super) fn finish_round(&mut self, email: &str) {
        let Some(account) = self.accounts.get_mut(email) else {
            return;
        };
        let mut new_mail = account.take_new_mail();
        if account.resumed {
            // What arrived while the app was closed is not announced as if just received.
            new_mail.clear();
        }
        let mut counts = std::mem::take(&mut account.round_labels);
        counts.extend(ROUND_COUNTS.iter().map(|s| s.to_string()));
        let mut listed = true;
        for view in self.page_one_views(email) {
            if let Err(e) = self.list(&view, false, PageMode::Refresh) {
                self.failed(email, &e);
                listed = false;
                break;
            }
        }
        self.queue_counts(email, counts);
        if !new_mail.is_empty() {
            self.emit(SyncEvent::NewMail {
                account: email.to_string(),
                threads: new_mail,
            });
        }
        if listed {
            self.round_done(email);
            self.emit(SyncEvent::Synced(Timestamp::now()));
        }
    }
}
