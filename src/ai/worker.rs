//! The `eloquawk-ai` thread (SPEC 16.5): one request at a time, an interactive slot that the
//! newest open replaces, a background FIFO capped at 50, the answer cache, retries with
//! backoff, pauses, usage totals and the session spend limit.
//!
//! Every `Checking` it emits is followed by exactly one `Checked`, `Failed` or `Dropped` for the
//! same message. Commands are always drained before the next request starts, so a revocation
//! (opt-out, removed key) sent before a retry is due always wins over that retry, and one that
//! arrives while a request is on the wire discards its result.

use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde_json::json;

use super::cache::{CACHE_SIZE, CacheEntry, CacheKey, DecisionCache};
use super::fake::FakeDecider;
use super::jev::{
    DecisionsRequest, DecisionsResponse, Question, THRESHOLDS, validate, verdict_from,
};
use super::openrouter::{self, AiEndpoints, AiError, ApiKey, BACKOFF_CAP, Backoff};
use super::prompt::{TRIAGE_IDS, fit, shrink, triage_request};
use super::{AiCommand, AiEvent, AiHandle, CheckRequest, KeyState, Outcome, Priority};
use crate::config::Prefs;
use crate::model::AccountId;
use crate::secrets::{SecretError, Secrets};
use crate::store::Mode;

/// The background FIFO's capacity; on overflow the oldest is dropped.
pub const QUEUE_CAP: usize = 50;
/// Attempts per check: an interactive one gets two retries, a background one three attempts.
pub const MAX_ATTEMPTS: u32 = 3;
/// The longest the loop sleeps without a command.
const IDLE: Duration = Duration::from_secs(30);
/// What an unknown `usage.cost` counts as towards the spend limit (SPEC 16.11).
pub const UNKNOWN_COST: f64 = 0.001;

/// Answers a decision request: OpenRouter, or the offline checker.
pub trait Decider: Send {
    fn decide(
        &mut self,
        key: Option<&ApiKey>,
        req: &DecisionsRequest,
    ) -> Result<DecisionsResponse, AiError>;
    /// The credit left on a key in USD, `None` without a limit.
    fn credit(&mut self, key: &ApiKey) -> Result<Option<f64>, AiError>;
}

/// OpenRouter over ureq.
pub struct HttpDecider {
    agent: ureq::Agent,
    endpoints: AiEndpoints,
}

impl HttpDecider {
    pub fn new(endpoints: AiEndpoints) -> Self {
        Self {
            agent: openrouter::agent(),
            endpoints,
        }
    }
}

impl Decider for HttpDecider {
    fn decide(
        &mut self,
        key: Option<&ApiKey>,
        req: &DecisionsRequest,
    ) -> Result<DecisionsResponse, AiError> {
        let key = key.ok_or(AiError::KeyRejected)?;
        let body = openrouter::post_decisions(&self.agent, &self.endpoints, key, &req.to_bytes())?;
        serde_json::from_slice(&body).map_err(|_| AiError::Decode)
    }

    fn credit(&mut self, key: &ApiKey) -> Result<Option<f64>, AiError> {
        openrouter::get_key(&self.agent, &self.endpoints, key)
    }
}

impl Decider for FakeDecider {
    fn decide(
        &mut self,
        _: Option<&ApiKey>,
        req: &DecisionsRequest,
    ) -> Result<DecisionsResponse, AiError> {
        FakeDecider::decide(self, req)
    }

    fn credit(&mut self, _: &ApiKey) -> Result<Option<f64>, AiError> {
        Ok(None)
    }
}

/// Starts the worker against `endpoints` (tests point them at a loopback server). Demo mode
/// never builds an agent and never touches the secrets.
pub(super) fn start_with(
    mode: Mode,
    secrets: Option<Secrets>,
    prefs: &Prefs,
    endpoints: AiEndpoints,
) -> std::io::Result<(AiHandle, async_channel::Receiver<AiEvent>)> {
    let decider: Box<dyn Decider> = match mode {
        Mode::Demo => Box::new(FakeDecider::default()),
        Mode::Google => Box::new(HttpDecider::new(endpoints)),
    };
    let secrets = if mode == Mode::Demo { None } else { secrets };
    spawn(mode, secrets, decider, prefs.ai.spend_limit_cents)
}

fn spawn(
    mode: Mode,
    secrets: Option<Secrets>,
    decider: Box<dyn Decider>,
    limit_cents: u32,
) -> std::io::Result<(AiHandle, async_channel::Receiver<AiEvent>)> {
    let (tx, rx) = mpsc::channel();
    let (etx, erx) = async_channel::unbounded();
    let worker = Worker::new(mode, secrets, decider, etx, rx, limit_cents);
    std::thread::Builder::new()
        .name("eloquawk-ai".into())
        .spawn(move || worker.run())?;
    Ok((AiHandle::from_sender(tx), erx))
}

struct Job {
    req: CheckRequest,
    attempts: u32,
    /// Retried after a 413 with the smallest body.
    shrunk: bool,
    /// The revocation epoch when it was accepted; a later revocation discards it.
    epoch: (u64, u64),
}

impl Job {
    fn id(&self) -> CacheKey {
        (self.req.account.clone(), self.req.message_id.clone())
    }
}

struct Worker {
    mode: Mode,
    secrets: Option<Secrets>,
    decider: Box<dyn Decider>,
    events: async_channel::Sender<AiEvent>,
    rx: mpsc::Receiver<AiCommand>,
    key: Option<ApiKey>,
    /// OpenRouter refused the saved key (401).
    rejected: bool,
    paused: Option<String>,
    /// The pause is the spend limit's, which "Try again" lifts by another limit.
    spend_paused: bool,
    slot: Option<Job>,
    queue: VecDeque<Job>,
    retry_at: Option<Instant>,
    in_flight: Option<CacheKey>,
    cache: DecisionCache,
    checks: u32,
    cost: f64,
    cost_unknown: u32,
    limit_cents: u32,
    resumes: u32,
    /// Bumped by `ClearKey`.
    global_epoch: u64,
    /// Bumped by `ForgetAccount`.
    epochs: HashMap<AccountId, u64>,
    shutdown: bool,
}

impl Worker {
    fn new(
        mode: Mode,
        secrets: Option<Secrets>,
        decider: Box<dyn Decider>,
        events: async_channel::Sender<AiEvent>,
        rx: mpsc::Receiver<AiCommand>,
        limit_cents: u32,
    ) -> Self {
        Self {
            mode,
            secrets,
            decider,
            events,
            rx,
            key: None,
            rejected: false,
            paused: None,
            spend_paused: false,
            slot: None,
            queue: VecDeque::new(),
            retry_at: None,
            in_flight: None,
            cache: DecisionCache::new(CACHE_SIZE),
            checks: 0,
            cost: 0.0,
            cost_unknown: 0,
            limit_cents,
            resumes: 0,
            global_epoch: 0,
            epochs: HashMap::new(),
            shutdown: false,
        }
    }

    fn emit(&self, ev: AiEvent) {
        let _ = self.events.send_blocking(ev);
    }

    fn demo(&self) -> bool {
        self.mode == Mode::Demo
    }

    fn run(mut self) {
        if self.demo() {
            self.emit(AiEvent::Key(KeyState::Demo));
        }
        loop {
            match self.rx.recv_timeout(self.wait()) {
                Ok(cmd) => self.handle(cmd),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            if !self.drain() || self.shutdown {
                return;
            }
            self.run_one();
            if self.shutdown {
                return;
            }
        }
    }

    /// Handles every queued command; false when the UI has gone.
    fn drain(&mut self) -> bool {
        loop {
            match self.rx.try_recv() {
                Ok(cmd) => self.handle(cmd),
                Err(mpsc::TryRecvError::Empty) => return true,
                Err(mpsc::TryRecvError::Disconnected) => return false,
            }
        }
    }

    fn has_work(&self) -> bool {
        self.slot.is_some() || !self.queue.is_empty()
    }

    fn can_send(&self) -> bool {
        self.paused.is_none() && !self.rejected && (self.demo() || self.key.is_some())
    }

    fn wait(&self) -> Duration {
        if !self.has_work() || !self.can_send() {
            return IDLE;
        }
        match self.retry_at {
            Some(at) => at.saturating_duration_since(Instant::now()).min(IDLE),
            None => Duration::ZERO,
        }
    }

    fn epoch(&self, account: &str) -> (u64, u64) {
        (
            self.global_epoch,
            self.epochs.get(account).copied().unwrap_or(0),
        )
    }

    fn dropped(&self, job: &Job) {
        self.emit(AiEvent::Dropped {
            thread: job.req.thread.clone(),
            message_id: job.req.message_id.clone(),
        });
    }

    fn handle(&mut self, cmd: AiCommand) {
        match cmd {
            AiCommand::SaveKey(key) => self.save_key(key),
            AiCommand::ClearKey => self.clear_key(),
            AiCommand::Reload => self.load_key(),
            AiCommand::TestKey(key) => self.test_key(key),
            AiCommand::Check(req) => self.accept(*req),
            AiCommand::ForgetAccount(account) => self.forget(&account),
            AiCommand::ResumePaused => self.resume(),
            AiCommand::SpendLimit(cents) => {
                self.limit_cents = cents;
                if self.paused.is_none() && self.spent() >= self.ceiling() {
                    self.spend_pause();
                }
            }
            AiCommand::Shutdown => self.shutdown = true,
        }
    }

    // ----- the key ---------------------------------------------------------------------------

    fn load_key(&mut self) {
        if self.demo() {
            self.emit(AiEvent::Key(KeyState::Demo));
            return;
        }
        let Some(secrets) = &self.secrets else {
            self.emit(AiEvent::Key(KeyState::Missing));
            return;
        };
        let state = match secrets.openrouter_key() {
            Ok(Some(key)) => {
                let hint = key.hint();
                self.key = Some(key);
                if self.rejected {
                    KeyState::Rejected { hint }
                } else {
                    KeyState::Saved { hint }
                }
            }
            Ok(None) => {
                self.key = None;
                KeyState::Missing
            }
            Err(SecretError::Unavailable(_)) => {
                eprintln!("eloquawk: jev: the keyring could not be read");
                KeyState::Locked
            }
            Err(SecretError::Corrupt(_)) => {
                eprintln!("eloquawk: jev: the stored key is unreadable");
                self.key = None;
                KeyState::Missing
            }
        };
        self.emit(AiEvent::Key(state));
    }

    fn save_key(&mut self, key: ApiKey) {
        if self.demo() {
            return;
        }
        let Some(secrets) = &self.secrets else {
            return;
        };
        match secrets.set_openrouter_key(&key) {
            Ok(()) => {
                let hint = key.hint();
                self.key = Some(key);
                self.rejected = false;
                self.clear_pause();
                self.emit(AiEvent::Key(KeyState::Saved { hint }));
            }
            Err(_) => {
                eprintln!("eloquawk: jev: the key could not be saved to the keyring");
                self.emit(AiEvent::Key(KeyState::Locked));
            }
        }
    }

    fn clear_key(&mut self) {
        if self.demo() {
            return;
        }
        self.drop_all();
        self.cache.clear();
        self.global_epoch += 1;
        self.retry_at = None;
        if let Some(secrets) = &self.secrets
            && secrets.clear_openrouter_key().is_err()
        {
            eprintln!("eloquawk: jev: the key could not be removed from the keyring");
        }
        self.key = None;
        self.rejected = false;
        self.emit(AiEvent::Key(KeyState::Missing));
    }

    // ----- pauses ----------------------------------------------------------------------------

    fn drop_all(&mut self) {
        if let Some(job) = self.slot.take() {
            self.dropped(&job);
        }
        for job in std::mem::take(&mut self.queue) {
            self.dropped(&job);
        }
    }

    fn pause(&mut self, reason: String) {
        self.drop_all();
        self.retry_at = None;
        self.paused = Some(reason.clone());
        self.emit(AiEvent::Paused(Some(reason)));
    }

    fn clear_pause(&mut self) {
        if self.paused.take().is_some() {
            if self.spend_paused {
                self.spend_paused = false;
                self.resumes += 1;
            }
            self.emit(AiEvent::Paused(None));
        }
    }

    /// Settings "Try again". A rejected key stays paused until a key is saved or the saved
    /// one passes a test (SPEC 16.14): retrying it as it is would only be refused again.
    fn resume(&mut self) {
        if self.rejected {
            return;
        }
        self.clear_pause();
    }

    fn spent(&self) -> f64 {
        self.cost + f64::from(self.cost_unknown) * UNKNOWN_COST
    }

    fn ceiling(&self) -> f64 {
        f64::from(self.limit_cents) / 100.0 * f64::from(self.resumes + 1)
    }

    fn spend_pause(&mut self) {
        self.spend_paused = true;
        let reason = format!(
            "Session spend limit reached (${:.2})",
            f64::from(self.limit_cents) / 100.0
        );
        self.pause(reason);
    }

    fn record_usage(&mut self, cost: Option<f64>, check: bool) {
        if check {
            self.checks += 1;
        }
        match cost {
            Some(c) if c.is_finite() && c >= 0.0 => self.cost += c,
            _ => self.cost_unknown += 1,
        }
        self.emit(AiEvent::Usage {
            checks: self.checks,
            cost_usd: self.cost,
            cost_unknown: self.cost_unknown,
        });
    }

    // ----- checks ----------------------------------------------------------------------------

    fn accept(&mut self, req: CheckRequest) {
        if !self.can_send() {
            self.emit(AiEvent::Dropped {
                thread: req.thread,
                message_id: req.message_id,
            });
            return;
        }
        let id: CacheKey = (req.account.clone(), req.message_id.clone());
        let fp = req.labels.as_ref().map(|l| l.fingerprint);
        if self.cache.covers(&id, req.verdict, fp)
            && let Some(entry) = self.cache.get(&id)
        {
            self.emit(AiEvent::Checked {
                thread: req.thread,
                message_id: req.message_id,
                outcome: Outcome {
                    verdict: entry.verdict,
                    labels: entry.labels,
                },
            });
            return;
        }
        // Already on the wire or waiting: coalesce.
        if self.in_flight.as_ref() == Some(&id) || self.slot.as_ref().is_some_and(|j| j.id() == id)
        {
            return;
        }
        if let Some(pos) = self.queue.iter().position(|j| j.id() == id) {
            if req.priority == Priority::Interactive
                && let Some(job) = self.queue.remove(pos)
                && let Some(old) = self.slot.replace(job)
            {
                self.dropped(&old);
            }
            return;
        }
        let job = Job {
            epoch: self.epoch(&req.account),
            req,
            attempts: 0,
            shrunk: false,
        };
        self.emit(AiEvent::Checking {
            thread: job.req.thread.clone(),
            message_id: job.req.message_id.clone(),
        });
        match job.req.priority {
            Priority::Interactive => {
                if let Some(old) = self.slot.replace(job) {
                    self.dropped(&old);
                }
            }
            Priority::Background => {
                if self.queue.len() >= QUEUE_CAP
                    && let Some(old) = self.queue.pop_front()
                {
                    self.dropped(&old);
                }
                self.queue.push_back(job);
            }
        }
    }

    fn forget(&mut self, account: &str) {
        if self.slot.as_ref().is_some_and(|j| j.req.account == account)
            && let Some(job) = self.slot.take()
        {
            self.dropped(&job);
        }
        let (gone, kept): (Vec<Job>, Vec<Job>) = std::mem::take(&mut self.queue)
            .into_iter()
            .partition(|j| j.req.account == account);
        self.queue = kept.into();
        for job in gone {
            self.dropped(&job);
        }
        self.cache.forget_account(account);
        *self.epochs.entry(account.to_string()).or_default() += 1;
    }

    /// Runs the next runnable check: the slot first, then the background queue.
    fn run_one(&mut self) {
        if !self.can_send() || self.retry_at.is_some_and(|t| t > Instant::now()) {
            return;
        }
        self.retry_at = None;
        if !self.has_work() {
            return;
        }
        if self.spent() >= self.ceiling() {
            self.spend_pause();
            return;
        }
        let (job, interactive) = match self.slot.take() {
            Some(job) => (job, true),
            None => match self.queue.pop_front() {
                Some(job) => (job, false),
                None => return,
            },
        };
        let input = fit(job.req.input.clone());
        let input = if job.shrunk { shrink(input) } else { input };
        let req = triage_request(&input);
        self.in_flight = Some(job.id());
        let result = self
            .decider
            .decide(self.key.as_ref(), &req)
            .and_then(|resp| validate(&resp, &TRIAGE_IDS));
        // Revocations and newer opens sent meanwhile are handled before the result is used.
        let alive = self.drain();
        self.in_flight = None;
        if !alive {
            self.shutdown = true;
            return;
        }
        if let Ok(answers) = &result {
            self.record_usage(answers.cost, true);
        }
        if job.epoch != self.epoch(&job.req.account) || self.shutdown {
            self.dropped(&job);
            return;
        }
        match result {
            Ok(answers) => {
                let p = |id: &str| answers.probs.get(id).copied().unwrap_or(0.0);
                let verdict = verdict_from(p("phishing"), p("scam"), p("spam"), &THRESHOLDS);
                self.cache.insert(
                    job.id(),
                    CacheEntry {
                        verdict: Some(verdict),
                        labels: None,
                    },
                );
                self.emit(AiEvent::Checked {
                    thread: job.req.thread.clone(),
                    message_id: job.req.message_id.clone(),
                    outcome: Outcome {
                        verdict: Some(verdict),
                        labels: None,
                    },
                });
                if self.paused.is_none() && self.spent() >= self.ceiling() {
                    self.spend_pause();
                }
            }
            Err(e) => self.failed(job, interactive, e),
        }
    }

    /// Puts a job back for another attempt, unless a newer open took the slot.
    fn requeue(&mut self, job: Job, interactive: bool) {
        if interactive {
            if self.slot.is_none() {
                self.slot = Some(job);
            } else {
                self.dropped(&job);
            }
        } else {
            self.queue.push_front(job);
        }
    }

    fn failed(&mut self, mut job: Job, interactive: bool, e: AiError) {
        let fail = |w: &Self, job: &Job, retryable: bool| {
            w.emit(AiEvent::Failed {
                thread: job.req.thread.clone(),
                message_id: job.req.message_id.clone(),
                error: e.user_text(),
                retryable,
            })
        };
        if e == AiError::TooLarge {
            if job.shrunk {
                fail(self, &job, false);
            } else {
                job.shrunk = true;
                self.requeue(job, interactive);
            }
            return;
        }
        if e.pauses() {
            self.dropped(&job);
            if e == AiError::KeyRejected {
                self.rejected = true;
                let hint = self.key.as_ref().map(ApiKey::hint).unwrap_or_default();
                self.emit(AiEvent::Key(KeyState::Rejected { hint }));
            }
            self.pause(e.user_text());
            return;
        }
        if e.retryable() {
            job.attempts += 1;
            if job.attempts < MAX_ATTEMPTS {
                let retry_after = match &e {
                    AiError::Busy { retry_after } => *retry_after,
                    _ => None,
                };
                let delay = Backoff::delay(job.attempts - 1, retry_after, Backoff::jitter());
                let now = Instant::now();
                self.retry_at = Some(now.checked_add(delay).unwrap_or(now + BACKOFF_CAP));
                self.requeue(job, interactive);
            } else if interactive {
                fail(self, &job, true);
            } else {
                self.dropped(&job);
            }
            return;
        }
        fail(self, &job, false);
    }

    // ----- the key test ----------------------------------------------------------------------

    fn test_key(&mut self, typed: Option<ApiKey>) {
        if self.demo() {
            self.emit(AiEvent::TestResult(Ok(
                "Demo mode: the checker is offline".into()
            )));
            return;
        }
        let saved = typed.is_none();
        let Some(key) = typed.as_ref().or(self.key.as_ref()) else {
            self.emit(AiEvent::TestResult(Err("No key saved".into())));
            return;
        };
        let result = self.decider.credit(key).and_then(|credit| {
            let mut questions = std::collections::BTreeMap::new();
            questions.insert(
                "test".to_string(),
                Question::noul("Is this text a connection test?", "It is.", "It is not."),
            );
            let req = DecisionsRequest::new(json!("Eloquawk connection test."), questions);
            let decided = Instant::now();
            let answers = self
                .decider
                .decide(Some(key), &req)
                .and_then(|r| validate(&r, &["test"]))?;
            Ok((credit, decided.elapsed(), answers.cost))
        });
        match result {
            Ok((credit, took, cost)) => {
                self.record_usage(cost, false);
                let mut text = format!("Key works · Jev answered in {:.1} s", took.as_secs_f64());
                if let Some(left) = credit {
                    text.push_str(&format!(" · ${left:.2} left on this key"));
                }
                if saved {
                    if self.rejected {
                        self.rejected = false;
                        if let Some(k) = &self.key {
                            let hint = k.hint();
                            self.emit(AiEvent::Key(KeyState::Saved { hint }));
                        }
                    }
                    self.clear_pause();
                } else {
                    text.push_str(" · Save it to use it");
                }
                self.emit(AiEvent::TestResult(Ok(text)));
            }
            Err(e) => self.emit(AiEvent::TestResult(Err(e.user_text()))),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::ai::prompt::{AiInput, Redact};
    use crate::google::fake::{FakeServer, Reply};
    use crate::model::{Address, BodySource, Message, MessageSignals, ThreadKey};
    use crate::secrets::{MemoryStore, SecretStore};

    const KEY: &str = "sk-or-test-0123456789abcdef";
    const DECISIONS: &str = "/api/alpha/decisions";
    const KEY_PATH: &str = "/api/v1/key";
    const T: Duration = Duration::from_secs(10);

    fn api_key() -> ApiKey {
        ApiKey::parse(KEY).unwrap()
    }

    fn message(subject: &str) -> Message {
        Message {
            id: format!("m-{subject}"),
            thread_id: subject.into(),
            label_ids: BTreeSet::new(),
            from: Address::new(Some("Sender"), &format!("{subject}@sender.example")),
            to: vec![],
            cc: vec![],
            bcc: vec![],
            reply_to: vec![],
            date: jiff::Timestamp::UNIX_EPOCH,
            subject: subject.into(),
            message_id: None,
            in_reply_to: None,
            references: vec![],
            snippet: String::new(),
            body: format!("Body of {subject}."),
            body_source: BodySource::Plain,
            body_truncated: false,
            attachments: vec![],
            signals: MessageSignals::default(),
        }
    }

    fn request(account: &str, subject: &str, priority: Priority) -> CheckRequest {
        let m = message(subject);
        CheckRequest {
            account: account.into(),
            thread: ThreadKey::new(account, subject),
            message_id: m.id.clone(),
            priority,
            verdict: true,
            labels: None,
            input: AiInput::from_message(&m, &Redact::default()),
        }
    }

    fn check(account: &str, subject: &str, priority: Priority) -> AiCommand {
        AiCommand::Check(Box::new(request(account, subject, priority)))
    }

    fn ok_body(p_phishing: f64, cost: Option<f64>) -> String {
        let usage = match cost {
            Some(c) => format!(r#","usage":{{"input_tokens":400,"output_tokens":40,"cost":{c}}}"#),
            None => r#","usage":{"input_tokens":400,"output_tokens":40}"#.to_string(),
        };
        format!(
            r#"{{"model":"typesafe/jev-1.13-20260917","answers":{{"phishing":{{"type":"noul","noul":{p_phishing}}},"scam":{{"type":"noul","noul":0.02}},"spam":{{"type":"noul","noul":0.05}},"test":{{"type":"noul","noul":0.99}}}}{usage},"id":"gen-dec-1","provider":"TypeSafe"}}"#
        )
    }

    fn ok(p: f64) -> Reply {
        Reply::json(200, &ok_body(p, Some(0.00002))).for_path(DECISIONS)
    }

    /// Keeps a server listening after its script is used up: a reply nothing asks for.
    fn keep_open() -> Reply {
        Reply::json(200, "{}").for_path("/never")
    }

    fn endpoints(server: &FakeServer) -> AiEndpoints {
        AiEndpoints::new(server.url(DECISIONS), server.url(KEY_PATH))
    }

    fn prefs() -> Prefs {
        Prefs {
            ephemeral: true,
            ..Prefs::default()
        }
    }

    /// A Google-mode worker with the test key saved and loaded.
    fn google(server: &FakeServer) -> (AiHandle, async_channel::Receiver<AiEvent>) {
        let secrets = Secrets::memory();
        secrets.set_openrouter_key(&api_key()).unwrap();
        let (h, rx) = start_with(Mode::Google, Some(secrets), &prefs(), endpoints(server)).unwrap();
        h.send(AiCommand::Reload);
        wait_for(&rx, |e| matches!(e, AiEvent::Key(KeyState::Saved { .. })));
        (h, rx)
    }

    /// Collects events until one matches `done` (included); panics after [`T`].
    fn wait_for(
        rx: &async_channel::Receiver<AiEvent>,
        done: impl Fn(&AiEvent) -> bool,
    ) -> Vec<AiEvent> {
        let deadline = Instant::now() + T;
        let mut out = Vec::new();
        while Instant::now() < deadline {
            match rx.try_recv() {
                Ok(ev) => {
                    let stop = done(&ev);
                    out.push(ev);
                    if stop {
                        return out;
                    }
                }
                Err(_) => std::thread::sleep(Duration::from_millis(5)),
            }
        }
        panic!("timed out; got {out:?}");
    }

    /// Every event that arrives within `d`.
    fn settle(rx: &async_channel::Receiver<AiEvent>, d: Duration) -> Vec<AiEvent> {
        let deadline = Instant::now() + d;
        let mut out = Vec::new();
        while Instant::now() < deadline {
            match rx.try_recv() {
                Ok(ev) => out.push(ev),
                Err(_) => std::thread::sleep(Duration::from_millis(5)),
            }
        }
        out
    }

    fn wait_requests(server: &FakeServer, n: usize) {
        let deadline = Instant::now() + T;
        while server.requests().len() < n {
            assert!(
                Instant::now() < deadline,
                "only {} requests",
                server.requests().len()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn checked(subject: &str) -> impl Fn(&AiEvent) -> bool + '_ {
        move |e| matches!(e, AiEvent::Checked { thread, .. } if thread.id == subject)
    }

    fn is_dropped(e: &AiEvent, subject: &str) -> bool {
        matches!(e, AiEvent::Dropped { thread, .. } if thread.id == subject)
    }

    fn body_subject(r: &crate::google::fake::Recorded) -> String {
        let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
        v["state"]["subject"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    #[test]
    fn fake_openrouter_check() {
        let server = FakeServer::start(vec![ok(0.97)]);
        let (h, rx) = google(&server);
        let golden: serde_json::Value =
            serde_json::from_str(include_str!("fixtures/request_phishing.json")).unwrap();
        let m = crate::demo_data::generate("2026-10-29T12:00:00Z".parse().unwrap())
            .threads
            .into_iter()
            .find(|t| t.0.key.id == "p-ai-phish")
            .unwrap()
            .1
            .messages
            .remove(0);
        let own = vec![
            crate::demo_data::PERSONAL.to_string(),
            crate::demo_data::WORK.to_string(),
        ];
        let req = || CheckRequest {
            account: own[0].clone(),
            thread: ThreadKey::new(own[0].as_str(), "p-ai-phish"),
            message_id: m.id.clone(),
            priority: Priority::Interactive,
            verdict: true,
            labels: None,
            input: AiInput::from_message(&m, &Redact::for_message(&m, &own)),
        };
        h.send(AiCommand::Check(Box::new(req())));
        let evs = wait_for(&rx, |e| matches!(e, AiEvent::Checked { .. }));
        assert!(matches!(evs[0], AiEvent::Checking { .. }), "{evs:?}");
        let Some(AiEvent::Checked { outcome, .. }) = evs.last() else {
            unreachable!()
        };
        let v = outcome.verdict.unwrap();
        assert_eq!(v.level, crate::ai::jev::Level::Danger);
        // Reopening the same message is answered from the cache, with no Checking.
        h.send(AiCommand::Check(Box::new(req())));
        let again = wait_for(&rx, |e| matches!(e, AiEvent::Checked { .. }));
        assert!(!again.iter().any(|e| matches!(e, AiEvent::Checking { .. })));
        let reqs = server.finish();
        assert_eq!(reqs.len(), 1);
        let r = &reqs[0];
        assert_eq!(r.method, "POST");
        assert_eq!(r.path(), DECISIONS);
        assert_eq!(
            r.header("authorization").as_deref(),
            Some(&*format!("Bearer {KEY}"))
        );
        assert_eq!(
            r.header("content-type").as_deref(),
            Some("application/json")
        );
        assert_eq!(
            r.header("http-referer").as_deref(),
            Some("https://github.com/korora-tech/eloquawk")
        );
        assert_eq!(r.header("x-openrouter-title").as_deref(), Some("Eloquawk"));
        let body: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(body, golden);
        h.send(AiCommand::Shutdown);
    }

    #[test]
    fn fake_openrouter_retry() {
        let busy = |s: u16| {
            Reply::json(s, r#"{"error":{"code":503,"message":"busy"}}"#)
                .with_header("Retry-After", "0")
                .for_path(DECISIONS)
        };
        let server = FakeServer::start(vec![
            Reply::json(429, r#"{"error":{"code":429,"message":"slow"}}"#)
                .with_header("Retry-After", "1")
                .for_path(DECISIONS),
            ok(0.1),
            Reply::json(
                402,
                r#"{"error":{"code":402,"message":"m","metadata":{"limit_source":"openrouter_in_flight_budget"}}}"#,
            )
            .with_header("Retry-After", "0")
            .for_path(DECISIONS),
            ok(0.1),
            busy(503),
            busy(503),
            busy(503),
        ]);
        let (h, rx) = google(&server);
        let started = Instant::now();
        h.send(check("a@x", "first", Priority::Interactive));
        let evs = wait_for(&rx, checked("first"));
        assert!(
            started.elapsed() >= Duration::from_millis(900),
            "Retry-After was honoured"
        );
        assert_eq!(
            evs.iter()
                .filter(|e| matches!(e, AiEvent::Checked { .. }))
                .count(),
            1
        );
        assert_eq!(server.requests().len(), 2);
        h.send(check("a@x", "second", Priority::Interactive));
        wait_for(&rx, checked("second"));
        assert_eq!(server.requests().len(), 4);
        h.send(check("a@x", "third", Priority::Interactive));
        let evs = wait_for(&rx, |e| matches!(e, AiEvent::Failed { .. }));
        assert!(matches!(
            evs.last(),
            Some(AiEvent::Failed {
                retryable: true,
                ..
            })
        ));
        assert_eq!(server.finish().len(), 7);
        h.send(AiCommand::Shutdown);
    }

    #[test]
    fn fake_openrouter_replaced_during_retry() {
        let server = FakeServer::start(vec![
            Reply::json(429, "{}")
                .with_header("Retry-After", "1")
                .for_path(DECISIONS),
            ok(0.1),
            ok(0.1),
        ]);
        let (h, rx) = google(&server);
        h.send(check("a@x", "A", Priority::Interactive));
        wait_requests(&server, 1);
        std::thread::sleep(Duration::from_millis(100));
        h.send(check("a@x", "B", Priority::Interactive));
        let evs = wait_for(&rx, checked("B"));
        assert!(evs.iter().any(|e| is_dropped(e, "A")), "{evs:?}");
        h.send(check("a@x", "A", Priority::Interactive));
        let evs = wait_for(&rx, checked("A"));
        assert!(matches!(evs[0], AiEvent::Checking { .. }), "{evs:?}");
        let reqs = server.finish();
        let subjects: Vec<String> = reqs.iter().map(body_subject).collect();
        assert_eq!(subjects, ["A", "B", "A"]);
        h.send(AiCommand::Shutdown);
    }

    #[test]
    fn fake_openrouter_opt_out_cancels_queue() {
        let held = || {
            Reply::json(429, "{}")
                .with_header("Retry-After", "1")
                .for_path(DECISIONS)
        };
        let server = FakeServer::start(vec![
            held(),
            Reply::json(200, &ok_body(0.1, Some(0.0)))
                .for_path(DECISIONS)
                .sticky(),
            keep_open(),
        ]);
        let (h, rx) = google(&server);
        for s in ["A1", "A2"] {
            h.send(check("a@x", s, Priority::Background));
        }
        for s in ["B1", "B2"] {
            h.send(check("b@x", s, Priority::Background));
        }
        wait_requests(&server, 1);
        h.send(AiCommand::ForgetAccount("a@x".into()));
        let evs = wait_for(&rx, checked("B2"));
        assert!(evs.iter().any(|e| is_dropped(e, "A1")));
        assert!(evs.iter().any(|e| is_dropped(e, "A2")));
        assert!(
            evs.iter()
                .any(|e| matches!(e, AiEvent::Checked { thread, .. } if thread.id == "B1"))
        );
        let subjects: Vec<String> = server.requests().iter().map(body_subject).collect();
        assert_eq!(subjects, ["A1", "B1", "B2"]);
        h.send(AiCommand::Shutdown);

        // ClearKey stops everything.
        let server = FakeServer::start(vec![
            held(),
            Reply::json(200, &ok_body(0.1, Some(0.0)))
                .for_path(DECISIONS)
                .sticky(),
            keep_open(),
        ]);
        let (h, rx) = google(&server);
        for s in ["A1", "A2", "B1"] {
            h.send(check(
                if s.starts_with('A') { "a@x" } else { "b@x" },
                s,
                Priority::Background,
            ));
        }
        wait_requests(&server, 1);
        h.send(AiCommand::ClearKey);
        let evs = wait_for(&rx, |e| matches!(e, AiEvent::Key(KeyState::Missing)));
        for s in ["A1", "A2", "B1"] {
            assert!(evs.iter().any(|e| is_dropped(e, s)), "{s}: {evs:?}");
        }
        settle(&rx, Duration::from_millis(1500));
        assert_eq!(server.requests().len(), 1);
        h.send(AiCommand::Shutdown);
    }

    #[test]
    fn fake_openrouter_flagged_no_pause() {
        let server = FakeServer::start(vec![
            Reply::json(
                403,
                r#"{"error":{"code":403,"message":"flagged","metadata":{"reasons":["x"],"flagged_input":"y"}}}"#,
            )
            .for_path(DECISIONS),
            ok(0.1),
        ]);
        let (h, rx) = google(&server);
        h.send(check("a@x", "bad", Priority::Interactive));
        let evs = wait_for(&rx, |e| matches!(e, AiEvent::Failed { .. }));
        assert!(matches!(
            evs.last(),
            Some(AiEvent::Failed {
                retryable: false,
                ..
            })
        ));
        h.send(check("a@x", "next", Priority::Interactive));
        let evs = wait_for(&rx, checked("next"));
        assert!(!evs.iter().any(|e| matches!(e, AiEvent::Paused(Some(_)))));
        assert_eq!(server.finish().len(), 2);
        h.send(AiCommand::Shutdown);
    }

    #[test]
    fn fake_openrouter_no_redirect() {
        let elsewhere = FakeServer::start(vec![ok(0.1), Reply::json(200, "{}")]);
        let server = FakeServer::start(vec![
            Reply::json(302, "")
                .with_header("Location", &elsewhere.url(DECISIONS))
                .for_path(DECISIONS),
        ]);
        let (h, rx) = google(&server);
        h.send(check("a@x", "redirected", Priority::Interactive));
        let evs = wait_for(&rx, |e| matches!(e, AiEvent::Paused(Some(_))));
        assert!(evs.iter().any(|e| is_dropped(e, "redirected")));
        let Some(AiEvent::Paused(Some(reason))) = evs.last() else {
            unreachable!()
        };
        assert_eq!(reason, &AiError::Unavailable.user_text());
        assert_eq!(server.finish().len(), 1);
        std::thread::sleep(Duration::from_millis(200));
        assert!(elsewhere.requests().is_empty());
        h.send(AiCommand::Shutdown);
    }

    #[test]
    fn fake_openrouter_spend_limit() {
        let server = FakeServer::start(vec![
            Reply::json(200, &ok_body(0.1, Some(0.6)))
                .for_path(DECISIONS)
                .sticky(),
            keep_open(),
        ]);
        let (h, rx) = google(&server);
        h.send(check("a@x", "c1", Priority::Interactive));
        wait_for(&rx, checked("c1"));
        h.send(check("a@x", "c2", Priority::Interactive));
        let evs = wait_for(&rx, |e| matches!(e, AiEvent::Paused(Some(_))));
        assert!(evs.iter().any(checked("c2")));
        let Some(AiEvent::Paused(Some(reason))) = evs.last() else {
            unreachable!()
        };
        assert_eq!(reason, "Session spend limit reached ($1.00)");
        h.send(check("a@x", "c3", Priority::Interactive));
        wait_for(&rx, |e| is_dropped(e, "c3"));
        h.send(AiCommand::ResumePaused);
        for s in ["c3", "c4"] {
            h.send(check("a@x", s, Priority::Interactive));
            wait_for(&rx, checked(s));
        }
        wait_for(&rx, |e| matches!(e, AiEvent::Paused(Some(_))));
        assert_eq!(server.requests().len(), 4);
        h.send(AiCommand::SpendLimit(500));
        h.send(AiCommand::ResumePaused);
        h.send(check("a@x", "c5", Priority::Interactive));
        let evs = wait_for(&rx, checked("c5"));
        assert!(!evs.iter().any(|e| matches!(e, AiEvent::Paused(Some(_)))));
        assert_eq!(server.requests().len(), 5);
        h.send(AiCommand::Shutdown);
    }

    #[test]
    fn fake_openrouter_pauses() {
        // A 401 rejects the key and stops every request.
        let server = FakeServer::start(vec![
            Reply::json(401, include_str!("fixtures/error_401.json")).for_path(DECISIONS),
            keep_open(),
        ]);
        let (h, rx) = google(&server);
        h.send(check("a@x", "one", Priority::Interactive));
        let evs = wait_for(&rx, |e| matches!(e, AiEvent::Paused(Some(_))));
        assert!(
            evs.iter()
                .any(|e| matches!(e, AiEvent::Key(KeyState::Rejected { .. })))
        );
        for s in ["two", "three"] {
            h.send(check("a@x", s, Priority::Interactive));
            wait_for(&rx, |e| is_dropped(e, s));
        }
        // Try again does not lift a rejection: only saving a key (or a passing test of the
        // saved one) does, so the rejected key is never sent again.
        h.send(AiCommand::ResumePaused);
        h.send(check("a@x", "four", Priority::Interactive));
        let evs = wait_for(&rx, |e| is_dropped(e, "four"));
        assert!(!evs.iter().any(|e| matches!(e, AiEvent::Paused(None))));
        assert_eq!(server.requests().len(), 1);
        h.send(AiCommand::Shutdown);

        // A 402 pauses; only Try again or a test of the saved key resumes.
        let server = FakeServer::start(vec![
            Reply::json(
                402,
                r#"{"error":{"code":402,"message":"Insufficient credits"}}"#,
            )
            .for_path(DECISIONS),
            ok(0.1),
            Reply::json(
                402,
                r#"{"error":{"code":402,"message":"Insufficient credits"}}"#,
            )
            .for_path(DECISIONS),
            Reply::json(200, include_str!("fixtures/key_limited.json")).for_path(KEY_PATH),
            ok(0.1),
            Reply::json(200, include_str!("fixtures/key_limited.json")).for_path(KEY_PATH),
            ok(0.1),
            ok(0.1),
        ]);
        let (h, rx) = google(&server);
        h.send(check("a@x", "p1", Priority::Interactive));
        let evs = wait_for(&rx, |e| matches!(e, AiEvent::Paused(Some(_))));
        let Some(AiEvent::Paused(Some(reason))) = evs.last() else {
            unreachable!()
        };
        assert_eq!(reason, "Your OpenRouter credit has run out");
        h.send(AiCommand::ResumePaused);
        wait_for(&rx, |e| matches!(e, AiEvent::Paused(None)));
        h.send(check("a@x", "p2", Priority::Interactive));
        wait_for(&rx, checked("p2"));
        h.send(check("a@x", "p3", Priority::Interactive));
        wait_for(&rx, |e| matches!(e, AiEvent::Paused(Some(_))));
        // A typed key that works does not resume.
        h.send(AiCommand::TestKey(Some(
            ApiKey::parse("sk-or-other-0123456789abcdef").unwrap(),
        )));
        let evs = wait_for(&rx, |e| matches!(e, AiEvent::TestResult(_)));
        let Some(AiEvent::TestResult(Ok(text))) = evs.last() else {
            panic!("{evs:?}")
        };
        assert!(text.ends_with(" · Save it to use it"), "{text}");
        assert!(!evs.iter().any(|e| matches!(e, AiEvent::Paused(None))));
        // The saved key does.
        h.send(AiCommand::TestKey(None));
        let evs = wait_for(&rx, |e| matches!(e, AiEvent::TestResult(_)));
        assert!(
            evs.iter().any(|e| matches!(e, AiEvent::Paused(None))),
            "{evs:?}"
        );
        h.send(check("a@x", "p4", Priority::Interactive));
        wait_for(&rx, checked("p4"));
        h.send(AiCommand::Shutdown);
    }

    #[test]
    fn fake_openrouter_slot() {
        let server = FakeServer::start(vec![
            ok(0.1).delayed(Duration::from_millis(600)),
            ok(0.1),
            keep_open(),
        ]);
        let (h, rx) = google(&server);
        h.send(check("a@x", "s1", Priority::Interactive));
        wait_requests(&server, 1);
        for s in ["s2", "s3", "s4", "s5"] {
            h.send(check("a@x", s, Priority::Interactive));
        }
        let evs = wait_for(&rx, checked("s5"));
        assert!(evs.iter().any(checked("s1")));
        for s in ["s2", "s3", "s4"] {
            assert!(evs.iter().any(|e| is_dropped(e, s)), "{s}");
        }
        settle(&rx, Duration::from_millis(300));
        let subjects: Vec<String> = server.requests().iter().map(body_subject).collect();
        assert_eq!(subjects, ["s1", "s5"]);
        h.send(AiCommand::Shutdown);
    }

    #[test]
    fn fake_openrouter_test_key() {
        for (fixture, credit) in [
            (
                include_str!("fixtures/key_limited.json"),
                Some("$3.12 left on this key"),
            ),
            (include_str!("fixtures/key_unlimited.json"), None),
        ] {
            let server =
                FakeServer::start(vec![Reply::json(200, fixture).for_path(KEY_PATH), ok(0.99)]);
            let (h, rx) = google(&server);
            h.send(AiCommand::TestKey(None));
            let evs = wait_for(&rx, |e| matches!(e, AiEvent::TestResult(_)));
            let Some(AiEvent::TestResult(Ok(text))) = evs.last() else {
                panic!("{evs:?}")
            };
            assert!(text.starts_with("Key works · Jev answered in "), "{text}");
            assert_eq!(
                text.ends_with("left on this key"),
                credit.is_some(),
                "{text}"
            );
            if let Some(c) = credit {
                assert!(text.contains(c), "{text}");
            }
            let reqs = server.finish();
            assert_eq!(
                reqs.iter()
                    .map(|r| r.path().to_string())
                    .collect::<Vec<_>>(),
                [KEY_PATH, DECISIONS]
            );
            assert_eq!(reqs[0].method, "GET");
            let body: serde_json::Value = serde_json::from_slice(&reqs[1].body).unwrap();
            assert_eq!(body["state"], "Eloquawk connection test.");
            assert_eq!(
                body["questions"]["test"]["criteria"],
                serde_json::json!({"true": "It is.", "false": "It is not."})
            );
            h.send(AiCommand::Shutdown);
        }
    }

    #[test]
    fn demo_ai_makes_no_network() {
        let server = FakeServer::start(vec![
            ok(0.9),
            Reply::json(200, include_str!("fixtures/key_limited.json")).for_path(KEY_PATH),
            Reply::json(200, "{}"),
        ]);
        let (h, rx) = start_with(Mode::Demo, None, &prefs(), endpoints(&server)).unwrap();
        wait_for(&rx, |e| matches!(e, AiEvent::Key(KeyState::Demo)));
        h.send(AiCommand::Reload);
        h.send(check("a@x", "demo", Priority::Interactive));
        wait_for(&rx, checked("demo"));
        h.send(AiCommand::TestKey(None));
        let evs = wait_for(&rx, |e| matches!(e, AiEvent::TestResult(_)));
        assert!(matches!(
            evs.last(),
            Some(AiEvent::TestResult(Ok(t))) if t == "Demo mode: the checker is offline"
        ));
        h.send(AiCommand::SaveKey(api_key()));
        h.send(AiCommand::ClearKey);
        settle(&rx, Duration::from_millis(200));
        assert!(server.requests().is_empty());
        h.send(AiCommand::Shutdown);
    }

    /// Counts keyring reads.
    struct Counting(MemoryStore, Arc<AtomicUsize>);

    impl SecretStore for Counting {
        fn get(&self, user: &str) -> Result<Option<String>, SecretError> {
            self.1.fetch_add(1, Ordering::SeqCst);
            self.0.get(user)
        }
        fn set(&self, user: &str, value: &str) -> Result<(), SecretError> {
            self.0.set(user, value)
        }
        fn delete(&self, user: &str) -> Result<(), SecretError> {
            self.0.delete(user)
        }
    }

    #[test]
    fn ai_waits_for_first_reload() {
        let reads = Arc::new(AtomicUsize::new(0));
        let secrets = Secrets::new(Arc::new(Counting(MemoryStore::default(), reads.clone())));
        let (h, rx) = start_with(
            Mode::Google,
            Some(secrets),
            &prefs(),
            AiEndpoints::new("http://127.0.0.1:9/d".into(), "http://127.0.0.1:9/k".into()),
        )
        .unwrap();
        h.send(check("a@x", "early", Priority::Interactive));
        let evs = settle(&rx, Duration::from_millis(300));
        assert_eq!(reads.load(Ordering::SeqCst), 0, "{evs:?}");
        assert!(
            evs.iter().any(|e| is_dropped(e, "early")),
            "no key yet: {evs:?}"
        );
        h.send(AiCommand::Reload);
        wait_for(&rx, |e| matches!(e, AiEvent::Key(KeyState::Missing)));
        assert_eq!(reads.load(Ordering::SeqCst), 1);
        h.send(AiCommand::Shutdown);
    }

    /// Answers by subject: `busy…` is always busy, `credits…` runs out of credit, anything
    /// else succeeds after a short delay.
    struct Script;

    impl Decider for Script {
        fn decide(
            &mut self,
            _: Option<&ApiKey>,
            req: &DecisionsRequest,
        ) -> Result<DecisionsResponse, AiError> {
            let subject = req.state["subject"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            std::thread::sleep(Duration::from_millis(15));
            if subject.starts_with("busy") {
                return Err(AiError::Busy {
                    retry_after: Some(Duration::ZERO),
                });
            }
            if subject.starts_with("credits") {
                return Err(AiError::Credits);
            }
            Ok(serde_json::from_str(&ok_body(0.1, Some(0.0))).unwrap())
        }

        fn credit(&mut self, _: &ApiKey) -> Result<Option<f64>, AiError> {
            Ok(None)
        }
    }

    #[test]
    fn fake_openrouter_event_invariant() {
        let secrets = Secrets::memory();
        secrets.set_openrouter_key(&api_key()).unwrap();
        let (h, rx) = spawn(Mode::Google, Some(secrets), Box::new(Script), 100).unwrap();
        h.send(AiCommand::Reload);
        // Slot replacement.
        for i in 0..6 {
            h.send(check("a@x", &format!("open{i}"), Priority::Interactive));
        }
        // FIFO overflow, background exhaustion and an account forgotten mid-queue.
        for i in 0..55 {
            let account = if i % 2 == 0 { "a@x" } else { "b@x" };
            h.send(check(account, &format!("bg{i}"), Priority::Background));
        }
        h.send(check("a@x", "busy-bg", Priority::Background));
        h.send(check("a@x", "busy-open", Priority::Interactive));
        h.send(AiCommand::ForgetAccount("b@x".into()));
        let mut evs = settle(&rx, Duration::from_millis(1500));
        // A pause drops what is queued; then ClearKey drops what came after.
        h.send(AiCommand::ResumePaused);
        h.send(check("a@x", "credits", Priority::Interactive));
        for i in 0..5 {
            h.send(check("a@x", &format!("after{i}"), Priority::Background));
        }
        evs.extend(settle(&rx, Duration::from_millis(500)));
        h.send(AiCommand::ResumePaused);
        for i in 0..5 {
            h.send(check("a@x", &format!("late{i}"), Priority::Background));
        }
        h.send(AiCommand::ClearKey);
        evs.extend(settle(&rx, Duration::from_millis(800)));
        h.send(AiCommand::Shutdown);

        let mut open: BTreeMap<(String, String), u32> = BTreeMap::new();
        let (mut checking, mut terminal) = (0, 0);
        for e in &evs {
            match e {
                AiEvent::Checking { thread, message_id } => {
                    checking += 1;
                    let n = open
                        .entry((thread.id.clone(), message_id.clone()))
                        .or_default();
                    assert_eq!(*n, 0, "two Checking for {} without an end", thread.id);
                    *n = 1;
                }
                AiEvent::Checked {
                    thread, message_id, ..
                }
                | AiEvent::Failed {
                    thread, message_id, ..
                }
                | AiEvent::Dropped { thread, message_id } => {
                    if let Some(n) = open.get_mut(&(thread.id.clone(), message_id.clone()))
                        && *n == 1
                    {
                        *n = 0;
                        terminal += 1;
                    }
                }
                _ => {}
            }
        }
        let unfinished: Vec<_> = open.iter().filter(|(_, n)| **n > 0).collect();
        assert!(unfinished.is_empty(), "no end for {unfinished:?}");
        assert_eq!(checking, terminal);
        assert!(checking > 60, "{checking}");
        assert!(evs.iter().any(|e| matches!(e, AiEvent::Failed { thread, retryable: true, .. } if thread.id == "busy-open")));
        assert!(evs.iter().any(|e| is_dropped(e, "busy-bg")));
        assert!(evs.iter().any(|e| matches!(e, AiEvent::Paused(Some(_)))));
        assert!(
            evs.iter()
                .any(|e| matches!(e, AiEvent::Key(KeyState::Missing)))
        );
    }
}
