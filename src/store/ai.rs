use super::*;

impl Store {
    // ----- AI triage (SPEC 16.10) ------------------------------------------------------------

    /// Every signed-in account, lowercased: their mail is never checked or sent as a field.
    pub(super) fn own_accounts(&self) -> Vec<String> {
        self.accounts.iter().map(|a| a.to_lowercase()).collect()
    }

    /// Whether `account` accepted the current disclosure.
    pub fn ai_opted_in(&self, account: &str) -> bool {
        self.prefs.ai.accounts.get(account) == Some(&DISCLOSURE_VERSION)
    }

    /// A key is usable (or this is the demo) and nothing is paused.
    pub(super) fn ai_ready(&self) -> bool {
        (self.mode == Mode::Demo
            || matches!(self.ai.key, KeyState::Saved { .. }) && self.ai_account_ready())
            && self.ai.paused.is_none()
    }

    /// Jev needs no account; Clef needs a valid Cloudflare account ID.
    pub fn ai_account_ready(&self) -> bool {
        self.prefs.ai.backend.clef_selector().is_none()
            || crate::ai::cloudflare::valid_account_id(&self.prefs.ai.cloudflare_account_id)
    }

    /// The message a thread's verdict is about (SPEC 16.6.3).
    pub fn ai_target<'a>(&self, detail: &'a ThreadDetail) -> Option<&'a Message> {
        prompt::target(detail, &self.own_accounts())
    }

    pub(super) fn loaded(&self, key: &ThreadKey) -> Option<&ThreadDetail> {
        match self.details.get(key) {
            Some(DetailState::Loaded(d)) => Some(d),
            _ => None,
        }
    }

    /// The open thread's (or any loaded thread's) target message id.
    pub(super) fn target_id(&self, key: &ThreadKey) -> Option<String> {
        match self.loaded(key) {
            Some(d) => self.ai_target(d).map(|m| m.id.clone()),
            None => self.ai.by_thread.get(key).cloned(),
        }
    }

    /// Label names may be sensitive; emit them only in explicitly enabled diagnostics.
    pub(super) fn debug_label_scores(&self, account: &str, scores: &[(String, f32)]) {
        if !crate::ai::debug_enabled() {
            return;
        }
        let labels = self
            .labels
            .get(account)
            .map(|ls| prompt::candidate_labels(ls))
            .unwrap_or_default();
        crate::ai::debug(format_args!(
            "label scores: count={}, normal_threshold={:.2}",
            scores.len(),
            crate::ai::jev::SUGGEST_MIN
        ));
        for (id, score) in scores {
            let name = labels
                .iter()
                .find(|label| label.id == *id)
                .map(|label| label.name.as_str())
                .unwrap_or("[unavailable label]");
            crate::ai::debug(format_args!("label score: name={name:?}, score={score:.4}"));
        }
    }

    /// The check for `detail`'s target, when SPEC 16.10's conditions hold.
    pub(super) fn check_request(
        &self,
        detail: &ThreadDetail,
        priority: Priority,
    ) -> Option<CheckRequest> {
        let account = &detail.key.account;
        if !self.ai_ready() || !self.ai_opted_in(account) {
            crate::ai::debug(format_args!(
                "check skipped: key={}, paused={}, consent={}",
                crate::ai::key_state_name(&self.ai.key),
                self.ai.paused.is_some(),
                self.ai_opted_in(account)
            ));
            return None;
        }
        let own = self.own_accounts();
        let Some(target) = prompt::target(detail, &own) else {
            crate::ai::debug(format_args!("check skipped: no eligible incoming message"));
            return None;
        };
        let id: MessageKey = (account.clone(), target.id.clone());
        if self.ai.checking.contains(&id)
            || self
                .ai
                .failed
                .get(&id)
                .is_some_and(|(_, retryable)| !retryable)
        {
            crate::ai::debug(format_args!(
                "check skipped: already checking or non-retryable failure"
            ));
            return None;
        }
        // Labels are asked only on open, while suggestions are on (SPEC 16.8, 16.11).
        let labels = (priority == Priority::Interactive)
            .then(|| self.label_ask(account))
            .flatten();
        let verdict = match self.ai.results.get(&id) {
            None => true,
            // Asked before: only the labels again, when the account's labels changed.
            Some(r) => match &labels {
                Some(ask) if r.label_fp != Some(ask.fingerprint) => false,
                _ => {
                    crate::ai::debug(format_args!("check skipped: result already available"));
                    if let Some(scores) = &r.labels {
                        self.debug_label_scores(account, scores);
                    }
                    return None;
                }
            },
        };
        crate::ai::debug(format_args!(
            "check requested: priority={priority:?}, verdict={verdict}, labels={}",
            labels.is_some()
        ));
        Some(CheckRequest {
            account: account.clone(),
            thread: detail.key.clone(),
            message_id: target.id.clone(),
            priority,
            verdict,
            labels,
            input: AiInput::from_message(target, &Redact::for_message(target, &own)),
        })
    }

    /// The label questions for a check in `account`, while suggestions are on and the
    /// account has labels to offer.
    pub(super) fn label_ask(&self, account: &str) -> Option<LabelAsk> {
        if !self.prefs.ai.suggest_labels {
            return None;
        }
        let labels = self.labels.get(account)?;
        let group = self.prefs.ai.required_label_group.trim();
        let candidates = prompt::label_candidates_for_group(labels, group);
        let guidance = prompt::cut_chars(
            prompt::strip_controls(&self.prefs.ai.label_guidance).trim(),
            crate::config::LABEL_GUIDANCE_MAX_CHARS,
        )
        .0;
        (!candidates.is_empty()).then(|| LabelAsk {
            fingerprint: prompt::label_fingerprint_with_options(labels, &guidance, group),
            guidance,
            candidates,
        })
    }

    /// Checks the open thread's target when it is due (SPEC 16.10).
    pub(super) fn check_open(&mut self, key: &ThreadKey) {
        if self.open.as_ref() != Some(key) {
            crate::ai::debug(format_args!("open check skipped: thread is no longer open"));
            return;
        }
        let Some(detail) = self.loaded(key) else {
            crate::ai::debug(format_args!(
                "open check waiting: thread details not loaded"
            ));
            return;
        };
        if let Some(req) = self.check_request(detail, Priority::Interactive) {
            self.send_ai(AiCommand::Check(Box::new(req)));
        }
    }

    /// The reader's "Retry" after a retryable failure: clears it and checks again.
    pub fn retry_ai(&mut self) {
        let Some(key) = self.open.clone() else {
            return;
        };
        if let Some(id) = self.target_id(&key) {
            self.ai.failed.remove(&(key.account.clone(), id));
            self.ai.bump(&key);
        }
        self.check_open(&key);
    }

    /// Settings' Test: the typed key, or the saved one with `None`.
    pub fn test_ai_key(&mut self, key: Option<crate::ai::openrouter::ApiKey>) {
        self.ai.test = None;
        self.ai.testing = true;
        self.send_ai(AiCommand::TestKey(key));
    }

    /// Turns the AI checks on (at the current disclosure) or off for an account.
    pub fn set_ai_consent(&mut self, account: &str, on: bool) {
        self.change_prefs(|p| {
            if on {
                p.ai.accounts
                    .insert(account.to_string(), DISCLOSURE_VERSION);
            } else {
                p.ai.accounts.remove(account);
            }
        });
    }

    /// The open thread's suggested labels (SPEC 16.8): none while suggestions are off, while
    /// the target's check is in flight (SPEC 16.12.3), for a dismissed row, or for risky mail (a Warning or Danger verdict, or a local caution),
    /// so one key press cannot file a scam among real mail. Labels already on the thread are
    /// left out, so an applied chip disappears without a new request.
    pub fn suggestions(&self, key: &ThreadKey) -> Vec<Suggestion> {
        self.suggestions_inner(key).unwrap_or_default()
    }

    pub(super) fn suggestions_inner(&self, key: &ThreadKey) -> Option<Vec<Suggestion>> {
        if !self.prefs.ai.suggest_labels || !self.ai_opted_in(&key.account) {
            return None;
        }
        let detail = self.loaded(key)?;
        let target = self.ai_target(detail)?;
        let id = (key.account.clone(), target.id.clone());
        if self.ai.checking.contains(&id)
            || self
                .ai
                .dismissed
                .contains(&(key.account.clone(), format!("labels:{}", target.id)))
        {
            return None;
        }
        let result = self.ai.results.get(&id)?;
        if result.label_fp.is_some()
            && result.label_fp != self.label_ask(&key.account).map(|ask| ask.fingerprint)
        {
            return None;
        }
        let risky = result.verdict.is_some_and(|v| v.level >= Level::Warning)
            || crate::ui::triage::local_caution(&target.signals).is_some();
        let probs = result.labels.as_ref()?;
        let names = prompt::candidate_labels(self.labels.get(&key.account)?);
        let applied = match self.threads.get(key) {
            Some(s) => &s.label_ids,
            None => &detail.label_ids,
        };
        let required =
            required_group_suggestion(probs, &names, applied, &self.prefs.ai.required_label_group);
        let mut suggestions = if risky {
            Vec::new()
        } else {
            select_suggestions(probs, &names, applied)
        };
        if let Some(required) = required {
            // Keep other qualifying labels; add the required group's best child if absent.
            suggestions.retain(|s| {
                !s.name
                    .eq_ignore_ascii_case(self.prefs.ai.required_label_group.trim())
            });
            if !suggestions.iter().any(|s| s.id == required.id) {
                suggestions.insert(0, required);
            }
        }
        Some(suggestions)
    }

    /// Applies the open thread's `n`th suggestion (0-based) to that thread only, through the
    /// label picker's undoable path. Nothing when there is no such suggestion.
    pub fn apply_suggestion(&mut self, n: usize) -> Option<OpId> {
        let key = self.open.clone()?;
        let s = self.suggestions(&key).into_iter().nth(n)?;
        let op = self.set_label(vec![key.clone()], &s.id, true);
        self.ai.bump(&key);
        op
    }

    /// Hides the open thread's suggestion row for this session.
    pub fn dismiss_suggestions(&mut self) {
        let Some(key) = self.open.clone() else {
            return;
        };
        if let Some(id) = self.target_id(&key) {
            self.ai
                .dismissed
                .insert((key.account.clone(), format!("labels:{id}")));
            self.ai.bump(&key);
        }
    }

    /// Hides the open thread's banner for this session.
    pub fn dismiss_triage(&mut self) {
        let Some(key) = self.open.clone() else {
            return;
        };
        if let Some(id) = self.target_id(&key) {
            self.ai.dismissed.insert((key.account.clone(), id));
            self.ai.bump(&key);
        }
    }

    pub(super) fn on_detail(&mut self, detail: ThreadDetail) {
        let key = detail.key.clone();
        let is_open = self.open.as_ref() == Some(&key);
        let prefetched = self.ai.prefetching.remove(&key).is_some();
        if !is_open && prefetched {
            // Re-checked now: whatever held when the prefetch went out is not trusted.
            let due = self.prefs.ai.background;
            let req = due
                .then(|| self.check_request(&detail, Priority::Background))
                .flatten();
            if let Some(req) = req {
                self.send_ai(AiCommand::Check(Box::new(req)));
            }
            // A prefetch never evicts the user's cached threads; a cached one is refreshed.
            if !self.details.contains_key(&key) {
                return;
            }
        }
        if let Some(s) = self.threads.get(&key) {
            self.detail_history.insert(key.clone(), s.history_id);
        }
        self.details
            .insert(key.clone(), DetailState::Loaded(detail));
        self.touch_detail(&key);
        if is_open {
            self.check_open(&key);
        }
    }

    /// Background checks of new inbox mail (SPEC 16.11): a prefetch per new thread, while the
    /// pref is on, the account opted in, the key usable and the hourly cap has room.
    pub(super) fn prefetch_new_mail(&mut self, account: &str, threads: &[ThreadSummary]) {
        if !self.prefs.ai.background || !self.ai_opted_in(account) || !self.ai_ready() {
            return;
        }
        let now = Instant::now();
        let times = self.ai.hourly.entry(account.to_string()).or_default();
        times.retain(|t| now.duration_since(*t) < HOUR);
        let mut send = Vec::new();
        for t in threads {
            if times.len() >= BACKGROUND_PER_HOUR {
                break;
            }
            if self.ai.prefetching.contains_key(&t.key) {
                continue;
            }
            times.push_back(now);
            self.ai.prefetching.insert(t.key.clone(), now);
            send.push(t.key.clone());
        }
        for key in send {
            self.send_command(Command::Prefetch(key));
        }
    }

    /// The thread's change counter, for the reader's fingerprint.
    pub fn ai_thread_rev(&self, key: &ThreadKey) -> u64 {
        self.ai.rev.get(key).copied().unwrap_or(0)
    }

    pub(super) fn result_for(&self, key: &ThreadKey) -> Option<(&String, &AiResult)> {
        let id = self.ai.by_thread.get(key)?;
        let r = self.ai.results.get(&(key.account.clone(), id.clone()))?;
        Some((id, r))
    }

    /// The list badge's verdict, for threads checked this session; `Level::Spam` is hidden in
    /// the Spam view.
    pub fn ai_badge_verdict(&self, key: &ThreadKey) -> Option<Verdict> {
        let v = self.result_for(key)?.1.verdict?;
        match v.level {
            Level::None => None,
            Level::Spam if self.viewing_label(labels::SPAM) => None,
            _ => Some(v),
        }
    }

    /// The list badge's level (SPEC 16.10).
    #[cfg(test)]
    pub fn ai_badge(&self, key: &ThreadKey) -> Option<Level> {
        self.ai_badge_verdict(key).map(|v| v.level)
    }

    /// The reader banner's verdict: the open target's, unless dismissed, clean, or spam in
    /// the Spam view.
    pub fn ai_banner(&self, key: &ThreadKey) -> Option<&Verdict> {
        let id = self.target_id(key)?;
        let mk = (key.account.clone(), id);
        if self.ai.dismissed.contains(&mk) {
            return None;
        }
        let v = self.ai.results.get(&mk)?.verdict.as_ref()?;
        match v.level {
            Level::None => None,
            Level::Spam if self.viewing_label(labels::SPAM) => None,
            _ => Some(v),
        }
    }

    /// Whether the open target's check is in flight.
    pub fn ai_checking(&self, key: &ThreadKey) -> bool {
        self.target_id(key)
            .is_some_and(|id| self.ai.checking.contains(&(key.account.clone(), id)))
    }

    /// The open target's failure, when its check failed.
    pub fn ai_failure(&self, key: &ThreadKey) -> Option<&(String, bool)> {
        let id = self.target_id(key)?;
        self.ai.failed.get(&(key.account.clone(), id))
    }

    /// The local caution's reasons for a loaded thread's target (SPEC 16.12.1): any account,
    /// no key, no request.
    pub fn local_caution(&self, key: &ThreadKey) -> Option<Vec<String>> {
        let target = self.ai_target(self.loaded(key)?)?;
        crate::ui::triage::local_caution(&target.signals)
    }

    /// Whether a click on a web link in `message_id` must ask first: the message is the
    /// target and Jev called it dangerous, or the local caution applies (SPEC 16.12.1).
    pub fn link_guard(&self, key: &ThreadKey, message_id: &str) -> bool {
        let Some(detail) = self.loaded(key) else {
            return false;
        };
        let Some(target) = self.ai_target(detail) else {
            return false;
        };
        if target.id != message_id {
            return false;
        }
        let danger = self
            .ai
            .results
            .get(&(key.account.clone(), target.id.clone()))
            .and_then(|r| r.verdict)
            .is_some_and(|v| v.level == Level::Danger);
        danger || crate::ui::triage::local_caution(&target.signals).is_some()
    }

    /// Applies one AI worker event (SPEC 16.10).
    pub fn apply_ai(&mut self, ev: AiEvent) {
        match &ev {
            AiEvent::Key(state) => crate::ai::debug(format_args!(
                "key state: {}",
                crate::ai::key_state_name(state)
            )),
            AiEvent::Checking { .. } => crate::ai::debug(format_args!("check started")),
            AiEvent::Checked { outcome, .. } => crate::ai::debug(format_args!(
                "check completed: verdict_level={:?}, labels={}",
                outcome.verdict.map(|v| v.level),
                outcome.labels.is_some()
            )),
            AiEvent::Failed { retryable, .. } => {
                crate::ai::debug(format_args!("check failed: retryable={retryable}"))
            }
            AiEvent::Dropped { .. } => crate::ai::debug(format_args!("check dropped")),
            AiEvent::Paused(reason) => crate::ai::debug(format_args!(
                "pause state: {}",
                if reason.is_some() {
                    "paused"
                } else {
                    "resumed"
                }
            )),
            AiEvent::TestResult(result) => crate::ai::debug(format_args!(
                "key test: {}",
                if result.is_ok() { "passed" } else { "failed" }
            )),
            AiEvent::Usage { .. } => {}
        }
        match ev {
            AiEvent::Key(k) => {
                let was_ready = self.ai_ready();
                if !matches!(k, KeyState::Saved { .. }) {
                    self.ai.checking.clear();
                }
                if k == KeyState::Missing {
                    self.ai.forget_all();
                }
                self.ai.key = k;
                self.ai.ai_rev += 1;
                if !was_ready
                    && self.ai_ready()
                    && let Some(key) = self.open.clone()
                {
                    self.check_open(&key);
                }
            }
            AiEvent::Checking { thread, message_id } => {
                self.ai
                    .checking
                    .insert((thread.account.clone(), message_id));
                self.ai.bump(&thread);
            }
            AiEvent::Checked {
                thread,
                message_id,
                outcome,
            } => {
                let id = (thread.account.clone(), message_id.clone());
                self.ai.checking.remove(&id);
                if !self.ai_opted_in(&thread.account) {
                    return;
                }
                self.ai.failed.remove(&id);
                if let Some((_, scores)) = &outcome.labels {
                    self.debug_label_scores(&thread.account, scores);
                }
                let unlabelled = outcome.labels.is_none();
                let (labels, label_fp) = match outcome.labels {
                    Some((fp, probs)) => (Some(probs), fp),
                    None => (None, None),
                };
                self.ai.insert_result(
                    id,
                    AiResult {
                        verdict: outcome.verdict,
                        labels,
                        label_fp,
                    },
                );
                self.ai.by_thread.insert(thread.clone(), message_id);
                self.ai.bump(&thread);
                // A background check that was queued or in flight when the thread opened
                // answered without labels: ask them now rather than on the next open.
                if unlabelled
                    || label_fp.is_some()
                        && label_fp != self.label_ask(&thread.account).map(|ask| ask.fingerprint)
                {
                    self.check_open(&thread);
                }
            }
            AiEvent::Failed {
                thread,
                message_id,
                error,
                retryable,
            } => {
                let id = (thread.account.clone(), message_id);
                self.ai.checking.remove(&id);
                // A failure sent before an opt-out or checker switch shows nothing.
                if !self.ai_opted_in(&thread.account) {
                    return;
                }
                // A failed labels-only re-ask leaves the stored verdict standing: no failure
                // caption, and the labels are asked again on the next open.
                if !self.ai.results.contains_key(&id) {
                    self.ai.failed.insert(id, (error, retryable));
                }
                self.ai.bump(&thread);
            }
            AiEvent::Dropped { thread, message_id } => {
                self.ai
                    .checking
                    .remove(&(thread.account.clone(), message_id));
                self.ai.bump(&thread);
            }
            AiEvent::Paused(reason) => {
                let was_ready = self.ai_ready();
                if reason.is_some() {
                    self.ai.checking.clear();
                }
                self.ai.paused = reason;
                self.ai.ai_rev += 1;
                if !was_ready
                    && self.ai_ready()
                    && let Some(key) = self.open.clone()
                {
                    self.check_open(&key);
                }
            }
            AiEvent::Usage {
                checks,
                cost_usd,
                cost_unknown,
            } => self.ai.usage = (checks, cost_usd, cost_unknown),
            AiEvent::TestResult(r) => {
                self.ai.testing = false;
                self.ai.test = Some(r);
            }
        }
    }
}
