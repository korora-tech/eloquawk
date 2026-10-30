//! The Google worker (SPEC 7): one thread, `eloquawk-sync`, that owns the credentials, talks
//! to Google and pushes [`SyncEvent`]s to the UI. The UI never blocks on the network or the
//! keyring; it sends [`Command`]s and renders whatever arrives.
//!
//! One OAuth client serves any number of Google accounts. Each account has its own refresh
//! token (in the keyring), access token (in memory only), caches, backoff and problems; a
//! failure in one account does not stop the others.
//!
//! Work runs as [`Job`]s on one queue with two lanes (SPEC 7.2): interactive jobs (open a
//! thread, list a view) go first, background jobs (initial sync, label counts, metadata
//! chunks, the poll's round) after them, coalesced. One job, so one HTTP request at a time,
//! runs between reads of the command channel, so a click waits for at most one chunk. An
//! account that hit a rate limit or a network failure parks its background jobs until its
//! backoff passes; they are never dropped.
//!
//! The read path (M10) is the initial sync, listing and paging views, metadata chunks with
//! skeleton rows, label counts and opening a thread. M11 adds the incremental sync (SPEC
//! 7.4): a poll round reads Gmail's history since the stored cursor, commits the cursor
//! together with the thread ids it implies (so a failure can never lose a change), refetches
//! those threads in chunks, re-lists page 1 of the inbox and the shown view, and reports new
//! inbox mail. M11 also changes mail (per-thread modify, star on the last message, trash and
//! untrash with label restore) and sends it with a multipart upload that is never retried.
//! M12 adds attachments: a save reads the message afresh for a current attachment id,
//! downloads the part (or takes its inline data) and writes it under a sanitised name to a
//! new file in the download folder; a forward resolves the original's attachments the same
//! way before it builds the message.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use jiff::Timestamp;

use crate::config::Prefs;
use crate::google::api::{self, ApiError, Endpoints};
use crate::google::gmail;
use crate::google::oauth::{self, AuthError, ClientCredentials, TokenResponse};
use crate::mail::{b64, compose, files, parse};
use crate::model::{
    AccountId, Attachment, AttachmentSource, Label, LabelDelta, LabelKind,
    MAX_OUTGOING_ATTACHMENTS, Outgoing, OutgoingAttachment, ThreadKey, ThreadSummary, ViewKey,
    ViewKind, labels,
};
use crate::protocol::{AuthState, Command, OpId, PageMode, SendError, SyncEvent, SyncHandle};
use crate::secrets::{SecretError, Secrets};

/// How long a browser sign-in may take before it is abandoned.
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// An access token this close to expiry is refreshed before use.
const TOKEN_MARGIN: Duration = Duration::from_secs(60);
/// Ids per metadata or label-count job, so an interactive job waits for at most one chunk.
const CHUNK: usize = 10;
/// The longest the loop sleeps without looking at the clock.
const MAX_WAIT: Duration = Duration::from_secs(30);
/// The least a quota problem waits before the next automatic try.
const QUOTA_WAIT: Duration = Duration::from_secs(15 * 60);
/// The system labels the sidebar shows (SPEC 4.1), whose counts are fetched.
const SHOWN_SYSTEM_LABELS: &[&str] = &[
    labels::INBOX,
    labels::STARRED,
    labels::IMPORTANT,
    labels::SENT,
    labels::DRAFT,
    labels::SPAM,
    labels::TRASH,
];

const NETWORK_PROBLEM: &str = "Couldn't reach Google. Check your connection; Ctrl+R tries again.";
const MISSING_SCOPE_SIGN_IN: &str = concat!(
    "Eloquawk needs permission to read and organise your mail. Sign in again and allow ",
    "Gmail access on Google's consent page.",
);
const NO_REFRESH_TOKEN: &str = concat!(
    "Google did not return a refresh token. Remove Eloquawk's access at ",
    "myaccount.google.com/permissions and sign in again.",
);
const SIGNED_OUT: &str = "This account is not signed in.";
const MAYBE_SENT: &str = "Couldn't confirm the message was sent. Check Sent before trying again.";
const ATTACHMENT_TOO_LARGE: &str = "Attachment too large to download";
const ATTACHMENT_GONE: &str = "The attachment is no longer in Gmail.";
const ATTACHMENT_UNREADABLE: &str = "Gmail sent the attachment's data in a form it couldn't read.";
/// More changed threads than this in one round is a burst: only new inbox mail is refetched,
/// the rest is dropped from the cache and refetched when a listing shows it (SPEC 7.4).
const BURST: usize = 100;
/// The most new inbox threads a burst still refetches.
const BURST_FRESH: usize = 20;
/// History pages read in one round before the cursor is treated as too old to follow.
const MAX_HISTORY_PAGES: usize = 100;
/// Labels whose new messages Gmail does not notify about (with categories on), nor do we.
const NOT_NOTIFIED: &[&str] = &[
    labels::SENT,
    labels::DRAFT,
    labels::SPAM,
    "CATEGORY_PROMOTIONS",
    "CATEGORY_SOCIAL",
    "CATEGORY_UPDATES",
    "CATEGORY_FORUMS",
];
/// Labels whose counts every round and every change refreshes.
const ROUND_COUNTS: &[&str] = &[labels::INBOX, labels::SPAM, labels::DRAFT];
const CHANGE_COUNTS: &[&str] = &[labels::INBOX, labels::SPAM, labels::TRASH, labels::DRAFT];

/// Starts the Google worker with the real endpoints, the compiled-in client and the sync
/// preferences from `prefs`.
pub fn start_google(
    secrets: Secrets,
    prefs: &Prefs,
) -> std::io::Result<(SyncHandle, async_channel::Receiver<SyncEvent>)> {
    start_with(
        secrets,
        Endpoints::google(),
        oauth::builtin_client(),
        Tuning::new(prefs.refresh_secs, prefs.page_size),
        None,
    )
}

/// Starts the worker against `endpoints` with `builtin` as the fallback client, saving
/// attachments to `downloads` (the user's download folder when `None`).
fn start_with(
    secrets: Secrets,
    endpoints: Endpoints,
    builtin: Option<ClientCredentials>,
    tuning: Tuning,
    downloads: Option<PathBuf>,
) -> std::io::Result<(SyncHandle, async_channel::Receiver<SyncEvent>)> {
    let (tx, rx) = mpsc::channel();
    let (etx, erx) = async_channel::unbounded();
    let self_tx = tx.clone();
    std::thread::Builder::new()
        .name("eloquawk-sync".into())
        .spawn(move || {
            let mut worker = Worker::new(secrets, etx, self_tx, endpoints, builtin, tuning);
            worker.downloads = downloads;
            worker.run(rx)
        })?;
    Ok((SyncHandle::from_sender(tx), erx))
}

/// The sticky message for an account whose refresh token Google rejected: a short hint says
/// why, depending on whether Google asked for a reauthentication (`invalid_rapt`).
pub fn signed_out_message(email: &str, description: &str) -> String {
    let d = description.to_ascii_lowercase();
    let hint = if d.contains("invalid_rapt") || d.contains("reauth") {
        "(Your organisation requires you to sign in again.)"
    } else {
        "(Test-mode tokens expire after 7 days.)"
    };
    format!("Google signed {email} out. Add it again. {hint}")
}

/// The sticky message for an account whose token lacks the Gmail scope.
fn missing_scope_message(email: &str) -> String {
    format!(
        "Eloquawk needs permission to read and organise mail for {email}. Add the account again."
    )
}

/// The poll interval, page size and backoff schedule. Tests shrink the durations.
#[derive(Clone, Copy, Debug)]
struct Tuning {
    refresh: Duration,
    page_size: u32,
    backoff_base: Duration,
    backoff_cap: Duration,
    jitter_max: Duration,
}

impl Tuning {
    fn new(refresh_secs: u64, page_size: u32) -> Self {
        Self {
            refresh: Duration::from_secs(refresh_secs.max(1)),
            page_size: page_size.max(1),
            backoff_base: Duration::from_secs(5),
            backoff_cap: Duration::from_secs(300),
            jitter_max: Duration::from_millis(1000),
        }
    }

    /// A random delay below `jitter_max`, so accounts and clients do not retry in step.
    fn jitter(&self) -> Duration {
        let max = u64::try_from(self.jitter_max.as_millis()).unwrap_or(u64::MAX);
        if max == 0 {
            return Duration::ZERO;
        }
        let mut buf = [0u8; 8];
        if getrandom::fill(&mut buf).is_err() {
            return Duration::ZERO;
        }
        Duration::from_millis(u64::from_le_bytes(buf) % max)
    }
}

/// An account's retry schedule after retryable failures (SPEC 7.6). Pure: every method takes
/// `now`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Backoff {
    attempts: u32,
    retry_at: Option<Instant>,
}

impl Backoff {
    /// Whether the account must still wait.
    fn waiting(&self, now: Instant) -> bool {
        self.retry_at.is_some_and(|t| t > now)
    }

    /// A rate limit, network failure, 5xx or unreadable answer: wait
    /// `max(hint, min(base·2^attempts, cap)) + jitter`. While the account is already waiting
    /// only interactive jobs run, and their failures (clicks while offline) must not hurry
    /// the schedule towards the cap: they only lengthen the wait to a longer hint.
    fn failed(&mut self, now: Instant, hint: Option<Duration>, jitter: Duration, t: &Tuning) {
        if self.waiting(now) {
            if let Some(hint) = hint {
                self.retry_at = self.retry_at.max(Some(now + hint));
            }
            return;
        }
        let factor = 1u32.checked_shl(self.attempts).unwrap_or(u32::MAX);
        let exp = t.backoff_base.saturating_mul(factor).min(t.backoff_cap);
        self.attempts = self.attempts.saturating_add(1);
        self.retry_at = Some(now + hint.unwrap_or_default().max(exp) + jitter);
    }

    /// A daily or sending limit: nothing automatic before the hint or 15 minutes, whichever
    /// is later.
    fn quota(&mut self, now: Instant, hint: Option<Duration>) {
        self.attempts = self.attempts.saturating_add(1);
        let until = now + hint.unwrap_or_default().max(QUOTA_WAIT);
        self.retry_at = self.retry_at.max(Some(until));
    }

    /// A success, or a manual Refresh: start afresh.
    fn reset(&mut self) {
        *self = Self::default();
    }
}

/// What a [`Mutation`] does to each thread.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Change {
    Modify(LabelDelta),
    Trash,
    /// Untrash, then apply the delta (when not empty) to put the labels back.
    Untrash(LabelDelta),
}

/// One optimistic change the store is waiting to hear about.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Mutation {
    op: OpId,
    account: AccountId,
    thread_ids: Vec<String>,
    change: Change,
}

/// A message to send. Two are the same job when they share an op id (`Outgoing` has no
/// equality of its own, and its `Debug` is redacting).
#[derive(Clone, Debug)]
struct Outbox {
    op: OpId,
    outgoing: Outgoing,
}

impl PartialEq for Outbox {
    fn eq(&self, other: &Self) -> bool {
        self.op == other.op
    }
}

impl Eq for Outbox {}

/// An attachment to save (SPEC 7.5 `SaveAttachment`). Two are the same job when they name the
/// same part of the same message; `Debug` leaves the filename out.
#[derive(Clone)]
struct Download {
    account: AccountId,
    message_id: String,
    attachment: Attachment,
}

impl PartialEq for Download {
    fn eq(&self, other: &Self) -> bool {
        self.account == other.account
            && self.message_id == other.message_id
            && self.attachment.part_id == other.attachment.part_id
    }
}

impl Eq for Download {}

impl std::fmt::Debug for Download {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Download")
            .field("message_id", &self.message_id)
            .field("part_id", &self.attachment.part_id)
            .finish_non_exhaustive()
    }
}

/// One unit of work (SPEC 7.2).
#[derive(Clone, Debug, PartialEq, Eq)]
enum Job {
    // Interactive lane: the front of the queue, FIFO among themselves.
    OpenThread(ThreadKey),
    /// A view's page 1 (`false`) or next page (`true`).
    ListView(ViewKey, bool),
    Mutate(Mutation),
    Send(Box<Outbox>),
    Save(Box<Download>),
    // Background lane: the back of the queue, coalesced.
    InitialSync(AccountId),
    /// Label ids whose counts to fetch, at most [`CHUNK`].
    Labels(AccountId, Vec<String>),
    /// Thread ids whose metadata to fetch, at most [`CHUNK`].
    FetchMeta(AccountId, Vec<String>),
    History(AccountId),
    /// The next chunk of a history round's changed threads (SPEC 7.4 step 4).
    Refetch(AccountId),
    /// A new thread's full detail for a background AI check (SPEC 16.11).
    Prefetch(ThreadKey),
}

impl Job {
    fn interactive(&self) -> bool {
        matches!(
            self,
            Job::OpenThread(_) | Job::ListView(..) | Job::Mutate(_) | Job::Send(_) | Job::Save(_)
        )
    }

    fn account(&self) -> &str {
        match self {
            Job::OpenThread(key) | Job::Prefetch(key) => &key.account,
            Job::ListView(view, _) => &view.account,
            Job::Mutate(m) => &m.account,
            Job::Send(s) => &s.outgoing.account,
            Job::Save(d) => &d.account,
            Job::InitialSync(a)
            | Job::Labels(a, _)
            | Job::FetchMeta(a, _)
            | Job::History(a)
            | Job::Refetch(a) => a,
        }
    }

    /// Whether this job shows the busy dot.
    fn shows_busy(&self) -> bool {
        matches!(
            self,
            Job::ListView(..) | Job::InitialSync(_) | Job::History(_)
        )
    }
}

/// The job queue's two lanes.
#[derive(Debug, Default)]
struct Queue {
    interactive: VecDeque<Job>,
    background: VecDeque<Job>,
}

impl Queue {
    /// Queues `job` at the back of its lane, unless the same job is already waiting.
    fn push(&mut self, job: Job) {
        let lane = if job.interactive() {
            &mut self.interactive
        } else {
            &mut self.background
        };
        if !lane.contains(&job) {
            lane.push_back(job);
        }
    }

    /// Queues background `jobs` at the front of the background lane, in order: the metadata
    /// of a page someone is looking at comes before label counts.
    fn push_soon(&mut self, jobs: Vec<Job>) {
        for job in jobs.into_iter().rev() {
            debug_assert!(!job.interactive());
            if !self.background.contains(&job) {
                self.background.push_front(job);
            }
        }
    }

    fn pop(&mut self) -> Option<Job> {
        self.interactive
            .pop_front()
            .or_else(|| self.background.pop_front())
    }

    fn is_empty(&self) -> bool {
        self.interactive.is_empty() && self.background.is_empty()
    }

    /// The thread ids already waiting in `account`'s queued metadata chunks.
    fn queued_meta(&self, account: &str) -> HashSet<String> {
        meta_ids(self.background.iter(), account)
    }

    /// Takes `ids` out of `account`'s queued metadata chunks (dropping chunks left empty),
    /// so a fresh listing can queue them again at the front without fetching them twice.
    fn strip_meta(&mut self, account: &str, ids: &[String]) {
        if ids.is_empty() {
            return;
        }
        let ids: HashSet<&String> = ids.iter().collect();
        self.background.retain_mut(|job| match job {
            Job::FetchMeta(a, chunk) if a == account => {
                chunk.retain(|id| !ids.contains(id));
                !chunk.is_empty()
            }
            _ => true,
        });
    }

    /// Drops every job of a signed-out account.
    fn forget(&mut self, account: &str) {
        self.interactive.retain(|j| j.account() != account);
        self.background.retain(|j| j.account() != account);
    }
}

/// The thread ids in `account`'s metadata chunks among `jobs`.
fn meta_ids<'a>(jobs: impl Iterator<Item = &'a Job>, account: &str) -> HashSet<String> {
    jobs.filter_map(|j| match j {
        Job::FetchMeta(a, ids) if a == account => Some(ids),
        _ => None,
    })
    .flatten()
    .cloned()
    .collect()
}

/// Splits `ids` into jobs of at most [`CHUNK`] ids.
fn chunked(ids: &[String], job: impl Fn(Vec<String>) -> Job) -> Vec<Job> {
    ids.chunks(CHUNK).map(|c| job(c.to_vec())).collect()
}

/// Why a Google call failed.
enum CallError {
    /// The account has no usable token (it was just signed out, or the client is rejected).
    Auth,
    /// The account is no longer signed in at all; skip it quietly.
    Gone,
    /// The token endpoint could not be reached (or failed with a 5xx).
    Network,
    Api(ApiError),
}

impl CallError {
    /// The text a failed thread shows in the reader.
    fn reader_message(&self) -> String {
        match self {
            CallError::Auth | CallError::Gone => SIGNED_OUT.into(),
            CallError::Network => NETWORK_PROBLEM.into(),
            CallError::Api(ApiError::TooLarge) => "Message too large to open".into(),
            CallError::Api(ApiError::NotFound) => "This conversation is no longer in Gmail.".into(),
            CallError::Api(e) => e.to_string(),
        }
    }
}

struct Access {
    token: String,
    expires: Instant,
}

/// What the worker last fetched of a thread.
struct Known {
    history_id: u64,
    /// The message a star goes on: the last one that is not a draft (SPEC 6.2).
    star_target: Option<String>,
    summary: ThreadSummary,
}

/// What one history round changed (SPEC 7.4 step 1), from [`collect_changes`].
#[derive(Debug, Default, PartialEq, Eq)]
struct Changes {
    /// Every thread a message was added to, deleted from, or relabelled in.
    changed: BTreeSet<String>,
    /// Threads with a new unread inbox message Gmail would notify about.
    fresh_inbox: BTreeSet<String>,
    /// Labels added to or removed from a message, whose counts moved.
    labels: BTreeSet<String>,
    /// The cursor for the next round: the last page's history id.
    new_history_id: Option<u64>,
}

/// Folds history pages into the threads to refetch and the new inbox mail (SPEC 7.4). Pure.
fn collect_changes(pages: &[gmail::HistoryList]) -> Changes {
    let mut c = Changes::default();
    let mut changed = |thread_id: &str| {
        if !thread_id.is_empty() {
            c.changed.insert(thread_id.to_string());
        }
    };
    let mut fresh = Vec::new();
    let mut moved = Vec::new();
    for record in pages.iter().flat_map(|p| &p.history) {
        for m in &record.messages {
            changed(&m.thread_id);
        }
        for added in &record.messages_added {
            let m = &added.message;
            changed(&m.thread_id);
            let has = |l: &str| m.label_ids.iter().any(|id| id == l);
            if has(labels::INBOX)
                && has(labels::UNREAD)
                && !NOT_NOTIFIED.iter().any(|l| has(l))
                && !m.thread_id.is_empty()
            {
                fresh.push(m.thread_id.clone());
            }
        }
        for deleted in &record.messages_deleted {
            changed(&deleted.message.thread_id);
        }
        for l in record.labels_added.iter().chain(&record.labels_removed) {
            changed(&l.message.thread_id);
            moved.extend(l.label_ids.iter().cloned());
        }
    }
    c.fresh_inbox.extend(fresh);
    c.labels.extend(moved);
    c.new_history_id = pages
        .last()
        .map(|p| parse::parse_u64(&p.history_id))
        .filter(|h| *h > 0);
    c
}

/// Whether a delta is exactly "star", which goes on one message rather than the thread.
fn is_star(delta: &LabelDelta) -> bool {
    delta.remove.is_empty() && delta.add == [labels::STARRED]
}

/// The message a star goes on: the last one that is not a draft.
fn star_target(dto: &gmail::ThreadDto) -> Option<String> {
    dto.messages
        .iter()
        .rev()
        .find(|m| !m.label_ids.iter().any(|l| l == labels::DRAFT))
        .map(|m| m.id.clone())
}

/// The text a failed change shows (the store rolls the change back with it).
fn change_message(e: &CallError) -> String {
    match e {
        CallError::Auth | CallError::Gone => SIGNED_OUT.into(),
        CallError::Network | CallError::Api(ApiError::Network(_)) => NETWORK_PROBLEM.into(),
        CallError::Api(ApiError::NotFound) => "This conversation is no longer in Gmail.".into(),
        CallError::Api(e) => e.to_string(),
    }
}

/// How a failed send is reported (SPEC 7.5). A send is never retried: when Google may have
/// taken the message (the connection failed, a 5xx, or a success answer that could not be
/// read) the composer says to check Sent; a refusal, or a failure before anything went out,
/// carries its reason.
fn send_error(e: &CallError) -> SendError {
    let (message, maybe_sent) = match e {
        CallError::Api(
            ApiError::Network(_)
            | ApiError::Http { status: 500.., .. }
            | ApiError::Decode(_)
            | ApiError::TooLarge,
        ) => (MAYBE_SENT.to_string(), true),
        CallError::Api(e) => (e.to_string(), false),
        // The token could not be refreshed, so nothing was sent.
        CallError::Network => (NETWORK_PROBLEM.to_string(), false),
        CallError::Auth | CallError::Gone => (SIGNED_OUT.to_string(), false),
    };
    SendError {
        message,
        maybe_sent,
    }
}

/// The text a failed attachment download shows (a save's toast, a forward's send error).
fn attachment_message(e: &CallError) -> String {
    match e {
        CallError::Api(ApiError::TooLarge) => ATTACHMENT_TOO_LARGE.into(),
        CallError::Api(ApiError::NotFound) => ATTACHMENT_GONE.into(),
        e => e.reader_message(),
    }
}

/// A short reason for a failed file write, for the toast.
fn io_message(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::PermissionDenied => "permission denied".into(),
        std::io::ErrorKind::StorageFull => "the disk is full".into(),
        std::io::ErrorKind::NotFound => "the download folder was not found".into(),
        std::io::ErrorKind::ReadOnlyFilesystem => "the disk is read-only".into(),
        kind => kind.to_string(),
    }
}

/// Finds the part `part_id` in a message's part tree.
fn find_part<'a>(part: &'a gmail::PartDto, part_id: &str) -> Option<&'a gmail::PartDto> {
    if part.part_id == part_id {
        return Some(part);
    }
    part.parts.iter().find_map(|p| find_part(p, part_id))
}

/// Gathers a message's attachments within the 25 MB budget, in order: a Gmail-held one (a
/// forward's) through `gmail`, a local one read from disk. Nothing has been uploaded when this
/// fails, so every error is `maybe_sent: false`.
fn gather_attachments(
    out: &Outgoing,
    mut gmail: impl FnMut(&str, &str) -> Result<Vec<u8>, String>,
) -> Result<Vec<(OutgoingAttachment, Vec<u8>)>, SendError> {
    let refuse = |message: String| SendError {
        message,
        maybe_sent: false,
    };
    let mut budget = MAX_OUTGOING_ATTACHMENTS;
    let mut parts = Vec::with_capacity(out.attachments.len());
    for a in &out.attachments {
        let bytes = match &a.source {
            // The declared size is close to the decoded one: refuse before downloading.
            AttachmentSource::Gmail { .. } if a.size > budget => {
                return Err(refuse(compose::TOO_LARGE_TO_SEND.into()));
            }
            AttachmentSource::Gmail {
                message_id,
                part_id,
            } => gmail(message_id, part_id).map_err(|e| {
                refuse(format!(
                    "Couldn't attach {}: {e}",
                    files::sanitise(&a.filename)
                ))
            })?,
            AttachmentSource::Local(path) => compose::read_local(path, budget).map_err(refuse)?,
        };
        budget = budget
            .checked_sub(bytes.len() as u64)
            .ok_or_else(|| refuse(compose::TOO_LARGE_TO_SEND.into()))?;
        let mut a = a.clone();
        a.size = bytes.len() as u64;
        parts.push((a, bytes));
    }
    Ok(parts)
}

/// Whether the sidebar shows a label, so its count is worth fetching.
fn shown(l: &Label) -> bool {
    match l.kind {
        LabelKind::System => SHOWN_SYSTEM_LABELS.contains(&l.id.as_str()),
        LabelKind::User => !l.hidden,
    }
}

struct AccountState {
    refresh_token: String,
    access: Option<Access>,
    /// The history cursor, recorded by `profile` before listing.
    history_id: Option<u64>,
    synced_once: bool,
    known: HashMap<String, Known>,
    /// The next page token per view; `None` once the last page is in.
    pages: HashMap<ViewKind, Option<String>>,
    /// Listed ids still without a summary (skeleton rows), with their listed history ids.
    missing: BTreeMap<String, u64>,
    /// Ids whose metadata Gmail refused without a retry (403, too large), with the history
    /// id they were refused at. Neither the poll nor a listing fetches them again until a
    /// listing shows a new history id.
    refused: HashMap<String, u64>,
    /// Thread ids a history round found changed whose refetch has not succeeded yet. They
    /// survive failures, so moving the cursor can never lose a change.
    pending_changed: BTreeSet<String>,
    /// Candidate new-mail thread ids not reported yet.
    pending_fresh: BTreeSet<String>,
    /// Labels whose counts the round in progress moved.
    round_labels: BTreeSet<String>,
    /// Background jobs parked while in backoff.
    deferred: Vec<Job>,
    backoff: Backoff,
    /// The last labels and counts, so a full list keeps the counts while new ones load.
    labels: BTreeMap<String, Label>,
}

impl AccountState {
    fn new(refresh_token: String, access: Option<Access>) -> Self {
        Self {
            refresh_token,
            access,
            history_id: None,
            synced_once: false,
            known: HashMap::new(),
            pages: HashMap::new(),
            missing: BTreeMap::new(),
            refused: HashMap::new(),
            pending_changed: BTreeSet::new(),
            pending_fresh: BTreeSet::new(),
            round_labels: BTreeSet::new(),
            deferred: Vec::new(),
            backoff: Backoff::default(),
            labels: BTreeMap::new(),
        }
    }

    /// Parks a background job until the backoff passes.
    fn defer(&mut self, job: Job) {
        if !self.deferred.contains(&job) {
            self.deferred.push(job);
        }
    }

    /// SPEC 7.4 step 2: the cursor moves together with the work it implies, in one step.
    fn commit_round(&mut self, changes: Changes) {
        self.pending_changed.extend(changes.changed);
        self.pending_fresh.extend(changes.fresh_inbox);
        self.round_labels.extend(changes.labels);
        if let Some(h) = changes.new_history_id {
            self.history_id = Some(h);
        }
    }

    /// The next chunk of changed threads to refetch, new inbox mail first. A burst of more
    /// than [`BURST`] keeps only the newest [`BURST_FRESH`] new inbox threads and drops the
    /// rest from the cache, so a later listing fetches them instead.
    fn next_refetch(&mut self) -> Vec<String> {
        if self.pending_changed.len() > BURST {
            let mut fresh: Vec<String> = self
                .pending_fresh
                .iter()
                .filter(|id| self.pending_changed.contains(*id))
                .cloned()
                .collect();
            // Gmail thread ids grow with time, so the longest, then greatest, is the newest.
            fresh.sort_by(|a, b| (b.len(), b).cmp(&(a.len(), a)));
            fresh.truncate(BURST_FRESH);
            let keep: BTreeSet<String> = fresh.into_iter().collect();
            for id in &self.pending_changed {
                if !keep.contains(id) {
                    self.known.remove(id);
                }
            }
            self.pending_fresh.retain(|id| keep.contains(id));
            self.pending_changed = keep;
        }
        let mut ids: Vec<String> = self
            .pending_fresh
            .iter()
            .filter(|id| self.pending_changed.contains(*id))
            .take(CHUNK)
            .cloned()
            .collect();
        for id in &self.pending_changed {
            if ids.len() >= CHUNK {
                break;
            }
            if !ids.contains(id) {
                ids.push(id.clone());
            }
        }
        ids
    }

    /// A changed thread was refetched.
    fn refetch_succeeded(&mut self, id: &str) {
        self.pending_changed.remove(id);
    }

    /// A changed thread is gone from Gmail: nothing to refetch or report.
    fn refetch_gone(&mut self, id: &str) {
        self.pending_changed.remove(id);
        self.pending_fresh.remove(id);
        self.known.remove(id);
        self.missing.remove(id);
        self.refused.remove(id);
    }

    /// SPEC 7.4 step 7, once every changed thread is refetched: the candidates still unread
    /// and in the inbox, newest first. Every candidate is cleared, reported or not.
    fn take_new_mail(&mut self) -> Vec<ThreadSummary> {
        let mut threads: Vec<ThreadSummary> = std::mem::take(&mut self.pending_fresh)
            .iter()
            .filter_map(|id| self.known.get(id))
            .map(|k| &k.summary)
            .filter(|s| s.unread() && s.in_label(labels::INBOX))
            .cloned()
            .collect();
        threads.sort_by_key(|s| std::cmp::Reverse(s.last_date));
        threads
    }

    /// SPEC 7.4 step 8: the history is too old to follow, so every cache goes and the next
    /// sync is a first one again (which never reports new mail).
    fn reset(&mut self) {
        self.known.clear();
        self.pages.clear();
        self.history_id = None;
        self.synced_once = false;
        self.pending_changed.clear();
        self.pending_fresh.clear();
        self.round_labels.clear();
        self.missing.clear();
        self.refused.clear();
        self.deferred
            .retain(|j| !matches!(j, Job::FetchMeta(..) | Job::Refetch(_) | Job::History(_)));
    }
}

struct SignIn {
    seq: u64,
    cancel: Arc<AtomicBool>,
}

#[derive(Clone, Copy)]
enum Revoke {
    WithGoogle,
    LocalOnly,
}

struct Worker {
    events: async_channel::Sender<SyncEvent>,
    self_tx: mpsc::Sender<Command>,
    agent: ureq::Agent,
    endpoints: Endpoints,
    secrets: Secrets,
    /// The compiled-in client, used when none is saved.
    builtin: Option<ClientCredentials>,
    /// The client in use: the saved one, else the built-in one.
    client: Option<ClientCredentials>,
    auth: AuthState,
    accounts: BTreeMap<AccountId, AccountState>,
    sign_in: Option<SignIn>,
    sign_in_seq: u64,
    /// Sticky messages for the banner, until dismissed or resolved. Keys: an account's email,
    /// "client", "tokens" or "network".
    problems: BTreeMap<String, String>,
    /// The last keyring read failed (locked, no Secret Service); retried on Refresh.
    keyring_failed: bool,
    tuning: Tuning,
    queue: Queue,
    /// The view the UI shows.
    want: Option<ViewKey>,
    last_poll: Instant,
    next_poll: Instant,
    busy: bool,
    /// Where saved attachments go; `None` resolves the user's download folder on each save.
    /// Tests point it at a temporary folder.
    downloads: Option<PathBuf>,
}

impl Worker {
    fn new(
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
        }
    }

    fn emit(&self, ev: SyncEvent) {
        let _ = self.events.send_blocking(ev);
    }

    fn set_auth(&mut self, auth: AuthState) {
        if self.auth != auth {
            self.auth = auth;
            self.emit(SyncEvent::Auth(auth));
        }
    }

    /// Sets the banner to the sticky problems.
    fn report(&self) {
        let all: Vec<&str> = self.problems.values().map(String::as_str).collect();
        self.emit(SyncEvent::Problem(
            (!all.is_empty()).then(|| all.join(" · ")),
        ));
    }

    fn emit_accounts(&self) {
        self.emit(SyncEvent::Accounts(self.accounts.keys().cloned().collect()));
    }

    fn run(mut self, rx: mpsc::Receiver<Command>) {
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
        self.cancel_sign_in();
    }

    /// How long the loop may sleep: not at all while jobs wait, otherwise until the next poll
    /// or the earliest backoff with parked jobs, capped at [`MAX_WAIT`].
    fn wait(&self, now: Instant) -> Duration {
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
    fn tick(&mut self, now: Instant) {
        self.requeue_deferred(now);
        self.step(now);
        if Instant::now() >= self.next_poll {
            self.poll(Instant::now());
        }
        if self.busy && self.queue.is_empty() {
            self.busy = false;
            self.emit(SyncEvent::Busy(false));
        }
    }

    fn requeue_deferred(&mut self, now: Instant) {
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
    fn poll(&mut self, now: Instant) {
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
    fn step(&mut self, now: Instant) {
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
            Job::Save(d) => self.save(*d),
            Job::InitialSync(email) => self.initial_sync(&email),
            Job::Labels(email, ids) => self.label_counts(&email, ids),
            Job::FetchMeta(email, ids) => self.fetch_meta(&email, ids),
            Job::History(email) => self.history(&email),
            Job::Refetch(email) => self.refetch(&email),
        }
    }

    fn handle(&mut self, cmd: Command) {
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

    // ----- keyring --------------------------------------------------------------------------

    fn load_from_keyring(&mut self) {
        self.keyring_failed = false;
        let saved = match self.secrets.client() {
            Ok(saved) => saved,
            // Unlocking cannot fix an unreadable client, so drop it (and the tokens it issued)
            // and say where to set it again.
            Err(SecretError::Corrupt(_)) => {
                let _ = self.secrets.clear_client();
                self.problems.insert(
                    "client".into(),
                    "Your saved OAuth client was unreadable and was removed; set it again in \
                     Settings and add your accounts again."
                        .into(),
                );
                None
            }
            Err(e) => {
                self.locked(e);
                return;
            }
        };
        self.emit(SyncEvent::Client(
            saved.as_ref().map(|c| c.client_id.clone()),
        ));
        self.client = saved.or_else(|| self.builtin.clone());
        if self.client.is_none() {
            self.set_auth(AuthState::NeedsClient);
            self.report();
            return;
        }
        let tokens = match self.secrets.tokens() {
            Ok(t) => t,
            // Unreadable, so unusable: start afresh rather than failing forever.
            // The parse error is left out: it could quote a token.
            Err(SecretError::Corrupt(_)) => {
                let _ = self.secrets.clear_tokens();
                self.problems.insert(
                    "tokens".into(),
                    "Stored sign-ins were unreadable and were reset; add your accounts again."
                        .into(),
                );
                BTreeMap::new()
            }
            Err(e) => {
                self.locked(e);
                return;
            }
        };
        // Accounts added while the keyring was unavailable live for this session only; a
        // reload must not forget them.
        for (email, refresh_token) in tokens {
            self.accounts
                .entry(email)
                .or_insert_with(|| AccountState::new(refresh_token, None));
        }
        self.emit_accounts();
        self.set_auth(AuthState::Ready);
        self.report();
        let emails: Vec<AccountId> = self.accounts.keys().cloned().collect();
        for email in emails {
            self.queue.push(Job::InitialSync(email));
        }
    }

    fn locked(&mut self, e: SecretError) {
        self.keyring_failed = true;
        self.emit(SyncEvent::Problem(Some(format!(
            "{e}. Eloquawk keeps Google credentials only in the system keyring; unlock it and \
             press Ctrl+R to retry."
        ))));
        self.set_auth(AuthState::Locked);
    }

    // ----- client ---------------------------------------------------------------------------

    fn save_client(&mut self, client_id: String, client_secret: Option<String>) {
        let client_id = client_id.trim().to_string();
        if client_id.is_empty() {
            self.emit(SyncEvent::Problem(Some("Enter a client ID.".into())));
            return;
        }
        let secret = client_secret
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let client_secret = match (secret, &self.client) {
            (Some(s), _) => s,
            (None, Some(existing)) if existing.client_id == client_id => {
                existing.client_secret.clone()
            }
            _ => {
                self.emit(SyncEvent::Problem(Some(
                    "Enter the client secret for this client ID.".into(),
                )));
                return;
            }
        };
        let client = ClientCredentials {
            client_id,
            client_secret,
        };
        let saved = matches!(self.secrets.client(), Ok(Some(ref c)) if *c == client);
        if saved {
            self.emit(SyncEvent::Notice("OAuth client unchanged".into()));
            return;
        }
        let same_id = self
            .client
            .as_ref()
            .is_some_and(|c| c.client_id == client.client_id);
        // The keyring write comes first: if it fails, the old client and its accounts stay
        // as they were.
        match self.secrets.set_client(&client) {
            Ok(()) => {
                if !same_id {
                    // Refresh tokens belong to the client id that issued them; a rotated
                    // secret for the same client keeps every session.
                    self.remove_all_accounts(Revoke::WithGoogle);
                } else {
                    // Saving the client in use (say, the built-in one) keeps its accounts,
                    // but the keyring dropped their tokens when no client was stored there.
                    for (email, account) in &self.accounts {
                        let _ = self.secrets.set_token(email, &account.refresh_token);
                    }
                }
                self.emit(SyncEvent::Client(Some(client.client_id.clone())));
                self.client = Some(client);
                self.problems.remove("client");
                self.report();
                self.emit(SyncEvent::Notice(
                    "OAuth client saved to the keyring".into(),
                ));
                self.set_auth(AuthState::Ready);
            }
            Err(e) => {
                // `set_client` may have dropped the stored tokens before failing; put back
                // those of the accounts still in use.
                for (email, account) in &self.accounts {
                    let _ = self.secrets.set_token(email, &account.refresh_token);
                }
                self.emit(SyncEvent::Problem(Some(e.to_string())));
            }
        }
    }

    fn clear_client(&mut self) {
        self.remove_all_accounts(Revoke::WithGoogle);
        match self.secrets.clear_client() {
            Ok(()) => {
                self.client = self.builtin.clone();
                self.problems.remove("client");
                self.report();
                self.emit(SyncEvent::Client(None));
                if self.client.is_some() {
                    self.emit(SyncEvent::Notice(
                        "Your OAuth client was removed; using the built-in client".into(),
                    ));
                    self.set_auth(AuthState::Ready);
                } else {
                    self.emit(SyncEvent::Notice("Your OAuth client was removed".into()));
                    self.set_auth(AuthState::NeedsClient);
                }
            }
            Err(e) => self.emit(SyncEvent::Problem(Some(e.to_string()))),
        }
    }

    // ----- sign-in --------------------------------------------------------------------------

    fn cancel_sign_in(&mut self) {
        if let Some(sign_in) = self.sign_in.take() {
            sign_in.cancel.store(true, Ordering::Relaxed);
            self.emit(SyncEvent::SigningIn(None));
        }
    }

    fn begin_sign_in(&mut self) {
        let Some(client) = self.client.clone() else {
            self.emit(SyncEvent::Problem(Some(
                "Set up a Google client in Settings first.".into(),
            )));
            return;
        };
        self.cancel_sign_in();
        let loopback = match oauth::Loopback::bind() {
            Ok(l) => l,
            Err(e) => {
                self.emit(SyncEvent::Problem(Some(e.to_string())));
                return;
            }
        };
        let pkce = oauth::new_pkce();
        let state = oauth::random_token(24);
        let url = oauth::authorize_url(
            &client.client_id,
            &loopback.redirect_uri,
            &pkce.challenge,
            &state,
            None,
        );
        self.sign_in_seq += 1;
        let seq = self.sign_in_seq;
        let cancel = Arc::new(AtomicBool::new(false));
        self.sign_in = Some(SignIn {
            seq,
            cancel: cancel.clone(),
        });
        self.report();
        self.emit(SyncEvent::SigningIn(Some(url.clone())));
        if let Err(e) = open::that_detached(&url) {
            eprintln!("eloquawk: could not open a browser: {}", e.kind());
        }
        let tx = self.self_tx.clone();
        let agent = self.agent.clone();
        let token_url = self.endpoints.token.clone();
        let spawned = std::thread::Builder::new()
            .name("eloquawk-sign-in".into())
            .spawn(move || {
                let result = loopback
                    .wait_for_code(&state, &cancel, SIGN_IN_TIMEOUT)
                    .and_then(|code| {
                        oauth::exchange_code(
                            &agent,
                            &token_url,
                            &client,
                            &code,
                            &pkce.verifier,
                            &loopback.redirect_uri,
                        )
                    });
                let _ = tx.send(Command::SignInFinished(seq, result));
            });
        if let Err(error) = spawned {
            self.sign_in = None;
            self.emit(SyncEvent::SigningIn(None));
            self.emit(SyncEvent::Problem(Some(format!(
                "Could not start the sign-in: {error}"
            ))));
        }
    }

    fn finish_sign_in(&mut self, seq: u64, result: Result<TokenResponse, AuthError>) {
        // Superseded or cancelled: drop whatever Google issued. Never revoke a token whose
        // account is unknown: revoking removes the app's whole grant for that user, which
        // would also sign out an account that is already here.
        if self.sign_in.as_ref().map(|s| s.seq) != Some(seq) {
            return;
        }
        self.sign_in = None;
        self.emit(SyncEvent::SigningIn(None));
        let tokens = match result {
            Ok(t) => t,
            Err(AuthError::Cancelled) => return,
            Err(e) => {
                self.emit(SyncEvent::Problem(Some(e.to_string())));
                return;
            }
        };
        // Granular consent: the user may have unticked Gmail. Check before saving anything.
        if !oauth::scope_granted(tokens.scope.as_deref()) {
            // Which account this is cannot be known without the scope (profile needs it).
            // With no account here it cannot be a known one, so the useless grant is revoked;
            // otherwise it is left alone, since revoking could sign out an existing account.
            if self.accounts.is_empty() {
                let token = tokens
                    .refresh_token
                    .as_ref()
                    .unwrap_or(&tokens.access_token);
                oauth::revoke(&self.agent, &self.endpoints.revoke, token);
            }
            self.emit(SyncEvent::Problem(Some(MISSING_SCOPE_SIGN_IN.into())));
            return;
        }
        let email = match gmail::profile(&self.agent, &self.endpoints, &tokens.access_token) {
            Ok(profile) => profile.email_address,
            Err(e) => {
                self.emit(SyncEvent::Problem(Some(format!(
                    "Signed in, but could not read the Gmail profile: {e}"
                ))));
                return;
            }
        };
        let Some(refresh_token) = tokens
            .refresh_token
            .clone()
            .or_else(|| self.accounts.get(&email).map(|a| a.refresh_token.clone()))
        else {
            self.emit(SyncEvent::Problem(Some(NO_REFRESH_TOKEN.into())));
            return;
        };
        self.problems.remove(&email);
        if let Err(e) = self.secrets.set_token(&email, &refresh_token) {
            self.problems.insert(
                email.clone(),
                format!("{e}. {email} is signed in for this session only."),
            );
        }
        let access = Some(Access {
            token: tokens.access_token,
            expires: Instant::now() + Duration::from_secs(tokens.expires_in),
        });
        // Signing in again replaces the refresh token; the old one is not revoked, because
        // that would revoke the whole grant, including the new token. The caches stay.
        let again = match self.accounts.get_mut(&email) {
            Some(account) => {
                account.refresh_token = refresh_token;
                account.access = access;
                account.backoff.reset();
                true
            }
            None => {
                self.accounts
                    .insert(email.clone(), AccountState::new(refresh_token, access));
                false
            }
        };
        self.queue.push(Job::InitialSync(email.clone()));
        self.emit(SyncEvent::Notice(if again {
            format!("{email} signed in again")
        } else {
            format!("Added {email}")
        }));
        self.report();
        self.emit_accounts();
    }

    // ----- accounts and tokens --------------------------------------------------------------

    /// Forgets one account. `Revoke::WithGoogle` also invalidates its refresh token with
    /// Google. Returns whether the account existed.
    fn remove_account(&mut self, email: &str, revoke: Revoke) -> bool {
        let Some(account) = self.accounts.remove(email) else {
            return false;
        };
        self.queue.forget(email);
        if matches!(revoke, Revoke::WithGoogle)
            && !oauth::revoke(&self.agent, &self.endpoints.revoke, &account.refresh_token)
        {
            self.emit(SyncEvent::Notice(format!(
                "Signed out of {email} here, but Google could not be told. Remove access at \
                 myaccount.google.com/permissions."
            )));
        }
        if let Err(e) = self.secrets.remove_token(email) {
            self.problems.insert(email.to_string(), e.to_string());
            self.report();
        }
        self.emit_accounts();
        true
    }

    fn remove_all_accounts(&mut self, revoke: Revoke) {
        self.cancel_sign_in();
        let emails: Vec<String> = self.accounts.keys().cloned().collect();
        for email in emails {
            self.remove_account(&email, revoke);
        }
        if let Err(e) = self.secrets.clear_tokens() {
            self.emit(SyncEvent::Problem(Some(e.to_string())));
        }
    }

    /// A valid access token for `email`, refreshed when less than a minute remains.
    fn access_token(&mut self, email: &str) -> Result<String, CallError> {
        let client = self.client.clone().ok_or(CallError::Gone)?;
        let account = self.accounts.get(email).ok_or(CallError::Gone)?;
        if let Some(a) = &account.access
            && a.expires > Instant::now() + TOKEN_MARGIN
        {
            return Ok(a.token.clone());
        }
        let rt = account.refresh_token.clone();
        match oauth::refresh(&self.agent, &self.endpoints.token, &client, &rt) {
            Ok(t) => {
                if self.problems.remove("network").is_some() {
                    self.report();
                }
                let account = self.accounts.get_mut(email).ok_or(CallError::Gone)?;
                if let Some(new_rt) = t.refresh_token
                    && new_rt != rt
                {
                    let _ = self.secrets.set_token(email, &new_rt);
                    account.refresh_token = new_rt;
                }
                account.access = Some(Access {
                    token: t.access_token.clone(),
                    expires: Instant::now() + Duration::from_secs(t.expires_in),
                });
                Ok(t.access_token)
            }
            Err(AuthError::InvalidGrant(description)) => {
                self.remove_account(email, Revoke::LocalOnly);
                let msg = signed_out_message(email, &description);
                self.problems.insert(email.to_string(), msg);
                self.report();
                Err(CallError::Auth)
            }
            Err(AuthError::InvalidClient(m)) => {
                // No account can work with a rejected client.
                self.remove_all_accounts(Revoke::LocalOnly);
                let msg = format!(
                    "Google rejected the OAuth client ({m}). If you saved your own in \
                     Settings, check it; then add your accounts again."
                );
                self.problems.insert("client".into(), msg);
                self.report();
                Err(CallError::Auth)
            }
            Err(AuthError::Network(_) | AuthError::Server(_)) => {
                self.problems
                    .insert("network".into(), NETWORK_PROBLEM.into());
                self.report();
                Err(CallError::Network)
            }
            // Anything else (a 400 other than invalid_grant, an unreadable answer) keeps the
            // account but must not fail silently. The message carries no URL or token.
            Err(e) => {
                self.problems
                    .insert(email.to_string(), format!("{email}: {e}"));
                self.report();
                Err(CallError::Auth)
            }
        }
    }

    /// Runs `f` with a valid token for `email`, refreshing once if Google says it is stale.
    fn with_token<T>(
        &mut self,
        email: &str,
        f: impl Fn(&ureq::Agent, &Endpoints, &str) -> Result<T, ApiError>,
    ) -> Result<T, CallError> {
        let token = self.access_token(email)?;
        match f(&self.agent, &self.endpoints, &token) {
            Err(ApiError::Unauthorized) => {
                if let Some(a) = self.accounts.get_mut(email) {
                    a.access = None;
                }
                let token = self.access_token(email)?;
                f(&self.agent, &self.endpoints, &token).map_err(CallError::Api)
            }
            other => other.map_err(CallError::Api),
        }
    }

    // ----- outcomes -------------------------------------------------------------------------

    /// A call for `email` worked: its backoff starts afresh, and a network problem, or the
    /// account's problem when it came from the failure that backed it off (a rate limit, a
    /// 5xx, a quota), is over.
    fn succeeded(&mut self, email: &str) {
        let mut resolved = false;
        if let Some(account) = self.accounts.get_mut(email)
            && account.backoff != Backoff::default()
        {
            account.backoff.reset();
            resolved |= self.problems.remove(email).is_some();
        }
        resolved |= self.problems.remove("network").is_some();
        if resolved {
            self.report();
        }
    }

    /// Handles a failed call for `email` (SPEC 7.6) and says whether it is worth retrying
    /// after the backoff: rate limits, network failures, 5xx and unreadable answers back off
    /// the account; quota problems wait at least 15 minutes; a refused or missing item is a
    /// problem but the sync carries on; an oversized or missing item is reported by the
    /// caller.
    fn failed(&mut self, email: &str, error: &CallError) -> bool {
        let now = Instant::now();
        let jitter = self.tuning.jitter();
        let tuning = self.tuning;
        let mut backoff = |hint: Option<Duration>, quota: bool| {
            if let Some(account) = self.accounts.get_mut(email) {
                if quota {
                    account.backoff.quota(now, hint);
                } else {
                    account.backoff.failed(now, hint, jitter, &tuning);
                }
            }
        };
        let (key, message, retry) = match error {
            // `access_token` reported these itself.
            CallError::Auth | CallError::Gone => return false,
            CallError::Network => {
                backoff(None, false);
                return true;
            }
            CallError::Api(ApiError::Network(_)) => {
                backoff(None, false);
                ("network".to_string(), NETWORK_PROBLEM.to_string(), true)
            }
            // `classify` already turns these into `QuotaExhausted`; this guards any other
            // source of a rate limit too long to be a burst (SPEC 7.6).
            CallError::Api(
                e @ ApiError::RateLimited {
                    retry_after: Some(hint),
                },
            ) if *hint > QUOTA_WAIT => {
                backoff(Some(*hint), true);
                let quota = ApiError::QuotaExhausted {
                    message: e.to_string(),
                    retry_after: Some(*hint),
                };
                (email.to_string(), format!("{email}: {quota}"), true)
            }
            CallError::Api(e @ ApiError::RateLimited { retry_after }) => {
                backoff(*retry_after, false);
                (email.to_string(), format!("{email}: {e}"), true)
            }
            CallError::Api(e @ (ApiError::Decode(_) | ApiError::Http { status: 500.., .. })) => {
                backoff(None, false);
                (email.to_string(), format!("{email}: {e}"), true)
            }
            CallError::Api(e @ ApiError::QuotaExhausted { retry_after, .. }) => {
                backoff(*retry_after, true);
                (email.to_string(), format!("{email}: {e}"), true)
            }
            CallError::Api(ApiError::NotFound | ApiError::TooLarge) => return false,
            CallError::Api(e) if e.missing_scope() => {
                (email.to_string(), missing_scope_message(email), false)
            }
            CallError::Api(e) => (email.to_string(), format!("{email}: {e}"), false),
        };
        self.problems.insert(key, message);
        self.report();
        retry
    }

    /// Parks `job` on its account until the backoff passes.
    fn defer(&mut self, job: Job) {
        if let Some(account) = self.accounts.get_mut(job.account()) {
            account.defer(job);
        }
    }

    // ----- jobs -----------------------------------------------------------------------------

    /// SPEC 7.3: the history cursor first, then the labels (counts follow in chunks), then
    /// page 1 of the inbox and of the shown view. The first sync never reports new mail. A
    /// repeat after a completed sync (Refresh) keeps the cursor it already has;
    /// a retried first sync takes the new one.
    fn initial_sync(&mut self, email: &str) {
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
    fn refresh_labels(&mut self, email: &str) -> bool {
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
    fn page_one_views(&self, email: &str) -> Vec<ViewKey> {
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
    fn list_view(&mut self, view: ViewKey, more: bool) {
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
    fn list(&mut self, view: &ViewKey, more: bool, mode: PageMode) -> Result<(), CallError> {
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
    fn remember(&mut self, email: &str, dto: &gmail::ThreadDto) -> Option<ThreadSummary> {
        let summary = parse::thread_summary(&email.to_string(), dto)?;
        let account = self.accounts.get_mut(email)?;
        account.missing.remove(&dto.id);
        account.refused.remove(&dto.id);
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
    fn fetch_meta(&mut self, email: &str, ids: Vec<String>) {
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
            }
        }
        if !updated.is_empty() {
            self.emit(SyncEvent::ThreadsUpdated(updated));
        }
        if !gone.is_empty() {
            self.emit(SyncEvent::ThreadsGone {
                account: email.to_string(),
                ids: gone,
            });
        }
    }

    /// One label-count chunk; the counts land as one `LabelCounts` when it ends.
    fn label_counts(&mut self, email: &str, ids: Vec<String>) {
        let mut counted = Vec::new();
        for (i, id) in ids.iter().enumerate() {
            match self.with_token(email, |agent, ep, token| gmail::label(agent, ep, token, id)) {
                Ok(dto) => {
                    self.succeeded(email);
                    let label = parse::label(&email.to_string(), &dto);
                    if let Some(account) = self.accounts.get_mut(email) {
                        account.labels.insert(label.id.clone(), label.clone());
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
    fn open_thread(&mut self, key: ThreadKey, prefetch: bool) {
        let email = key.account.clone();
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
                if let Some(summary) = self.remember(&email, &dto)
                    && before.as_ref() != Some(&summary)
                {
                    self.emit(SyncEvent::ThreadsUpdated(vec![summary]));
                }
                let mut detail = parse::thread_detail(&email, &dto);
                detail.key = key;
                self.emit(SyncEvent::ThreadDetail(detail));
            }
            Err(e) => {
                let error = e.reader_message();
                self.failed(&email, &e);
                if !prefetch {
                    self.emit(SyncEvent::ThreadFailed { key, error });
                }
            }
        }
    }

    /// Queues the count refresh for `ids`, keeping only labels the sidebar shows (a count
    /// for any other would add it to the sidebar), in chunks.
    fn queue_counts(&mut self, email: &str, ids: impl IntoIterator<Item = String>) {
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
    fn history(&mut self, email: &str) {
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
            self.emit(SyncEvent::Synced(Timestamp::now()));
        } else {
            self.queue.push(Job::Refetch(email.to_string()));
        }
    }

    /// SPEC 7.4 step 8: Gmail no longer has the history since our cursor.
    fn history_expired(&mut self, email: &str) {
        let Some(account) = self.accounts.get_mut(email) else {
            return;
        };
        account.reset();
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
    fn refetch(&mut self, email: &str) {
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
    fn finish_round(&mut self, email: &str) {
        let Some(account) = self.accounts.get_mut(email) else {
            return;
        };
        let new_mail = account.take_new_mail();
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
            self.emit(SyncEvent::Synced(Timestamp::now()));
        }
    }

    /// Queues a change, or answers at once for an account that is not signed in.
    fn queue_mutation(&mut self, m: Mutation) {
        if self.accounts.contains_key(&m.account) {
            self.queue.push(Job::Mutate(m));
        } else {
            self.emit(SyncEvent::MutationFinished {
                op: m.op,
                error: Some(SIGNED_OUT.into()),
            });
        }
    }

    /// SPEC 7.5 `Mutate`: thread by thread, stopping at the first failure. Never retried
    /// (bar the one token refresh); the store rolls back on an error. The touched threads'
    /// metadata is then fetched again, which also corrects a change that stopped part-way,
    /// and the counts of the labels involved are refreshed.
    fn mutate(&mut self, m: Mutation) {
        let email = m.account.clone();
        let mut carried: BTreeSet<String> = CHANGE_COUNTS.iter().map(|s| s.to_string()).collect();
        if let Some(account) = self.accounts.get(&email) {
            for id in &m.thread_ids {
                if let Some(k) = account.known.get(id) {
                    carried.extend(k.summary.label_ids.iter().cloned());
                }
            }
        }
        let mut error = None;
        let mut touched = false;
        for id in &m.thread_ids {
            let result = match &m.change {
                Change::Modify(delta) if is_star(delta) => self.star(&email, id),
                Change::Modify(delta) => self.with_token(&email, |agent, ep, token| {
                    gmail::modify_thread(agent, ep, token, id, delta)
                }),
                Change::Trash => self
                    .with_token(&email, |agent, ep, token| {
                        gmail::trash(agent, ep, token, id)
                    })
                    .map(drop),
                Change::Untrash(restore) => self
                    .with_token(&email, |agent, ep, token| {
                        gmail::untrash(agent, ep, token, id)
                    })
                    .and_then(|_| {
                        // Out of Trash already: a failed restore below still needs the
                        // refetch to show the thread as Gmail now has it.
                        touched = true;
                        if restore.is_empty() {
                            return Ok(());
                        }
                        self.with_token(&email, |agent, ep, token| {
                            gmail::modify_thread(agent, ep, token, id, restore)
                        })
                    }),
            };
            match result {
                Ok(()) => {
                    touched = true;
                    self.succeeded(&email);
                }
                Err(e) => {
                    error = Some(change_message(&e));
                    self.failed(&email, &e);
                    break;
                }
            }
        }
        self.emit(SyncEvent::MutationFinished { op: m.op, error });
        if !touched {
            return;
        }
        match &m.change {
            Change::Modify(delta) | Change::Untrash(delta) => {
                carried.extend(delta.add.iter().chain(&delta.remove).cloned());
            }
            Change::Trash => {}
        }
        self.queue.strip_meta(&email, &m.thread_ids);
        self.queue.push_soon(chunked(&m.thread_ids, |ids| {
            Job::FetchMeta(email.clone(), ids)
        }));
        self.queue_counts(&email, carried);
    }

    /// Stars the thread's last message that is not a draft, as Gmail web does. A thread the
    /// worker has not fetched yet is fetched first to find it.
    fn star(&mut self, email: &str, id: &str) -> Result<(), CallError> {
        let known = self
            .accounts
            .get(email)
            .and_then(|a| a.known.get(id))
            .and_then(|k| k.star_target.clone());
        let target = match known {
            Some(target) => target,
            None => {
                let dto = self.with_token(email, |agent, ep, token| {
                    gmail::thread_meta(agent, ep, token, id)
                })?;
                self.remember(email, &dto);
                match star_target(&dto) {
                    Some(target) => target,
                    // Only drafts: star the thread, so the change still lands.
                    None => {
                        return self.with_token(email, |agent, ep, token| {
                            gmail::modify_thread(agent, ep, token, id, &LabelDelta::star())
                        });
                    }
                }
            }
        };
        self.with_token(email, |agent, ep, token| {
            gmail::modify_message(agent, ep, token, &target, &LabelDelta::star())
        })
        .map(drop)
    }

    /// SPEC 7.5 `Send`: build the message and upload it once. Never retried; a failure says
    /// whether the message may have gone out. A sent message shows up through the history
    /// round queued afterwards.
    fn send(&mut self, op: OpId, out: Outgoing) {
        let result = self.deliver(&out);
        let sent = result.is_ok();
        self.emit(SyncEvent::SendFinished {
            op,
            error: result.err(),
        });
        if sent
            && let Some(account) = self.accounts.get(&out.account)
            && account.synced_once
            && account.history_id.is_some()
        {
            self.queue.push(Job::History(out.account.clone()));
        }
    }

    /// SPEC 7.5 `SaveAttachment`: download the attachment afresh, then write it to a new file
    /// in the download folder under a sanitised name. The outcome is a toast.
    fn save(&mut self, d: Download) {
        let name = files::sanitise(&d.attachment.filename);
        let data = match self.attachment_base64(&d.account, &d.message_id, &d.attachment.part_id) {
            Ok(data) => {
                self.succeeded(&d.account);
                data
            }
            Err(e) => {
                let message = attachment_message(&e);
                self.failed(&d.account, &e);
                self.emit(SyncEvent::Notice(format!(
                    "Couldn't save {name}: {message}"
                )));
                return;
            }
        };
        // Bad data in one attachment is that attachment's problem, not the account's: a
        // toast, with no backoff or banner.
        let Ok(bytes) = b64::decode(&data) else {
            self.emit(SyncEvent::Notice(format!(
                "Couldn't save {name}: {ATTACHMENT_UNREADABLE}"
            )));
            return;
        };
        let saved = match &self.downloads {
            Some(dir) => std::fs::create_dir_all(dir).map(|()| dir.clone()),
            None => files::download_dir(),
        }
        .and_then(|dir| files::save_new(&dir, &name, &bytes));
        match saved {
            Ok(path) => self.emit(SyncEvent::Saved(path)),
            Err(e) => {
                eprintln!("eloquawk: couldn't save an attachment: {}", e.kind());
                self.emit(SyncEvent::Notice(format!(
                    "Couldn't save {name}: {}",
                    io_message(&e)
                )));
            }
        }
    }

    /// The base64url data of part `part_id` of message `message_id`, still encoded: decoding
    /// is the caller's, since bad data is no reason to back the account off. The message is
    /// read first for a fresh attachment id, since Gmail's are not stable; a part whose data
    /// is inline (no attachment id) needs no second call. A part that is gone is `NotFound`.
    fn attachment_base64(
        &mut self,
        email: &str,
        message_id: &str,
        part_id: &str,
    ) -> Result<String, CallError> {
        let message = self.with_token(email, |agent, ep, token| {
            gmail::message_full(agent, ep, token, message_id)
        })?;
        let body = message
            .payload
            .as_ref()
            .and_then(|root| find_part(root, part_id))
            .and_then(|part| part.body.clone())
            .ok_or(CallError::Api(ApiError::NotFound))?;
        let data = match (body.attachment_id, body.data) {
            (Some(attachment_id), _) => {
                self.with_token(email, |agent, ep, token| {
                    gmail::attachment(agent, ep, token, message_id, &attachment_id, body.size)
                })?
                .data
            }
            (None, Some(data)) => data,
            (None, None) => return Err(CallError::Api(ApiError::NotFound)),
        };
        Ok(data)
    }

    fn deliver(&mut self, out: &Outgoing) -> Result<(), SendError> {
        let parts = gather_attachments(out, |message_id, part_id| {
            let data = self
                .attachment_base64(&out.account, message_id, part_id)
                .map_err(|e| attachment_message(&e))?;
            b64::decode(&data).map_err(|_| ATTACHMENT_UNREADABLE.to_string())
        })?;
        let message_id = compose::new_message_id();
        let raw =
            compose::build(out, &parts, Timestamp::now(), &message_id).map_err(|e| SendError {
                message: e.to_string(),
                maybe_sent: false,
            })?;
        drop(parts);
        let thread_id = out.thread_id.as_deref();
        self.with_token(&out.account, |agent, ep, token| {
            gmail::send(agent, ep, token, &raw, thread_id)
        })
        .map(drop)
        .map_err(|e| send_error(&e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::google::fake::{FakeServer, Reply};
    use crate::google::gmail::fixtures;
    use crate::secrets::SecretStore;

    const PROFILE: &str = r#"{"emailAddress":"me@example.com","historyId":"42"}"#;
    const TOKEN: &str = r#"{"access_token":"at2","expires_in":3599,"token_type":"Bearer"}"#;

    /// Short timings so the poll and the backoff happen within a test.
    fn fast() -> Tuning {
        Tuning {
            refresh: Duration::from_millis(300),
            page_size: 2,
            backoff_base: Duration::from_millis(30),
            backoff_cap: Duration::from_millis(200),
            jitter_max: Duration::from_millis(5),
        }
    }

    fn client() -> ClientCredentials {
        ClientCredentials {
            client_id: "c.apps.googleusercontent.com".into(),
            client_secret: "cs".into(),
        }
    }

    /// Endpoints on a port nobody listens on, for tests that must not make a request.
    fn nowhere() -> Endpoints {
        let port = std::net::TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let base = format!("http://127.0.0.1:{port}");
        Endpoints {
            base: base.clone(),
            upload: base.clone(),
            token: format!("{base}/token"),
            revoke: format!("{base}/revoke"),
        }
    }

    fn fake_endpoints(server: &FakeServer) -> Endpoints {
        Endpoints {
            base: server.url("/gmail"),
            upload: server.url("/upload"),
            token: server.url("/token"),
            revoke: server.url("/revoke"),
        }
    }

    /// A worker that is not running on a thread, and its event receiver.
    fn worker(
        secrets: Secrets,
        endpoints: Endpoints,
    ) -> (Worker, async_channel::Receiver<SyncEvent>) {
        let (etx, erx) = async_channel::unbounded();
        let (tx, _rx) = mpsc::channel();
        let mut w = Worker::new(secrets, etx, tx, endpoints, Some(client()), fast());
        w.client = Some(client());
        w.auth = AuthState::Ready;
        (w, erx)
    }

    fn drain(rx: &async_channel::Receiver<SyncEvent>) -> Vec<SyncEvent> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    /// Receives events from a running worker until `done` matches one, or 5 s pass.
    fn until(
        rx: &async_channel::Receiver<SyncEvent>,
        done: impl Fn(&SyncEvent) -> bool,
    ) -> Vec<SyncEvent> {
        let deadline = Instant::now() + Duration::from_secs(5);
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

    fn problem(events: &[SyncEvent]) -> Option<String> {
        events.iter().rev().find_map(|e| match e {
            SyncEvent::Problem(p) => Some(p.clone().unwrap_or_default()),
            _ => None,
        })
    }

    fn signing_in(w: &mut Worker) -> u64 {
        w.sign_in_seq += 1;
        w.sign_in = Some(SignIn {
            seq: w.sign_in_seq,
            cancel: Arc::new(AtomicBool::new(false)),
        });
        w.sign_in_seq
    }

    fn tokens(scope: Option<&str>) -> TokenResponse {
        TokenResponse {
            access_token: "at".into(),
            expires_in: 3599,
            refresh_token: Some("rt".into()),
            scope: scope.map(str::to_string),
        }
    }

    #[test]
    fn no_client_at_all_needs_one_and_pasting_saves_it() {
        let secrets = Secrets::memory();
        let (handle, rx) = start_with(secrets.clone(), nowhere(), None, fast(), None).unwrap();
        let events = until(&rx, |e| {
            matches!(e, SyncEvent::Auth(AuthState::NeedsClient))
        });
        assert!(matches!(events[0], SyncEvent::Client(None)));

        // A secret is required for a client id that is not in use.
        handle.send(Command::SaveClient {
            client_id: "pasted.apps.googleusercontent.com".into(),
            client_secret: None,
        });
        let events = until(&rx, |e| matches!(e, SyncEvent::Problem(Some(_))));
        assert!(problem(&events).unwrap().contains("client secret"));
        assert_eq!(secrets.client().unwrap(), None);

        handle.send(Command::SaveClient {
            client_id: " pasted.apps.googleusercontent.com ".into(),
            client_secret: Some(" GOCSPX-pasted ".into()),
        });
        let events = until(&rx, |e| matches!(e, SyncEvent::Auth(AuthState::Ready)));
        assert!(events.iter().any(|e| matches!(
            e,
            SyncEvent::Client(Some(id)) if id == "pasted.apps.googleusercontent.com"
        )));
        assert_eq!(
            secrets.client().unwrap(),
            Some(ClientCredentials {
                client_id: "pasted.apps.googleusercontent.com".into(),
                client_secret: "GOCSPX-pasted".into(),
            })
        );

        // "Use built-in" without a built-in client goes back to asking for one.
        handle.send(Command::ClearClient);
        until(&rx, |e| {
            matches!(e, SyncEvent::Auth(AuthState::NeedsClient))
        });
        assert_eq!(secrets.client().unwrap(), None);
        handle.send(Command::Shutdown);
    }

    #[test]
    fn builtin_client_starts_ready_without_accounts() {
        let (handle, rx) =
            start_with(Secrets::memory(), nowhere(), Some(client()), fast(), None).unwrap();
        let events = until(&rx, |e| matches!(e, SyncEvent::Auth(_)));
        assert!(matches!(events[0], SyncEvent::Client(None)));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, SyncEvent::Accounts(a) if a.is_empty()))
        );
        assert!(matches!(
            events.last(),
            Some(SyncEvent::Auth(AuthState::Ready))
        ));
        handle.send(Command::Shutdown);
    }

    #[test]
    fn prefetch_queues_a_background_job_for_known_accounts() {
        let (mut w, _rx) = worker(Secrets::memory(), nowhere());
        signed_in(&mut w, "me@example.com", "rt");
        w.queue = Queue::default();
        let key = ThreadKey::new("me@example.com", "t");
        w.handle(Command::Prefetch(key.clone()));
        w.handle(Command::Prefetch(ThreadKey::new("gone@example.com", "t")));
        assert!(w.queue.interactive.is_empty());
        assert_eq!(w.queue.background, [Job::Prefetch(key)]);
    }

    #[test]
    fn reads_and_changes_queue_interactive_jobs() {
        let (mut w, rx) = worker(Secrets::memory(), nowhere());
        signed_in(&mut w, "me@example.com", "rt");
        let inbox = ViewKey {
            account: "me@example.com".into(),
            kind: ViewKind::Label("INBOX".into()),
        };
        w.handle(Command::Want {
            view: inbox.clone(),
            more: false,
        });
        w.handle(Command::OpenThread(ThreadKey::new("me@example.com", "t")));
        w.handle(Command::Trash {
            op: 3,
            account: "me@example.com".into(),
            thread_ids: vec!["t".into()],
        });
        assert_eq!(w.want.as_ref(), Some(&inbox));
        assert_eq!(
            w.queue.interactive,
            [
                Job::ListView(inbox, false),
                Job::OpenThread(ThreadKey::new("me@example.com", "t")),
                Job::Mutate(Mutation {
                    op: 3,
                    account: "me@example.com".into(),
                    thread_ids: vec!["t".into()],
                    change: Change::Trash,
                }),
            ]
        );
        // A view, a change or a send for an account that is not signed in answers at once.
        w.handle(Command::Want {
            view: ViewKey {
                account: "gone@example.com".into(),
                kind: ViewKind::AllMail,
            },
            more: false,
        });
        w.handle(Command::Modify {
            op: 4,
            account: "gone@example.com".into(),
            thread_ids: vec!["t".into()],
            delta: LabelDelta::archive(),
        });
        w.handle(Command::Send {
            op: 5,
            outgoing: outgoing("gone@example.com", None),
        });
        let events = drain(&rx);
        assert!(matches!(
            &events[0],
            SyncEvent::ThreadPage { ids, has_more: false, mode: PageMode::Replace, .. } if ids.is_empty()
        ));
        assert!(matches!(
            &events[1],
            SyncEvent::MutationFinished { op: 4, error: Some(e) } if e == SIGNED_OUT
        ));
        assert!(matches!(
            &events[2],
            SyncEvent::SendFinished {
                op: 5,
                error: Some(SendError {
                    maybe_sent: false,
                    ..
                })
            }
        ));
        assert_eq!(w.queue.interactive.len(), 3);
    }

    #[test]
    fn backoff_doubles_up_to_the_cap_and_honours_hints() {
        let t = Tuning::new(60, 50);
        let now = Instant::now();
        let mut b = Backoff::default();
        let mut waits = Vec::new();
        // Each failure comes once the previous wait is over.
        let mut at = now;
        for _ in 0..8 {
            b.failed(at, None, Duration::ZERO, &t);
            waits.push(b.retry_at.unwrap() - at);
            at = b.retry_at.unwrap();
        }
        let secs: Vec<u64> = waits.iter().map(Duration::as_secs).collect();
        assert_eq!(secs, [5, 10, 20, 40, 80, 160, 300, 300]);
        assert!(b.waiting(at - Duration::from_secs(1)));
        assert!(!b.waiting(at));

        // Jitter is added on top; a longer hint wins over the schedule.
        b.reset();
        b.failed(now, None, Duration::from_millis(400), &t);
        assert_eq!(b.retry_at.unwrap() - now, Duration::from_millis(5400));
        b.reset();
        b.failed(now, Some(Duration::from_secs(90)), Duration::ZERO, &t);
        assert_eq!(b.retry_at.unwrap() - now, Duration::from_secs(90));

        // An HTTP-date Retry-After, through the api classification.
        let now_ts: Timestamp = "2015-10-21T07:27:00Z".parse().unwrap();
        let e = api::classify(429, Some("Wed, 21 Oct 2015 07:28:00 GMT"), "{}", now_ts);
        let ApiError::RateLimited { retry_after } = e else {
            panic!("{e:?}");
        };
        b.reset();
        b.failed(now, retry_after, Duration::ZERO, &t);
        assert_eq!(b.retry_at.unwrap() - now, Duration::from_secs(60));

        // Quota: at least 15 minutes, longer when Google says so.
        b.reset();
        b.quota(now, None);
        assert_eq!(b.retry_at.unwrap() - now, QUOTA_WAIT);
        b.quota(now, Some(Duration::from_secs(3600)));
        assert_eq!(b.retry_at.unwrap() - now, Duration::from_secs(3600));
        b.reset();
        assert_eq!(b, Backoff::default());
    }

    #[test]
    fn jitter_stays_below_its_bound() {
        let t = fast();
        for _ in 0..50 {
            assert!(t.jitter() < t.jitter_max);
        }
        let none = Tuning {
            jitter_max: Duration::ZERO,
            ..t
        };
        assert_eq!(none.jitter(), Duration::ZERO);
    }

    #[test]
    fn background_jobs_in_backoff_are_parked_then_requeued() {
        let (mut w, rx) = worker(Secrets::memory(), nowhere());
        signed_in(&mut w, "me@example.com", "rt");
        let now = Instant::now();
        let retry_at = now + Duration::from_secs(20);
        w.accounts
            .get_mut("me@example.com")
            .unwrap()
            .backoff
            .retry_at = Some(retry_at);
        let chunk = Job::FetchMeta("me@example.com".into(), vec!["a".into(), "b".into()]);
        w.queue.push(chunk.clone());
        w.queue
            .push(Job::Labels("me@example.com".into(), vec!["INBOX".into()]));
        w.step(now);
        w.step(now);
        assert!(w.queue.is_empty());
        assert!(drain(&rx).is_empty(), "nothing ran");
        let parked = &w.accounts["me@example.com"].deferred;
        assert_eq!(parked.len(), 2);
        assert_eq!(parked[0], chunk);
        // The loop wakes for the retry, not later (and never sleeps past 30 s).
        w.next_poll = now + Duration::from_secs(3600);
        assert_eq!(w.wait(now), Duration::from_secs(20));

        w.requeue_deferred(now + Duration::from_secs(5));
        assert!(w.queue.is_empty(), "still waiting");
        w.requeue_deferred(retry_at);
        assert_eq!(w.queue.background.front(), Some(&chunk));
        assert_eq!(w.queue.background.len(), 2);
        assert!(w.accounts["me@example.com"].deferred.is_empty());
        assert_eq!(w.wait(now), Duration::ZERO);
    }

    #[test]
    fn queue_coalesces_and_runs_interactive_jobs_first() {
        let a = || "me@example.com".to_string();
        let key = ThreadKey::new("me@example.com", "t");
        let view = ViewKey {
            account: a(),
            kind: ViewKind::AllMail,
        };
        let mut q = Queue::default();
        q.push(Job::InitialSync(a()));
        q.push(Job::Labels(a(), vec!["INBOX".into()]));
        q.push(Job::InitialSync(a()));
        q.push(Job::OpenThread(key.clone()));
        q.push(Job::ListView(view.clone(), false));
        q.push(Job::ListView(view.clone(), false));
        q.push_soon(vec![
            Job::FetchMeta(a(), vec!["1".into()]),
            Job::FetchMeta(a(), vec!["2".into()]),
        ]);
        q.push_soon(vec![Job::FetchMeta(a(), vec!["1".into()])]);
        q.push(Job::History("other@example.com".into()));
        assert_eq!(
            q.queued_meta("me@example.com"),
            HashSet::from(["1".to_string(), "2".to_string()])
        );
        let order: Vec<Job> = std::iter::from_fn(|| q.pop()).collect();
        assert_eq!(
            order,
            [
                Job::OpenThread(key.clone()),
                Job::ListView(view.clone(), false),
                Job::FetchMeta(a(), vec!["1".into()]),
                Job::FetchMeta(a(), vec!["2".into()]),
                Job::InitialSync(a()),
                Job::Labels(a(), vec!["INBOX".into()]),
                Job::History("other@example.com".into()),
            ]
        );

        // A prefetch waits in the background lane, coalesced, behind an open.
        q.push(Job::Prefetch(key.clone()));
        q.push(Job::Prefetch(key.clone()));
        q.push(Job::OpenThread(key.clone()));
        assert_eq!(q.pop(), Some(Job::OpenThread(key.clone())));
        assert_eq!(q.pop(), Some(Job::Prefetch(key.clone())));
        assert!(q.is_empty());

        q.push(Job::OpenThread(key));
        q.push(Job::InitialSync(a()));
        q.push(Job::History("other@example.com".into()));
        q.forget("me@example.com");
        assert_eq!(q.pop(), Some(Job::History("other@example.com".into())));
        assert!(q.is_empty());
    }

    #[test]
    fn long_id_lists_are_chunked_by_ten() {
        let ids: Vec<String> = (0..23).map(|i| i.to_string()).collect();
        let jobs = chunked(&ids, |c| Job::FetchMeta("a".into(), c));
        let sizes: Vec<usize> = jobs
            .iter()
            .map(|j| match j {
                Job::FetchMeta(_, c) => c.len(),
                _ => 0,
            })
            .collect();
        assert_eq!(sizes, [10, 10, 3]);
    }

    #[test]
    fn the_poll_queues_initial_sync_until_one_completes() {
        let (mut w, _rx) = worker(Secrets::memory(), nowhere());
        signed_in(&mut w, "me@example.com", "rt");
        let now = Instant::now();
        let poll = |w: &mut Worker| {
            w.queue = Queue::default();
            w.poll(now);
            std::iter::from_fn(|| w.queue.pop()).collect::<Vec<Job>>()
        };
        // Never synced (say `profile` failed): initial sync again.
        assert_eq!(poll(&mut w), [Job::InitialSync("me@example.com".into())]);
        assert_eq!(w.next_poll, now + w.tuning.refresh);
        // A cursor but no completed first sync (listing failed): still initial sync.
        w.accounts.get_mut("me@example.com").unwrap().history_id = Some(7);
        assert_eq!(poll(&mut w), [Job::InitialSync("me@example.com".into())]);
        // After the first sync: a history round, plus the skeleton rows a chunk left behind.
        {
            let account = w.accounts.get_mut("me@example.com").unwrap();
            account.synced_once = true;
            account.missing.insert("t9".into(), 1);
        }
        assert_eq!(
            poll(&mut w),
            [
                Job::History("me@example.com".into()),
                Job::FetchMeta("me@example.com".into(), vec!["t9".into()]),
            ]
        );
        // An account in backoff is left alone.
        w.accounts
            .get_mut("me@example.com")
            .unwrap()
            .backoff
            .retry_at = Some(now + Duration::from_secs(5));
        assert!(poll(&mut w).is_empty());
        // So is everything while the keyring is locked.
        w.accounts
            .get_mut("me@example.com")
            .unwrap()
            .backoff
            .reset();
        w.auth = AuthState::Locked;
        assert!(poll(&mut w).is_empty());
    }

    #[test]
    fn failures_while_waiting_do_not_hurry_the_schedule() {
        let t = Tuning::new(60, 50);
        let now = Instant::now();
        let mut b = Backoff::default();
        b.failed(now, None, Duration::ZERO, &t);
        let first = b.clone();
        // Clicks while offline: the interactive jobs fail again within the wait.
        for i in 1..5 {
            b.failed(now + Duration::from_secs(i), None, Duration::ZERO, &t);
        }
        assert_eq!(b, first, "neither the attempts nor the wait grow");
        // A longer hint still lengthens the wait.
        b.failed(
            now + Duration::from_secs(1),
            Some(Duration::from_secs(30)),
            Duration::ZERO,
            &t,
        );
        assert_eq!(b.retry_at.unwrap() - now, Duration::from_secs(31));
        assert_eq!(b.attempts, 1);
        // Once the wait is over the schedule moves on as before.
        b.failed(now + Duration::from_secs(31), None, Duration::ZERO, &t);
        assert_eq!(b.attempts, 2);
        assert_eq!(b.retry_at.unwrap() - now, Duration::from_secs(41));
        // A quota problem never shortens a longer wait.
        b.quota(now, Some(Duration::from_secs(3600)));
        b.quota(now, None);
        assert_eq!(b.retry_at.unwrap() - now, Duration::from_secs(3600));
    }

    #[test]
    fn long_rate_limits_are_quota_problems_that_wait_for_the_hint() {
        let (mut w, _rx) = worker(Secrets::memory(), nowhere());
        signed_in(&mut w, "me@example.com", "rt");
        let hour = Duration::from_secs(3600);
        let wait = |w: &Worker| {
            w.accounts["me@example.com"]
                .backoff
                .retry_at
                .unwrap()
                .saturating_duration_since(Instant::now())
        };

        // As `classify` reports it: the Google message and the hint.
        let e = api::classify(
            429,
            Some("3600"),
            r#"{"error":{"message":"slow"}}"#,
            Timestamp::now(),
        );
        assert!(w.failed("me@example.com", &CallError::Api(e)));
        assert!(
            wait(&w) > hour - Duration::from_secs(5),
            "the hint, not 15 minutes"
        );
        assert_eq!(
            w.problems["me@example.com"],
            "me@example.com: Gmail limit reached: slow"
        );

        // A long `RateLimited` from anywhere else gets the same treatment.
        w.accounts
            .get_mut("me@example.com")
            .unwrap()
            .backoff
            .reset();
        w.problems.clear();
        let e = ApiError::RateLimited {
            retry_after: Some(QUOTA_WAIT + Duration::from_secs(60)),
        };
        assert!(w.failed("me@example.com", &CallError::Api(e)));
        assert!(wait(&w) > QUOTA_WAIT);
        assert!(w.problems["me@example.com"].contains("Gmail limit reached"));
        assert_eq!(w.accounts["me@example.com"].backoff.attempts, 1);

        // A short one is a burst.
        w.accounts
            .get_mut("me@example.com")
            .unwrap()
            .backoff
            .reset();
        let e = ApiError::RateLimited {
            retry_after: Some(Duration::from_secs(2)),
        };
        assert!(w.failed("me@example.com", &CallError::Api(e)));
        assert!(wait(&w) < Duration::from_secs(3));
        assert!(w.problems["me@example.com"].contains("slow down"));
    }

    #[test]
    fn the_poll_skips_ids_already_queued_or_parked() {
        let (mut w, _rx) = worker(Secrets::memory(), nowhere());
        signed_in(&mut w, "me@example.com", "rt");
        let a = || "me@example.com".to_string();
        let now = Instant::now();
        {
            let account = w.accounts.get_mut("me@example.com").unwrap();
            account.synced_once = true;
            account.history_id = Some(1);
            for id in ["t1", "t2", "t3"] {
                account.missing.insert(id.into(), 1);
            }
            account.defer(Job::FetchMeta(a(), vec!["t1".into()]));
        }
        w.queue.push(Job::FetchMeta(a(), vec!["t2".into()]));
        w.poll(now);
        let jobs: Vec<Job> = std::iter::from_fn(|| w.queue.pop()).collect();
        assert_eq!(
            jobs,
            [
                Job::FetchMeta(a(), vec!["t2".into()]),
                Job::History(a()),
                Job::FetchMeta(a(), vec!["t3".into()]),
            ]
        );

        // A listing takes its ids out of queued chunks and queues them at the front.
        let mut q = Queue::default();
        q.push(Job::FetchMeta(a(), vec!["1".into(), "2".into()]));
        q.push(Job::FetchMeta(a(), vec!["3".into()]));
        q.push(Job::FetchMeta("other".into(), vec!["3".into()]));
        q.strip_meta(&a(), &["2".into(), "3".into()]);
        assert_eq!(
            std::iter::from_fn(|| q.pop()).collect::<Vec<Job>>(),
            [
                Job::FetchMeta(a(), vec!["1".into()]),
                Job::FetchMeta("other".into(), vec!["3".into()]),
            ]
        );
    }

    #[test]
    fn refused_threads_wait_for_a_new_history_id() {
        let listed = |h: u32| format!(r#"{{"threads":[{{"id":"t1","historyId":"{h}"}}]}}"#);
        let server = FakeServer::start(vec![
            Reply::json(200, &listed(5)).for_path("/gmail/threads"),
            Reply::json(403, r#"{"error":{"code":403,"message":"nope"}}"#)
                .for_path("/gmail/threads/t1"),
            Reply::json(200, &listed(5)).for_path("/gmail/threads"),
            Reply::json(200, &listed(6)).for_path("/gmail/threads"),
        ]);
        let (mut w, _rx) = worker(Secrets::memory(), fake_endpoints(&server));
        let access = Access {
            token: "at".into(),
            expires: Instant::now() + Duration::from_secs(3600),
        };
        w.accounts.insert(
            "me@example.com".into(),
            AccountState::new("rt".into(), Some(access)),
        );
        let view = ViewKey {
            account: "me@example.com".into(),
            kind: ViewKind::AllMail,
        };
        let meta = |ids: &[&str]| {
            Job::FetchMeta(
                "me@example.com".into(),
                ids.iter().map(|s| s.to_string()).collect(),
            )
        };

        assert!(w.list(&view, false, PageMode::Replace).is_ok());
        assert_eq!(w.queue.pop(), Some(meta(&["t1"])));
        w.fetch_meta("me@example.com", vec!["t1".into()]);
        let account = &w.accounts["me@example.com"];
        assert!(account.missing.is_empty());
        assert_eq!(account.refused.get("t1"), Some(&5));
        assert!(account.deferred.is_empty(), "a refusal is not retried");

        // Neither the poll nor listing the same thread again asks for it.
        w.accounts.get_mut("me@example.com").unwrap().synced_once = true;
        w.accounts.get_mut("me@example.com").unwrap().history_id = Some(1);
        w.poll(Instant::now());
        assert_eq!(w.queue.pop(), Some(Job::History("me@example.com".into())));
        assert!(w.queue.is_empty());
        assert!(w.list(&view, false, PageMode::Refresh).is_ok());
        assert!(w.queue.is_empty());

        // Once it changes it is fetched again.
        assert!(w.list(&view, false, PageMode::Refresh).is_ok());
        assert_eq!(w.queue.pop(), Some(meta(&["t1"])));
        assert!(w.accounts["me@example.com"].refused.is_empty());
        server.finish();
    }

    #[test]
    fn searches_left_behind_drop_their_page_tokens() {
        let (mut w, _rx) = worker(Secrets::memory(), nowhere());
        signed_in(&mut w, "me@example.com", "rt");
        let view = |kind: ViewKind| ViewKey {
            account: "me@example.com".into(),
            kind,
        };
        let search = ViewKind::Search("from:a".into());
        {
            let pages = &mut w.accounts.get_mut("me@example.com").unwrap().pages;
            pages.insert(ViewKind::AllMail, Some("p".into()));
            pages.insert(search.clone(), Some("q".into()));
            pages.insert(ViewKind::Search("old".into()), None);
        }
        w.handle(Command::Want {
            view: view(search.clone()),
            more: false,
        });
        let pages = &w.accounts["me@example.com"].pages;
        assert_eq!(pages.len(), 2);
        assert!(
            pages.contains_key(&search),
            "the shown search keeps its token"
        );
        // Its next page keeps it too; moving away drops it.
        w.handle(Command::Want {
            view: view(search.clone()),
            more: true,
        });
        assert!(w.accounts["me@example.com"].pages.contains_key(&search));
        w.handle(Command::Want {
            view: view(ViewKind::AllMail),
            more: false,
        });
        let pages = &w.accounts["me@example.com"].pages;
        assert_eq!(pages.keys().collect::<Vec<_>>(), [&ViewKind::AllMail]);
    }

    #[test]
    fn refresh_resyncs_and_keeps_following_the_history() {
        let (mut w, _rx) = worker(Secrets::memory(), nowhere());
        signed_in(&mut w, "me@example.com", "rt");
        signed_in(&mut w, "new@example.com", "rt");
        {
            let a = w.accounts.get_mut("me@example.com").unwrap();
            a.synced_once = true;
            a.history_id = Some(7);
            a.backoff.retry_at = Some(Instant::now() + Duration::from_secs(60));
        }
        w.handle(Command::Refresh);
        let jobs: Vec<Job> = std::iter::from_fn(|| w.queue.pop()).collect();
        assert_eq!(
            jobs,
            [
                Job::InitialSync("me@example.com".into()),
                Job::History("me@example.com".into()),
                Job::InitialSync("new@example.com".into()),
            ]
        );
        assert_eq!(w.accounts["me@example.com"].backoff, Backoff::default());
    }

    #[test]
    fn tune_moves_the_next_poll() {
        let (mut w, _rx) = worker(Secrets::memory(), nowhere());
        let last = w.last_poll;
        w.handle(Command::Tune {
            refresh_secs: 900,
            page_size: 25,
        });
        assert_eq!(w.tuning.page_size, 25);
        assert_eq!(w.next_poll, last + Duration::from_secs(900));
    }

    #[test]
    fn signing_out_drops_the_accounts_jobs() {
        let (mut w, _rx) = worker(Secrets::memory(), nowhere());
        signed_in(&mut w, "me@example.com", "rt");
        w.queue.push(Job::InitialSync("me@example.com".into()));
        w.queue
            .push(Job::OpenThread(ThreadKey::new("me@example.com", "t")));
        assert!(w.remove_account("me@example.com", Revoke::LocalOnly));
        assert!(w.queue.is_empty());
    }

    const A: &str = "18c0a1b2c3d4e5f6";
    const B: &str = "18c0a1b2c3d4e5f7";

    /// The metadata fixture, renamed to thread `id`.
    fn meta(id: &str) -> String {
        fixtures::THREAD_META.replace(A, id)
    }

    fn query(r: &crate::google::fake::Recorded) -> Vec<(String, String)> {
        let q = r.target.split_once('?').map(|(_, q)| q).unwrap_or_default();
        url::form_urlencoded::parse(q.as_bytes())
            .into_owned()
            .collect()
    }

    fn has(q: &[(String, String)], k: &str, v: &str) -> bool {
        q.iter().any(|(a, b)| a == k && b == v)
    }

    /// SPEC 12.2 `fake_gmail_read_path`: the real worker thread against a scripted Gmail.
    #[test]
    fn fake_gmail_read_path() {
        let labels: gmail::LabelList = serde_json::from_str(fixtures::LABELS).unwrap();
        let mut script = vec![
            Reply::json(200, TOKEN).for_path("/token"),
            // The first profile fails; the poll queues the initial sync again.
            Reply::json(500, r#"{"error":{"code":500,"message":"Backend Error"}}"#)
                .for_path("/gmail/profile"),
            Reply::json(200, fixtures::PROFILE).for_path("/gmail/profile"),
            // Later polls find no history since the cursor.
            Reply::json(200, fixtures::HISTORY_EMPTY)
                .for_path("/gmail/history")
                .sticky(),
            Reply::json(200, fixtures::LABELS).for_path("/gmail/labels"),
            Reply::json(200, fixtures::THREADS).for_path("/gmail/threads"),
            Reply::json(200, r#"{"threads":[{"id":"t3","historyId":"9"}]}"#)
                .for_path("/gmail/threads"),
            Reply::json(200, &meta(A)).for_path(&format!("/gmail/threads/{A}")),
            Reply::json(
                429,
                r#"{"error":{"code":429,"message":"Too many concurrent requests for user"}}"#,
            )
            .with_header("Retry-After", "0")
            .for_path(&format!("/gmail/threads/{B}")),
            Reply::json(200, &meta(B)).for_path(&format!("/gmail/threads/{B}")),
            Reply::json(200, &meta("t3")).for_path("/gmail/threads/t3"),
            Reply::json(200, fixtures::THREAD_FULL).for_path(&format!("/gmail/threads/{A}")),
        ];
        for l in &labels.labels {
            let body = if l.id == "INBOX" {
                fixtures::LABEL_INBOX.to_string()
            } else {
                format!(
                    r#"{{"id":"{}","name":"{}","threadsUnread":1,"threadsTotal":2,"messagesTotal":3}}"#,
                    l.id, l.name
                )
            };
            script.push(
                Reply::json(200, &body)
                    .for_path(&format!("/gmail/labels/{}", l.id))
                    .sticky(),
            );
        }
        let server = FakeServer::start(script);
        let secrets = Secrets::memory();
        secrets.set_token("me@example.com", "rt").unwrap();
        let (handle, rx) = start_with(
            secrets,
            fake_endpoints(&server),
            Some(client()),
            fast(),
            None,
        )
        .unwrap();

        // Initial sync, after one failed attempt: labels, page 1 with skeletons, metadata
        // (one chunk deferred by a 429, not lost), then the label counts.
        let mut events = until(&rx, |e| matches!(e, SyncEvent::LabelCounts { .. }));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, SyncEvent::Problem(Some(p)) if p.contains("HTTP 500"))),
            "the failed profile is reported"
        );
        assert_eq!(
            problem(&events).as_deref(),
            Some(""),
            "and cleared by the sync"
        );
        let labels_ev = events
            .iter()
            .find_map(|e| match e {
                SyncEvent::Labels { labels, .. } => Some(labels.clone()),
                _ => None,
            })
            .unwrap();
        assert!(labels_ev.iter().any(|l| l.id == "Label_1"));
        let page = events
            .iter()
            .find_map(|e| match e {
                SyncEvent::ThreadPage {
                    ids,
                    threads,
                    mode,
                    has_more,
                    ..
                } => Some((ids.clone(), threads.len(), *mode, *has_more)),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            page,
            (
                vec![A.to_string(), B.to_string()],
                0,
                PageMode::Replace,
                true
            )
        );
        let updated: Vec<Vec<String>> = events
            .iter()
            .filter_map(|e| match e {
                SyncEvent::ThreadsUpdated(t) => Some(t.iter().map(|s| s.key.id.clone()).collect()),
                _ => None,
            })
            .collect();
        assert_eq!(updated, [vec![A.to_string()], vec![B.to_string()]]);
        let SyncEvent::LabelCounts { labels: counts, .. } = events.last().unwrap() else {
            unreachable!()
        };
        let inbox = counts.iter().find(|l| l.id == "INBOX").unwrap();
        assert_eq!((inbox.threads_unread, inbox.threads_total), (3, 42));
        assert!(events.iter().any(|e| matches!(e, SyncEvent::Busy(true))));
        assert!(events.iter().any(|e| matches!(e, SyncEvent::Synced(_))));

        // The next page, with the token Gmail gave.
        let inbox_view = ViewKey {
            account: "me@example.com".into(),
            kind: ViewKind::Label("INBOX".into()),
        };
        handle.send(Command::Want {
            view: inbox_view,
            more: true,
        });
        let more = until(
            &rx,
            |e| matches!(e, SyncEvent::ThreadsUpdated(t) if t[0].key.id == "t3"),
        );
        assert!(more.iter().any(|e| matches!(
            e,
            SyncEvent::ThreadPage { ids, mode: PageMode::Append, has_more: false, .. }
                if ids == &vec!["t3".to_string()]
        )));

        // Opening a thread fetches it in full.
        handle.send(Command::OpenThread(ThreadKey::new("me@example.com", A)));
        let opened = until(&rx, |e| matches!(e, SyncEvent::ThreadDetail(_)));
        let Some(SyncEvent::ThreadDetail(detail)) = opened.last() else {
            unreachable!()
        };
        assert_eq!(detail.key, ThreadKey::new("me@example.com", A));
        assert_eq!(detail.messages.len(), 2);
        events.extend(more);
        events.extend(opened);
        handle.send(Command::Shutdown);

        let reqs = server.finish();
        assert_eq!(reqs[0].path(), "/token");
        assert!(
            reqs[0]
                .form()
                .contains(&("refresh_token".into(), "rt".into()))
        );
        assert_eq!(reqs[1].path(), "/gmail/profile");
        assert!(
            reqs[1..]
                .iter()
                .filter(|r| r.path() != "/token")
                .all(|r| r.header("authorization").as_deref() == Some("Bearer at2")),
            "one refresh serves every call"
        );
        assert_eq!(reqs.iter().filter(|r| r.path() == "/token").count(), 1);
        assert_eq!(
            reqs.iter().filter(|r| r.path() == "/gmail/profile").count(),
            2
        );

        let lists: Vec<_> = reqs
            .iter()
            .filter(|r| r.path() == "/gmail/threads")
            .map(query)
            .collect();
        assert_eq!(lists.len(), 2);
        assert!(has(&lists[0], "labelIds", "INBOX") && has(&lists[0], "maxResults", "2"));
        assert!(!lists[0].iter().any(|(k, _)| k == "pageToken"));
        assert!(has(&lists[1], "pageToken", "09876543210"));

        let thread_reqs = |id: &str| -> Vec<Vec<(String, String)>> {
            reqs.iter()
                .filter(|r| r.path() == format!("/gmail/threads/{id}"))
                .map(query)
                .collect()
        };
        let b = thread_reqs(B);
        assert_eq!(b.len(), 2, "the deferred chunk ran again");
        assert!(
            b.iter()
                .all(|q| has(q, "format", "metadata") && has(q, "metadataHeaders", "Subject"))
        );
        let a = thread_reqs(A);
        assert_eq!(a.len(), 2);
        assert!(has(&a[0], "format", "metadata"));
        assert!(has(&a[1], "format", "full"));

        let counted: Vec<&str> = reqs
            .iter()
            .filter_map(|r| r.path().strip_prefix("/gmail/labels/"))
            .collect();
        for shown in ["INBOX", "DRAFT", "SPAM", "Label_1"] {
            assert!(counted.contains(&shown), "{shown} counted");
        }
        for hidden in ["UNREAD", "CHAT", "CATEGORY_PROMOTIONS", "Label_3"] {
            assert!(!counted.contains(&hidden), "{hidden} not counted");
        }
    }

    #[test]
    fn sign_in_names_the_account_from_profile_and_saves_its_token() {
        let server = FakeServer::start(vec![Reply::json(200, PROFILE).for_path("/gmail/profile")]);
        let secrets = Secrets::memory();
        let (mut w, rx) = worker(secrets.clone(), fake_endpoints(&server));
        let seq = signing_in(&mut w);
        // A stale result is dropped.
        w.finish_sign_in(seq + 7, Ok(tokens(None)));
        assert!(drain(&rx).is_empty());

        w.finish_sign_in(seq, Ok(tokens(Some(oauth::GMAIL_MODIFY))));
        let events = drain(&rx);
        assert!(events.iter().any(|e| matches!(
            e,
            SyncEvent::Accounts(a) if a == &vec!["me@example.com".to_string()]
        )));
        assert_eq!(secrets.tokens().unwrap()["me@example.com"], "rt");
        let reqs = server.finish();
        assert_eq!(reqs.len(), 1);
        assert_eq!(
            reqs[0].header("authorization").as_deref(),
            Some("Bearer at")
        );
    }

    #[test]
    fn missing_gmail_scope_saves_nothing_and_revokes_only_an_unknown_grant() {
        // No accounts yet: the new grant cannot belong to a known account, so it is revoked.
        let server = FakeServer::start(vec![Reply::json(200, "{}").for_path("/revoke")]);
        let secrets = Secrets::memory();
        let (mut w, rx) = worker(secrets.clone(), fake_endpoints(&server));
        let seq = signing_in(&mut w);
        w.finish_sign_in(seq, Ok(tokens(Some("openid"))));
        assert!(problem(&drain(&rx)).unwrap().contains("needs permission"));
        assert!(secrets.tokens().unwrap().is_empty());
        assert!(w.accounts.is_empty());
        let reqs = server.finish();
        assert_eq!(reqs.len(), 1, "no profile call, one revoke");
        assert!(reqs[0].form().contains(&("token".into(), "rt".into())));

        // With an account here the grant might be its own: nothing is revoked.
        let (mut w, rx) = worker(secrets.clone(), nowhere());
        w.accounts.insert(
            "old@example.com".into(),
            AccountState::new("old".into(), None),
        );
        let seq = signing_in(&mut w);
        w.finish_sign_in(seq, Ok(tokens(Some("openid"))));
        assert!(problem(&drain(&rx)).unwrap().contains("needs permission"));
        assert_eq!(w.accounts.len(), 1);
    }

    #[test]
    fn invalid_grant_signs_the_account_out_with_a_hint() {
        let server = FakeServer::start(vec![Reply::json(
            400,
            r#"{"error":"invalid_grant","error_description":"Token has been expired or revoked."}"#,
        )]);
        let secrets = Secrets::memory();
        secrets.set_token("me@example.com", "rt").unwrap();
        let (mut w, rx) = worker(secrets.clone(), fake_endpoints(&server));
        w.accounts.insert(
            "me@example.com".into(),
            AccountState::new("rt".into(), None),
        );
        assert!(matches!(
            w.access_token("me@example.com"),
            Err(CallError::Auth)
        ));
        assert!(w.accounts.is_empty());
        assert!(secrets.tokens().unwrap().is_empty());
        assert_eq!(
            problem(&drain(&rx)).unwrap(),
            "Google signed me@example.com out. Add it again. (Test-mode tokens expire after 7 days.)"
        );
        assert_eq!(server.finish().len(), 1, "a local sign-out revokes nothing");
    }

    #[test]
    fn signed_out_hint_depends_on_reauth() {
        assert!(
            signed_out_message("a@b", "reauth related error (invalid_rapt)")
                .ends_with("(Your organisation requires you to sign in again.)")
        );
        assert!(
            signed_out_message("a@b", "Bad Request")
                .ends_with("(Test-mode tokens expire after 7 days.)")
        );
    }

    #[test]
    fn token_endpoint_outages_are_network_problems_that_keep_the_account() {
        let server = FakeServer::start(vec![Reply::json(503, "down"), Reply::json(200, TOKEN)]);
        let (mut w, rx) = worker(Secrets::memory(), fake_endpoints(&server));
        w.accounts.insert(
            "me@example.com".into(),
            AccountState::new("rt".into(), None),
        );
        assert!(matches!(
            w.access_token("me@example.com"),
            Err(CallError::Network)
        ));
        assert_eq!(w.accounts.len(), 1);
        assert_eq!(problem(&drain(&rx)).as_deref(), Some(NETWORK_PROBLEM));
        // The next success clears it.
        assert_eq!(
            w.access_token("me@example.com").ok().as_deref(),
            Some("at2")
        );
        assert_eq!(problem(&drain(&rx)).as_deref(), Some(""));
    }

    #[test]
    fn a_rejected_access_token_is_refreshed_once() {
        let server = FakeServer::start(vec![
            Reply::json(401, "{}").for_path("/gmail/profile"),
            Reply::json(200, TOKEN).for_path("/token"),
            Reply::json(200, PROFILE).for_path("/gmail/profile"),
        ]);
        let (mut w, _rx) = worker(Secrets::memory(), fake_endpoints(&server));
        w.accounts.insert(
            "me@example.com".into(),
            AccountState::new(
                "rt".into(),
                Some(Access {
                    token: "stale".into(),
                    expires: Instant::now() + Duration::from_secs(3000),
                }),
            ),
        );
        let profile = w.with_token("me@example.com", gmail::profile).ok().unwrap();
        assert_eq!(profile.email_address, "me@example.com");
        let reqs = server.finish();
        assert_eq!(
            reqs[0].header("authorization").as_deref(),
            Some("Bearer stale")
        );
        assert_eq!(
            reqs[2].header("authorization").as_deref(),
            Some("Bearer at2")
        );
    }

    #[test]
    fn saving_the_client_in_use_keeps_its_accounts() {
        let secrets = Secrets::memory();
        let (mut w, rx) = worker(secrets.clone(), nowhere());
        w.accounts.insert(
            "me@example.com".into(),
            AccountState::new("rt".into(), None),
        );
        secrets.set_token("me@example.com", "rt").unwrap();
        // The built-in client's id with a secret pasted: same client, so nobody is signed out.
        w.save_client(client().client_id, Some("cs".into()));
        assert_eq!(secrets.client().unwrap(), Some(client()));
        assert_eq!(secrets.tokens().unwrap()["me@example.com"], "rt");
        assert_eq!(w.accounts.len(), 1);
        assert!(
            drain(&rx)
                .iter()
                .any(|e| matches!(e, SyncEvent::Client(Some(_))))
        );
    }

    fn signed_in(w: &mut Worker, email: &str, rt: &str) {
        w.accounts
            .insert(email.into(), AccountState::new(rt.into(), None));
    }

    #[test]
    fn an_unexpected_refresh_error_is_reported_and_keeps_the_account() {
        let server = FakeServer::start(vec![
            Reply::json(
                400,
                r#"{"error":"invalid_request","error_description":"Missing parameter"}"#,
            ),
            Reply::json(200, TOKEN),
        ]);
        let (mut w, rx) = worker(Secrets::memory(), fake_endpoints(&server));
        signed_in(&mut w, "me@example.com", "rt");
        assert!(matches!(
            w.access_token("me@example.com"),
            Err(CallError::Auth)
        ));
        assert_eq!(w.accounts.len(), 1);
        assert_eq!(
            problem(&drain(&rx)).as_deref(),
            Some("me@example.com: Missing parameter")
        );
        // A later success clears it through `check_accounts`' usual path.
        assert!(w.access_token("me@example.com").is_ok());
    }

    #[test]
    fn an_unreadable_token_answer_is_reported_without_its_body() {
        let server = FakeServer::start(vec![Reply::json(
            200,
            r#"{"access_token":"ya29.secret","expires_in":"soon"}"#,
        )]);
        let (mut w, rx) = worker(Secrets::memory(), fake_endpoints(&server));
        signed_in(&mut w, "me@example.com", "rt");
        assert!(w.access_token("me@example.com").is_err());
        let p = problem(&drain(&rx)).unwrap();
        assert!(
            p.starts_with("me@example.com: Google's sign-in service"),
            "{p}"
        );
        assert!(!p.contains("ya29") && !p.contains("soon"), "{p}");
    }

    #[test]
    fn unreadable_tokens_are_reset_without_quoting_them() {
        let store = Arc::new(crate::secrets::MemoryStore::default());
        store
            .set(crate::secrets::TOKENS_USER, r#""1//0g-refresh""#)
            .unwrap();
        let (handle, rx) = start_with(
            Secrets::new(store.clone()),
            nowhere(),
            Some(client()),
            fast(),
            None,
        )
        .unwrap();
        let events = until(&rx, |e| matches!(e, SyncEvent::Problem(Some(_))));
        let p = problem(&events).unwrap();
        assert!(p.starts_with("Stored sign-ins were unreadable"), "{p}");
        assert!(!p.contains("0g-refresh"), "{p}");
        assert_eq!(store.get(crate::secrets::TOKENS_USER).unwrap(), None);
        handle.send(Command::Shutdown);
    }

    #[test]
    fn an_unreadable_client_is_removed_and_points_to_settings() {
        for builtin in [Some(client()), None] {
            let store = Arc::new(crate::secrets::MemoryStore::default());
            store
                .set(crate::secrets::CLIENT_USER, r#"{"client_id":"x"}"#)
                .unwrap();
            store
                .set(crate::secrets::TOKENS_USER, r#"{"me@example.com":"rt"}"#)
                .unwrap();
            let expect = if builtin.is_some() {
                AuthState::Ready
            } else {
                AuthState::NeedsClient
            };
            let (handle, rx) = start_with(
                Secrets::new(store.clone()),
                nowhere(),
                builtin,
                fast(),
                None,
            )
            .unwrap();
            let events = until(&rx, |e| matches!(e, SyncEvent::Problem(Some(_))));
            assert!(
                events
                    .iter()
                    .any(|e| matches!(e, SyncEvent::Auth(a) if *a == expect))
            );
            assert!(
                problem(&events)
                    .unwrap()
                    .contains("set it again in Settings")
            );
            assert_eq!(store.get(crate::secrets::CLIENT_USER).unwrap(), None);
            assert_eq!(store.get(crate::secrets::TOKENS_USER).unwrap(), None);
            handle.send(Command::Shutdown);
        }
    }

    /// A memory store whose client entry cannot be written.
    #[derive(Default)]
    struct ClientWriteFails(crate::secrets::MemoryStore);

    impl SecretStore for ClientWriteFails {
        fn get(&self, user: &str) -> Result<Option<String>, SecretError> {
            self.0.get(user)
        }
        fn set(&self, user: &str, value: &str) -> Result<(), SecretError> {
            if user == crate::secrets::CLIENT_USER {
                return Err(SecretError::Unavailable("locked".into()));
            }
            self.0.set(user, value)
        }
        fn delete(&self, user: &str) -> Result<(), SecretError> {
            self.0.delete(user)
        }
    }

    #[test]
    fn a_failed_client_save_keeps_the_accounts() {
        let store = Arc::new(ClientWriteFails::default());
        let secrets = Secrets::new(store.clone());
        secrets.set_token("me@example.com", "rt").unwrap();
        let (mut w, rx) = worker(secrets.clone(), nowhere());
        signed_in(&mut w, "me@example.com", "rt");
        w.handle(Command::SaveClient {
            client_id: "other.apps.googleusercontent.com".into(),
            client_secret: Some("s".into()),
        });
        let events = drain(&rx);
        assert!(problem(&events).unwrap().contains("locked"));
        // Nothing was signed out or revoked.
        assert!(!events.iter().any(|e| matches!(e, SyncEvent::Notice(_))));
        assert_eq!(w.accounts.len(), 1);
        assert_eq!(w.client, Some(client()));
        assert_eq!(secrets.tokens().unwrap()["me@example.com"], "rt");
    }

    fn outgoing(account: &str, thread_id: Option<&str>) -> Outgoing {
        Outgoing {
            account: account.into(),
            kind: crate::model::ComposeKind::Reply,
            from: crate::model::Address::new(Some("Me"), account),
            to: vec![crate::model::Address::new(Some("Zoë"), "zoe@example.org")],
            cc: vec![],
            bcc: vec![crate::model::Address::new(None, "hidden@example.org")],
            subject: "Re: Café plans".into(),
            body: "See you at noon.\n".into(),
            thread_id: thread_id.map(str::to_string),
            in_reply_to: Some("<m2@mail.example>".into()),
            references: vec!["<m2@mail.example>".into()],
            attachments: vec![],
        }
    }

    fn history_page(json: &str) -> gmail::HistoryList {
        serde_json::from_str(json).unwrap()
    }

    fn set(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn collect_changes_folds_every_kind_and_filters_new_inbox_mail() {
        let fixture = history_page(fixtures::HISTORY);
        let added = |id: &str, labels: &[&str]| {
            format!(
                r#"{{"messagesAdded":[{{"message":{{"id":"x","threadId":"{id}","labelIds":{}}}}}]}}"#,
                serde_json::to_string(labels).unwrap()
            )
        };
        let second = history_page(&format!(
            r#"{{"history":[{},{},{},{},{}],"historyId":"123600"}}"#,
            added("promo", &["INBOX", "UNREAD", "CATEGORY_PROMOTIONS"]),
            added("social", &["INBOX", "UNREAD", "CATEGORY_SOCIAL"]),
            added("self", &["INBOX", "UNREAD", "SENT"]),
            added("read", &["INBOX"]),
            added("personal", &["INBOX", "UNREAD", "CATEGORY_PERSONAL"]),
        ));
        let c = collect_changes(&[fixture, second]);
        assert_eq!(
            c.changed,
            set(&[
                "t-new",
                "18c0a1b2c3d4e5f6",
                "t-old",
                "promo",
                "social",
                "self",
                "read",
                "personal"
            ]),
            "added, relabelled and deleted threads are all refetched"
        );
        assert_eq!(c.fresh_inbox, set(&["t-new", "personal"]));
        assert_eq!(c.labels, set(&["UNREAD"]));
        assert_eq!(c.new_history_id, Some(123600), "the last page's id");

        let empty = collect_changes(&[history_page(fixtures::HISTORY_EMPTY)]);
        assert!(empty.changed.is_empty() && empty.fresh_inbox.is_empty());
        assert_eq!(empty.new_history_id, Some(123456));
        assert_eq!(collect_changes(&[]).new_history_id, None);
    }

    fn summary(id: &str, label_ids: &[&str]) -> ThreadSummary {
        ThreadSummary {
            key: ThreadKey::new("me@example.com", id),
            history_id: 1,
            subject: String::new(),
            snippet: String::new(),
            participants: vec![],
            last_date: Timestamp::UNIX_EPOCH,
            message_count: 1,
            label_ids: label_ids.iter().map(|s| s.to_string()).collect(),
            has_attachments: false,
        }
    }

    fn know(account: &mut AccountState, id: &str, label_ids: &[&str]) {
        account.known.insert(
            id.into(),
            Known {
                history_id: 1,
                star_target: None,
                summary: summary(id, label_ids),
            },
        );
    }

    #[test]
    fn the_cursor_moves_with_its_pending_work_and_failures_keep_it() {
        let mut a = AccountState::new("rt".into(), None);
        a.history_id = Some(5);
        a.commit_round(Changes {
            changed: set(&["a", "b", "c"]),
            fresh_inbox: set(&["c"]),
            labels: set(&["UNREAD"]),
            new_history_id: Some(9),
        });
        assert_eq!(a.history_id, Some(9), "committed with the pending ids");
        // New inbox mail first.
        assert_eq!(a.next_refetch(), ["c", "a", "b"]);
        // `c` lands, `a` is gone, then `b` fails: `b` stays pending for the next round.
        know(&mut a, "c", &["INBOX", "UNREAD"]);
        a.refetch_succeeded("c");
        a.refetch_gone("a");
        assert_eq!(a.pending_changed, set(&["b"]));
        assert_eq!(a.pending_fresh, set(&["c"]), "reported once the round ends");

        // A failed history page commits nothing; an empty round keeps what is pending.
        a.commit_round(Changes {
            new_history_id: Some(11),
            ..Changes::default()
        });
        assert_eq!(a.history_id, Some(11));
        assert_eq!(a.next_refetch(), ["b"]);
        know(&mut a, "b", &["INBOX"]);
        a.refetch_succeeded("b");
        assert!(a.pending_changed.is_empty());
        let new_mail = a.take_new_mail();
        assert_eq!(new_mail.len(), 1);
        assert_eq!(new_mail[0].key.id, "c");
        assert!(a.pending_fresh.is_empty());

        // A candidate that was read meanwhile is cleared without a report.
        a.commit_round(Changes {
            changed: set(&["d"]),
            fresh_inbox: set(&["d"]),
            ..Changes::default()
        });
        know(&mut a, "d", &["INBOX"]);
        a.refetch_succeeded("d");
        assert!(a.take_new_mail().is_empty());
        assert!(a.pending_fresh.is_empty());

        // An expired history drops every cache.
        a.commit_round(Changes {
            changed: set(&["e"]),
            ..Changes::default()
        });
        a.missing.insert("f".into(), 1);
        a.synced_once = true;
        a.reset();
        assert!(a.known.is_empty() && a.pending_changed.is_empty() && a.missing.is_empty());
        assert_eq!((a.history_id, a.synced_once), (None, false));
    }

    #[test]
    fn a_burst_still_refetches_new_inbox_mail() {
        let mut a = AccountState::new("rt".into(), None);
        let ids: Vec<String> = (0..150).map(|i| format!("18c0{i:012x}")).collect();
        for id in &ids {
            know(&mut a, id, &["INBOX"]);
        }
        let fresh: Vec<&str> = ids[100..130].iter().map(String::as_str).collect();
        a.commit_round(Changes {
            changed: ids.iter().cloned().collect(),
            fresh_inbox: set(&fresh),
            ..Changes::default()
        });
        let first = a.next_refetch();
        assert_eq!(a.pending_changed.len(), BURST_FRESH);
        assert_eq!(a.pending_fresh, a.pending_changed);
        // The newest twenty new threads survive; the rest leave the cache.
        let kept: BTreeSet<String> = ids[110..130].iter().cloned().collect();
        assert_eq!(a.pending_changed, kept);
        assert_eq!(first.len(), CHUNK);
        assert!(first.iter().all(|id| kept.contains(id)));
        assert_eq!(a.known.len(), BURST_FRESH);
        assert!(!a.known.contains_key(&ids[0]));
    }

    #[test]
    fn send_errors_say_whether_the_message_may_have_gone() {
        let maybe = |e: ApiError| send_error(&CallError::Api(e)).maybe_sent;
        assert!(maybe(ApiError::Network(api::NetKind::Timeout)));
        assert!(maybe(ApiError::Network(api::NetKind::Io)));
        for status in [500, 502, 503, 504] {
            assert!(maybe(ApiError::Http {
                status,
                body: String::new()
            }));
        }
        assert!(
            maybe(ApiError::Decode("eof".into())),
            "a 2xx we could not read"
        );
        for e in [
            ApiError::Http {
                status: 400,
                body: "Invalid To header".into(),
            },
            ApiError::Http {
                status: 413,
                body: String::new(),
            },
            ApiError::QuotaExhausted {
                message: "sending limit".into(),
                retry_after: None,
            },
            ApiError::Unauthorized,
            ApiError::Forbidden("no".into()),
            ApiError::RateLimited { retry_after: None },
        ] {
            assert!(!maybe(e.clone()), "{e:?}");
        }
        let e = send_error(&CallError::Api(ApiError::Http {
            status: 400,
            body: "Invalid To header".into(),
        }));
        assert!(e.message.contains("Invalid To header"));
        assert_eq!(
            send_error(&CallError::Api(ApiError::Network(api::NetKind::Connect))).message,
            MAYBE_SENT
        );
        // The token could not be refreshed, so nothing went out.
        assert!(!send_error(&CallError::Network).maybe_sent);
        assert!(!send_error(&CallError::Auth).maybe_sent);

        // A Gmail-held attachment that cannot be fetched stops the send before any upload.
        let mut out = outgoing("me@example.com", None);
        out.attachments.push(OutgoingAttachment {
            filename: "a\u{202E}fdp.exe".into(),
            mime_type: "application/pdf".into(),
            size: 1,
            source: AttachmentSource::Gmail {
                message_id: "m".into(),
                part_id: "1".into(),
            },
        });
        let e = gather_attachments(&out, |_, _| Err(ATTACHMENT_GONE.into()))
            .err()
            .unwrap();
        assert_eq!(
            e.message,
            format!("Couldn't attach afdp.exe: {ATTACHMENT_GONE}")
        );
        assert!(!e.maybe_sent);
        // Its declared size counts against the 25 MB before anything is downloaded.
        out.attachments[0].size = MAX_OUTGOING_ATTACHMENTS + 1;
        let e = gather_attachments(&out, |_, _| panic!("not downloaded"))
            .err()
            .unwrap();
        assert_eq!(e.message, compose::TOO_LARGE_TO_SEND);
        // The fetched bytes are used, with their real size.
        out.attachments[0].size = 1;
        let parts = gather_attachments(&out, |m, p| {
            assert_eq!((m, p), ("m", "1"));
            Ok(b"%PDF".to_vec())
        })
        .unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].0.size, 4);
        assert_eq!(parts[0].1, b"%PDF");
    }

    #[test]
    fn auth_network_errors_take_the_backoff_path() {
        let server = FakeServer::start(vec![Reply::json(503, "down").for_path("/token")]);
        let (mut w, rx) = worker(Secrets::memory(), fake_endpoints(&server));
        signed_in(&mut w, "me@example.com", "rt");
        w.fetch_meta("me@example.com", vec!["t1".into(), "t2".into()]);
        let account = &w.accounts["me@example.com"];
        assert!(account.backoff.waiting(Instant::now()), "backed off");
        assert_eq!(account.backoff.attempts, 1);
        assert_eq!(
            account.deferred,
            [Job::FetchMeta(
                "me@example.com".into(),
                vec!["t1".into(), "t2".into()]
            )],
            "the chunk is parked, not lost"
        );
        assert_eq!(w.accounts.len(), 1, "the account stays");
        assert_eq!(problem(&drain(&rx)).as_deref(), Some(NETWORK_PROBLEM));
        assert_eq!(server.finish().len(), 1);
    }

    #[test]
    fn a_failed_restore_after_untrash_still_refetches_the_thread() {
        let server = FakeServer::start(vec![
            Reply::json(200, TOKEN).for_path("/token"),
            Reply::json(200, r#"{"id":"t1"}"#).for_path("/gmail/threads/t1/untrash"),
            Reply::json(500, r#"{"error":{"code":500,"message":"Backend Error"}}"#)
                .for_path("/gmail/threads/t1/modify"),
        ]);
        let (mut w, rx) = worker(Secrets::memory(), fake_endpoints(&server));
        signed_in(&mut w, "me@example.com", "rt");
        w.mutate(Mutation {
            op: 9,
            account: "me@example.com".into(),
            thread_ids: vec!["t1".into(), "t2".into()],
            change: Change::Untrash(LabelDelta::archive().inverse()),
        });
        let failed = drain(&rx).into_iter().any(|e| {
            matches!(
                e,
                SyncEvent::MutationFinished {
                    op: 9,
                    error: Some(_)
                }
            )
        });
        assert!(failed, "the store rolls the change back");
        let jobs: Vec<Job> = std::iter::from_fn(|| w.queue.pop()).collect();
        assert!(
            jobs.iter().any(|j| matches!(
                j,
                Job::FetchMeta(a, ids) if a == "me@example.com" && ids.iter().any(|i| i == "t1")
            )),
            "the half-applied untrash is corrected by a refetch: {jobs:?}"
        );
        let reqs = server.finish();
        assert!(
            reqs.iter().all(|r| !r.path().contains("/t2/")),
            "stops at the failure"
        );
    }

    #[test]
    fn starring_a_thread_of_drafts_stars_the_thread() {
        let drafts = r#"{"id":"td","historyId":"5","messages":[
            {"id":"d1","threadId":"td","labelIds":["DRAFT"],"historyId":"5","internalDate":"1700000000000"}
        ]}"#;
        let server = FakeServer::start(vec![
            Reply::json(200, TOKEN).for_path("/token"),
            Reply::json(200, drafts).for_path("/gmail/threads/td"),
            Reply::json(200, "{}").for_path("/gmail/threads/td/modify"),
        ]);
        let (mut w, rx) = worker(Secrets::memory(), fake_endpoints(&server));
        signed_in(&mut w, "me@example.com", "rt");
        w.mutate(Mutation {
            op: 3,
            account: "me@example.com".into(),
            thread_ids: vec!["td".into()],
            change: Change::Modify(LabelDelta::star()),
        });
        let ok = drain(&rx)
            .into_iter()
            .any(|e| matches!(e, SyncEvent::MutationFinished { op: 3, error: None }));
        assert!(ok, "no misleading 'no longer in Gmail'");
        let reqs = server.finish();
        let modify = reqs
            .iter()
            .find(|r| r.method == "POST" && r.path() == "/gmail/threads/td/modify")
            .expect("the thread is starred");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&modify.body).unwrap(),
            serde_json::json!({"addLabelIds": ["STARRED"]})
        );
    }

    #[test]
    fn a_retried_first_sync_takes_the_new_cursor() {
        for (synced_once, expected) in [(false, 123456), (true, 7)] {
            let server = FakeServer::start(vec![
                Reply::json(200, TOKEN).for_path("/token"),
                Reply::json(200, fixtures::PROFILE).for_path("/gmail/profile"),
                Reply::json(500, r#"{"error":{"code":500,"message":"Backend Error"}}"#)
                    .for_path("/gmail/labels"),
            ]);
            let (mut w, _rx) = worker(Secrets::memory(), fake_endpoints(&server));
            signed_in(&mut w, "me@example.com", "rt");
            {
                let a = w.accounts.get_mut("me@example.com").unwrap();
                a.history_id = Some(7);
                a.synced_once = synced_once;
            }
            w.initial_sync("me@example.com");
            assert_eq!(
                w.accounts["me@example.com"].history_id,
                Some(expected),
                "synced_once = {synced_once}"
            );
            server.finish();
        }
    }

    /// Waits until an event in `all` (old or newly received) matches `done`.
    fn wait_for(
        rx: &async_channel::Receiver<SyncEvent>,
        all: &mut Vec<SyncEvent>,
        done: impl Fn(&SyncEvent) -> bool,
    ) {
        if all.iter().any(&done) {
            return;
        }
        all.extend(until(rx, done));
    }

    /// The parts of a `multipart/related` body, as (content type, bytes).
    fn multipart_parts(content_type: &str, body: &[u8]) -> Vec<(String, Vec<u8>)> {
        let boundary = content_type
            .split_once("boundary=")
            .map(|(_, b)| b.trim_matches('"'))
            .unwrap();
        let delimiter = format!("--{boundary}");
        let text = body.to_vec();
        let mut parts = Vec::new();
        let mut rest = &text[..];
        while let Some(start) = find(rest, delimiter.as_bytes()) {
            rest = &rest[start + delimiter.len()..];
            if rest.starts_with(b"--") {
                break;
            }
            let Some(end) = find(rest, delimiter.as_bytes()) else {
                break;
            };
            let part = &rest[..end];
            let part = part.strip_prefix(b"\r\n").unwrap_or(part);
            let part = part.strip_suffix(b"\r\n").unwrap_or(part);
            let split = find(part, b"\r\n\r\n").unwrap();
            let head = String::from_utf8_lossy(&part[..split]).to_string();
            let ct = head
                .lines()
                .find_map(|l| l.strip_prefix("Content-Type: "))
                .unwrap_or_default()
                .to_string();
            parts.push((ct, part[split + 4..].to_vec()));
            rest = &rest[end..];
        }
        parts
    }

    fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    /// SPEC 12.2 `fake_gmail_write_path`: changes, stars, trash and untrash, history rounds
    /// with new mail, a failed refetch, a deleted thread, an expired history, a stale access
    /// token, and sends, all through the real worker thread against a scripted Gmail.
    #[test]
    fn fake_gmail_write_path() {
        let labels: gmail::LabelList = serde_json::from_str(fixtures::LABELS).unwrap();
        let listed = format!(r#"{{"threads":[{{"id":"{A}","historyId":"123450"}}]}}"#);
        let round_one = r#"{"history":[
            {"messagesAdded":[{"message":{"id":"m9","threadId":"tnew","labelIds":["INBOX","UNREAD"]}}]},
            {"messagesDeleted":[{"message":{"id":"m5","threadId":"t-old"}}]}
        ],"historyId":"200"}"#;
        let ok = "{}";
        // A message with an inline attachment (`body.data`) and one held by `attachmentId`.
        let pdf: &[u8] = b"%PDF-1.4\x00\xff\xfe binary";
        let message_full = format!(
            r#"{{"id":"mfull","threadId":"{A}","payload":{{"partId":"","mimeType":"multipart/mixed","parts":[
                {{"partId":"0","mimeType":"text/plain","body":{{"size":2,"data":"aGk"}}}},
                {{"partId":"1","mimeType":"text/plain","filename":"notes.txt","body":{{"size":11,"data":"aGVsbG8gd29ybGQ"}}}},
                {{"partId":"2","mimeType":"application/pdf","filename":"../report.pdf","body":{{"size":{},"attachmentId":"ATT9"}}}},
                {{"partId":"3","mimeType":"application/octet-stream","filename":"bad.bin","body":{{"size":3,"data":"!!!"}}}}
            ]}}}}"#,
            pdf.len()
        );
        let attachment_body = format!(
            r#"{{"size":{},"data":"{}"}}"#,
            pdf.len(),
            crate::mail::b64::encode(pdf)
        );
        let downloads =
            std::env::temp_dir().join(format!("eloquawk-sync-downloads-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&downloads);
        let mut script = vec![
            Reply::json(200, TOKEN).for_path("/token"),
            // The second history call finds the access token stale: one refresh, one retry.
            Reply::json(200, TOKEN).for_path("/token"),
            Reply::json(200, fixtures::PROFILE)
                .for_path("/gmail/profile")
                .sticky(),
            Reply::json(200, fixtures::LABELS)
                .for_path("/gmail/labels")
                .sticky(),
            Reply::json(200, &listed)
                .for_path("/gmail/threads")
                .sticky(),
            Reply::json(200, &meta(A))
                .for_path(&format!("/gmail/threads/{A}"))
                .sticky(),
            // Archive both threads, then star, trash and untrash with the inbox put back.
            Reply::json(200, ok).for_path(&format!("/gmail/threads/{A}/modify")),
            Reply::json(200, ok).for_path("/gmail/threads/t2/modify"),
            Reply::json(200, &meta("t2")).for_path("/gmail/threads/t2"),
            Reply::json(200, r#"{"id":"m2","threadId":"x","labelIds":["STARRED"]}"#)
                .for_path("/gmail/messages/m2/modify"),
            Reply::json(200, &format!(r#"{{"id":"{A}"}}"#))
                .for_path(&format!("/gmail/threads/{A}/trash")),
            Reply::json(200, &format!(r#"{{"id":"{A}"}}"#))
                .for_path(&format!("/gmail/threads/{A}/untrash")),
            Reply::json(200, ok).for_path(&format!("/gmail/threads/{A}/modify")),
            // Round one: new inbox mail and a deleted message; the new thread's refetch fails.
            Reply::json(200, round_one).for_path("/gmail/history"),
            Reply::json(500, r#"{"error":{"code":500,"message":"Backend Error"}}"#)
                .for_path("/gmail/threads/tnew"),
            // Round two: nothing new, after a stale token; the pending threads are refetched.
            Reply::json(401, r#"{"error":{"code":401,"message":"stale"}}"#)
                .for_path("/gmail/history"),
            Reply::json(200, r#"{"historyId":"200"}"#).for_path("/gmail/history"),
            Reply::json(200, &meta("tnew")).for_path("/gmail/threads/tnew"),
            Reply::json(404, r#"{"error":{"code":404,"message":"Not Found"}}"#)
                .for_path("/gmail/threads/t-old"),
            // Round three: the history expired.
            Reply::json(404, r#"{"error":{"code":404,"message":"Not Found"}}"#)
                .for_path("/gmail/history"),
            Reply::json(200, r#"{"historyId":"123456"}"#)
                .for_path("/gmail/history")
                .sticky(),
            // Sends: a 503 (may have gone), then a success.
            Reply::json(503, r#"{"error":{"code":503,"message":"Unavailable"}}"#)
                .for_path("/upload/messages/send"),
            Reply::json(200, fixtures::SEND).for_path("/upload/messages/send"),
            // Saves and a forward read the message afresh, then the attachment by its id.
            Reply::json(200, &message_full)
                .for_path("/gmail/messages/mfull")
                .sticky(),
            Reply::json(200, &attachment_body)
                .for_path("/gmail/messages/mfull/attachments/ATT9")
                .sticky(),
            Reply::json(200, fixtures::SEND).for_path("/upload/messages/send"),
        ];
        for l in &labels.labels {
            script.push(
                Reply::json(
                    200,
                    &format!(
                        r#"{{"id":"{}","name":"{}","threadsUnread":1,"threadsTotal":2}}"#,
                        l.id, l.name
                    ),
                )
                .for_path(&format!("/gmail/labels/{}", l.id))
                .sticky(),
            );
        }
        let server = FakeServer::start(script);
        let secrets = Secrets::memory();
        secrets.set_token("me@example.com", "rt").unwrap();
        let (handle, rx) = start_with(
            secrets,
            fake_endpoints(&server),
            Some(client()),
            fast(),
            Some(downloads.clone()),
        )
        .unwrap();
        let me = || "me@example.com".to_string();
        let mut all = Vec::new();

        // The first sync, which never reports new mail.
        wait_for(
            &rx,
            &mut all,
            |e| matches!(e, SyncEvent::ThreadsUpdated(t) if t[0].key.id == A),
        );
        let finished = |op: OpId| move |e: &SyncEvent| matches!(e, SyncEvent::MutationFinished { op: o, .. } if *o == op);
        let mutation_error = |all: &[SyncEvent], op: OpId| {
            all.iter().find_map(|e| match e {
                SyncEvent::MutationFinished { op: o, error } if *o == op => Some(error.clone()),
                _ => None,
            })
        };

        handle.send(Command::Modify {
            op: 1,
            account: me(),
            thread_ids: vec![A.into(), "t2".into()],
            delta: LabelDelta::archive(),
        });
        wait_for(&rx, &mut all, finished(1));
        // The authoritative state follows the change.
        wait_for(
            &rx,
            &mut all,
            |e| matches!(e, SyncEvent::ThreadsUpdated(t) if t.iter().any(|s| s.key.id == "t2")),
        );
        handle.send(Command::Modify {
            op: 2,
            account: me(),
            thread_ids: vec![A.into()],
            delta: LabelDelta::star(),
        });
        wait_for(&rx, &mut all, finished(2));
        handle.send(Command::Trash {
            op: 3,
            account: me(),
            thread_ids: vec![A.into()],
        });
        wait_for(&rx, &mut all, finished(3));
        handle.send(Command::Untrash {
            op: 4,
            account: me(),
            thread_ids: vec![A.into()],
            restore: LabelDelta::archive().inverse(),
        });
        wait_for(&rx, &mut all, finished(4));
        for op in 1..=4 {
            assert_eq!(mutation_error(&all, op), Some(None), "op {op}");
        }

        // History rounds: new mail is reported once its refetch finally works, and the deleted
        // thread is gone.
        wait_for(&rx, &mut all, |e| matches!(e, SyncEvent::NewMail { .. }));
        let new_mail: Vec<&Vec<ThreadSummary>> = all
            .iter()
            .filter_map(|e| match e {
                SyncEvent::NewMail { threads, .. } => Some(threads),
                _ => None,
            })
            .collect();
        assert_eq!(new_mail.len(), 1);
        assert_eq!(new_mail[0].len(), 1);
        assert_eq!(new_mail[0][0].key, ThreadKey::new("me@example.com", "tnew"));
        assert!(all.iter().any(|e| matches!(
            e,
            SyncEvent::ThreadsGone { ids, .. } if ids == &vec!["t-old".to_string()]
        )));
        let refresh_after_new_mail = all.iter().any(|e| {
            matches!(
                e,
                SyncEvent::ThreadPage {
                    mode: PageMode::Refresh,
                    ..
                }
            )
        });
        assert!(refresh_after_new_mail, "the round re-lists page 1");

        // The expired history resets the account and syncs it from scratch.
        wait_for(&rx, &mut all, |e| matches!(e, SyncEvent::AccountReset(_)));
        let reset_at = all
            .iter()
            .position(|e| matches!(e, SyncEvent::AccountReset(a) if a == "me@example.com"))
            .unwrap();
        let mut after = all.split_off(reset_at);
        wait_for(&rx, &mut after, |e| {
            matches!(
                e,
                SyncEvent::ThreadPage {
                    mode: PageMode::Replace,
                    ..
                }
            )
        });
        all.extend(after);

        // A send that may have gone out, then one that did.
        handle.send(Command::Send {
            op: 5,
            outgoing: outgoing("me@example.com", Some(A)),
        });
        wait_for(&rx, &mut all, |e| {
            matches!(e, SyncEvent::SendFinished { op: 5, .. })
        });
        handle.send(Command::Send {
            op: 6,
            outgoing: outgoing("me@example.com", Some(A)),
        });
        wait_for(&rx, &mut all, |e| {
            matches!(e, SyncEvent::SendFinished { op: 6, .. })
        });
        let send_result = |op: OpId| send_result_in(&all, op);
        assert_eq!(
            send_result(5),
            Some(Some(SendError {
                message: MAYBE_SENT.into(),
                maybe_sent: true
            }))
        );
        assert_eq!(send_result(6), Some(None));

        // Saves: an inline part, the same part again (never overwritten), and one fetched by
        // its attachment id under a sanitised name.
        let part = |part_id: &str, filename: &str, attachment_id: Option<&str>| Attachment {
            part_id: part_id.into(),
            // A stale id from an earlier fetch: the worker looks up a fresh one.
            attachment_id: attachment_id.map(str::to_string),
            filename: filename.into(),
            mime_type: "application/octet-stream".into(),
            size: 11,
        };
        let mut saved = Vec::new();
        for attachment in [
            part("1", "notes.txt", None),
            part("1", "notes.txt", None),
            part("2", "../report.pdf", Some("stale")),
        ] {
            handle.send(Command::SaveAttachment {
                account: me(),
                message_id: "mfull".into(),
                attachment,
            });
            let events = until(&rx, |e| {
                matches!(e, SyncEvent::Saved(_) | SyncEvent::Notice(_))
            });
            match events.last() {
                Some(SyncEvent::Saved(path)) => saved.push(path.clone()),
                other => panic!("not saved: {other:?}"),
            }
            all.extend(events);
        }
        assert_eq!(
            saved,
            [
                downloads.join("notes.txt"),
                downloads.join("notes (1).txt"),
                downloads.join("report.pdf"),
            ]
        );
        assert_eq!(std::fs::read(&saved[0]).unwrap(), b"hello world");
        assert_eq!(std::fs::read(&saved[1]).unwrap(), b"hello world");
        assert_eq!(std::fs::read(&saved[2]).unwrap(), pdf);
        // A part that is no longer in the message is a toast, and nothing is written.
        handle.send(Command::SaveAttachment {
            account: me(),
            message_id: "mfull".into(),
            attachment: part("9", "gone.bin", None),
        });
        let events = until(&rx, |e| {
            matches!(e, SyncEvent::Saved(_) | SyncEvent::Notice(_))
        });
        assert!(
            matches!(events.last(), Some(SyncEvent::Notice(n)) if n == &format!("Couldn't save gone.bin: {ATTACHMENT_GONE}")),
            "{events:?}"
        );
        all.extend(events);
        // Data that is not base64 is a toast too, without backing the account off: no banner.
        handle.send(Command::SaveAttachment {
            account: me(),
            message_id: "mfull".into(),
            attachment: part("3", "bad.bin", None),
        });
        let events = until(&rx, |e| {
            matches!(e, SyncEvent::Saved(_) | SyncEvent::Notice(_))
        });
        assert!(
            matches!(events.last(), Some(SyncEvent::Notice(n)) if n == &format!("Couldn't save bad.bin: {ATTACHMENT_UNREADABLE}")),
            "{events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, SyncEvent::Problem(Some(_)))),
            "{events:?}"
        );
        assert!(!downloads.join("bad.bin").exists());
        all.extend(events);
        let _ = std::fs::remove_dir_all(&downloads);

        // A forward carrying the original's attachment.
        let mut forward = outgoing("me@example.com", Some(A));
        forward.attachments.push(OutgoingAttachment {
            filename: "report.pdf".into(),
            mime_type: "application/pdf".into(),
            size: pdf.len() as u64,
            source: AttachmentSource::Gmail {
                message_id: "mfull".into(),
                part_id: "2".into(),
            },
        });
        handle.send(Command::Send {
            op: 7,
            outgoing: forward,
        });
        wait_for(&rx, &mut all, |e| {
            matches!(e, SyncEvent::SendFinished { op: 7, .. })
        });
        assert_eq!(send_result_in(&all, 7), Some(None));
        assert_eq!(
            all.iter()
                .filter(|e| matches!(e, SyncEvent::NewMail { .. }))
                .count(),
            1,
            "the sync after the reset reports nothing"
        );
        handle.send(Command::Shutdown);

        let reqs = server.finish();
        let posts = |path: &str| -> Vec<&crate::google::fake::Recorded> {
            reqs.iter()
                .filter(|r| r.method == "POST" && r.path() == path)
                .collect()
        };
        let body = |r: &crate::google::fake::Recorded| -> serde_json::Value {
            serde_json::from_slice(&r.body).unwrap()
        };
        let a_modify = posts(&format!("/gmail/threads/{A}/modify"));
        assert_eq!(a_modify.len(), 2);
        assert_eq!(
            body(a_modify[0]),
            serde_json::json!({"removeLabelIds": ["INBOX"]})
        );
        assert_eq!(
            body(a_modify[1]),
            serde_json::json!({"addLabelIds": ["INBOX"]}),
            "untrash puts the inbox back"
        );
        let t2 = posts("/gmail/threads/t2/modify");
        assert_eq!(t2.len(), 1, "one modify per thread");
        assert_eq!(
            body(t2[0]),
            serde_json::json!({"removeLabelIds": ["INBOX"]})
        );
        let star = posts("/gmail/messages/m2/modify");
        assert_eq!(star.len(), 1, "star goes on the last message");
        assert_eq!(
            body(star[0]),
            serde_json::json!({"addLabelIds": ["STARRED"]})
        );
        assert_eq!(posts(&format!("/gmail/threads/{A}/trash")).len(), 1);
        let untrash_at = reqs
            .iter()
            .position(|r| r.path() == format!("/gmail/threads/{A}/untrash"))
            .unwrap();
        assert_eq!(
            reqs[untrash_at + 1].path(),
            format!("/gmail/threads/{A}/modify"),
            "the restore follows the untrash"
        );

        // History: repeated types from the cursor, and one refresh for the stale token.
        let history: Vec<&crate::google::fake::Recorded> = reqs
            .iter()
            .filter(|r| r.path() == "/gmail/history")
            .collect();
        assert!(history.len() >= 4);
        let q = query(history[0]);
        assert!(has(&q, "startHistoryId", "123456"));
        assert_eq!(q.iter().filter(|(k, _)| k == "historyTypes").count(), 4);
        assert!(
            has(&query(history[1]), "startHistoryId", "200"),
            "cursor moved"
        );
        assert_eq!(reqs.iter().filter(|r| r.path() == "/token").count(), 2);
        let second_token = reqs
            .iter()
            .enumerate()
            .filter(|(_, r)| r.path() == "/token")
            .nth(1)
            .unwrap()
            .0;
        assert_eq!(reqs[second_token - 1].path(), "/gmail/history");
        assert_eq!(reqs[second_token + 1].path(), "/gmail/history");
        assert_eq!(
            reqs[second_token + 1].header("authorization").as_deref(),
            Some("Bearer at2")
        );
        let tnew = reqs
            .iter()
            .filter(|r| r.path() == "/gmail/threads/tnew")
            .count();
        assert_eq!(tnew, 2, "the failed refetch was retried by the next round");
        // The reset's initial sync listed the inbox again.
        assert_eq!(
            reqs.iter().filter(|r| r.path() == "/gmail/profile").count(),
            2
        );

        // Saves and the forward: the message read afresh each time, the attachment by the
        // fresh id (never the stale one), and never for an inline part.
        let full: Vec<_> = reqs
            .iter()
            .filter(|r| r.path() == "/gmail/messages/mfull")
            .collect();
        assert_eq!(
            full.len(),
            6,
            "three saves, a missing part, a bad one and the forward"
        );
        assert!(full.iter().all(|r| has(&query(r), "format", "full")));
        assert_eq!(
            reqs.iter()
                .filter(|r| r.path() == "/gmail/messages/mfull/attachments/ATT9")
                .count(),
            2,
            "the pdf save and the forward"
        );
        assert!(!reqs.iter().any(|r| r.path().contains("stale")));

        // Sends: a multipart upload carrying the thread id and the raw message.
        let sends = posts("/upload/messages/send");
        assert_eq!(sends.len(), 3);
        for send in &sends {
            assert!(has(&query(send), "uploadType", "multipart"));
            let ct = send.header("content-type").unwrap();
            assert!(ct.starts_with("multipart/related; boundary="), "{ct}");
            let parts = multipart_parts(&ct, &send.body);
            assert_eq!(parts.len(), 2);
            assert_eq!(parts[0].0, "application/json; charset=UTF-8");
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&parts[0].1).unwrap(),
                serde_json::json!({"threadId": A})
            );
            assert_eq!(parts[1].0, "message/rfc822");
            let m = mail_parser::MessageParser::default()
                .parse(&parts[1].1[..])
                .unwrap();
            assert_eq!(m.subject(), Some("Re: Café plans"));
            assert_eq!(
                m.in_reply_to().and_then(|l| l.first()),
                Some("m2@mail.example")
            );
            let bcc = m.bcc().and_then(|l| l.first()).and_then(|a| a.address());
            assert_eq!(bcc, Some("hidden@example.org"), "Bcc stays for Gmail");
        }
        let forwarded = multipart_parts(&sends[2].header("content-type").unwrap(), &sends[2].body);
        let m = mail_parser::MessageParser::default()
            .parse(&forwarded[1].1[..])
            .unwrap();
        let parts: Vec<_> = m.attachments().collect();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].attachment_name(), Some("report.pdf"));
        assert!(parts[0].is_content_type("application", "pdf"));
        assert_eq!(&parts[0].decoded()[..], pdf, "the original bytes");
    }

    fn send_result_in(all: &[SyncEvent], op: OpId) -> Option<Option<SendError>> {
        all.iter().find_map(|e| match e {
            SyncEvent::SendFinished { op: o, error } if *o == op => Some(error.clone()),
            _ => None,
        })
    }
}
