//! Everything the UI knows, built only from [`SyncEvent`]s (SPEC 8.1). A plain struct that
//! the app wraps in a GPUI entity, so it is tested here without a window.
//!
//! The store owns the thread lists and their merging, the cursor and selection (always by
//! key, never by index), the reading pane, and optimistic mutations: a change is applied
//! straight away, re-applied over any fresh data that arrives while it is pending, rolled back
//! if the worker reports a failure, and (for the "move" kinds) undoable from the toast.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use jiff::Timestamp;

use crate::ai::jev::{Level, Suggestion, Verdict, required_group_suggestion, select_suggestions};
use crate::ai::prompt::{self, AiInput, Redact};
use crate::ai::{
    AiCommand, AiEvent, AiHandle, CheckRequest, DISCLOSURE_VERSION, KeyState, LabelAsk, Priority,
};
use crate::config::Prefs;
use crate::mail::address::{self, ParticipantsLabel};
use crate::mail::query::Query;
use crate::model::{
    AccountId, Label, LabelDelta, LabelKind, Message, Outgoing, SendAs, ThreadDetail, ThreadKey,
    ThreadSummary, ViewKey, ViewKind, labels,
};
use crate::protocol::{AuthState, Command, OpId, PageMode, SendError, SyncEvent, SyncHandle};

/// How many thread details are kept in memory.
const DETAIL_CACHE: usize = 30;
/// Moving the cursor into this many last rows asks for the next page.
const MORE_THRESHOLD: usize = 10;
/// The label id `last_view` stores for All Mail, which is not a Gmail label.
const ALL_MAIL_ID: &str = "ALL_MAIL";
/// How many AI results are kept; the least recently inserted go first.
const AI_RESULTS: usize = 1000;
/// Background prefetches per account per rolling hour (SPEC 16.11).
pub const BACKGROUND_PER_HOUR: usize = 60;
/// A prefetch whose detail has not landed by then is forgotten.
const PREFETCH_TTL: std::time::Duration = std::time::Duration::from_secs(5 * 60);
const HOUR: std::time::Duration = std::time::Duration::from_secs(3600);

/// Where the data comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Google,
    Demo,
}

/// One loaded thread list.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ListState {
    /// Thread ids in list order.
    pub ids: Vec<String>,
    pub has_more: bool,
    /// A page has been asked for and not yet arrived.
    pub loading: bool,
    /// Pages merged so far; a refresh only updates `has_more` while this is one.
    pages: u32,
}

/// The reading pane's data for one thread.
#[derive(Clone, Debug, PartialEq)]
pub enum DetailState {
    Loading,
    Loaded(ThreadDetail),
    Failed(String),
}

/// What the toast's button does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NoticeAction {
    Undo,
    /// Shows the folder holding a saved attachment, with the file selected (M12).
    OpenFolder(PathBuf),
    /// Opens a link the phishing guard held back (SPEC 16.12.1); the URL stays in memory.
    OpenLink(String),
}

/// A transient message for the toast.
#[derive(Clone, Debug, PartialEq)]
pub struct Notice {
    pub text: String,
    pub action: Option<NoticeAction>,
    pub posted_at: Instant,
}

/// A message's AI answers (SPEC 16.10).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AiResult {
    pub verdict: Option<Verdict>,
    /// Label probabilities by label id (M17).
    pub labels: Option<Vec<(String, f32)>>,
    /// The label fingerprint the labels were asked under; `None` when partial (M17).
    pub label_fp: Option<u64>,
}

/// A message, by account and Gmail message id.
pub type MessageKey = (AccountId, String);

/// What the UI knows of the AI checks (SPEC 16.10). Holds no key and no mail beyond the
/// verdicts.
#[derive(Debug)]
pub struct AiState {
    pub key: KeyState,
    pub paused: Option<String>,
    /// Checks, known cost in USD, and checks whose cost was not reported, this session.
    pub usage: (u32, f64, u32),
    pub results: HashMap<MessageKey, AiResult>,
    /// Insertion order of `results`, for the bound.
    results_order: VecDeque<MessageKey>,
    pub checking: HashSet<MessageKey>,
    /// The failure text and whether a retry may help.
    pub failed: HashMap<MessageKey, (String, bool)>,
    /// Banners dismissed this session, and suggestion rows under `"labels:" + message id`.
    pub dismissed: HashSet<MessageKey>,
    /// Each checked thread's last checked target message.
    pub by_thread: HashMap<ThreadKey, String>,
    pub test: Option<Result<String, String>>,
    /// A key test is waiting for its result.
    pub testing: bool,
    /// Background prefetch times per account, for the hourly cap.
    pub hourly: HashMap<AccountId, VecDeque<Instant>>,
    /// Prefetches sent whose detail has not landed.
    pub prefetching: HashMap<ThreadKey, Instant>,
    /// Bumped on any change for a thread, so the reader re-renders.
    rev: HashMap<ThreadKey, u64>,
    /// Bumped on any global change: the key, a pause, the AI prefs.
    pub ai_rev: u64,
    /// The first `Reload` went out (after the sync worker left `Starting`).
    reloaded: bool,
}

impl Default for AiState {
    fn default() -> Self {
        Self {
            key: KeyState::Unknown,
            paused: None,
            usage: (0, 0.0, 0),
            results: HashMap::new(),
            results_order: VecDeque::new(),
            checking: HashSet::new(),
            failed: HashMap::new(),
            dismissed: HashSet::new(),
            by_thread: HashMap::new(),
            test: None,
            testing: false,
            hourly: HashMap::new(),
            prefetching: HashMap::new(),
            rev: HashMap::new(),
            ai_rev: 0,
            reloaded: false,
        }
    }
}

impl AiState {
    /// Drops everything known about `account`'s messages.
    fn forget_account(&mut self, account: &str) {
        self.results.retain(|(a, _), _| a != account);
        self.results_order.retain(|(a, _)| a != account);
        self.checking.retain(|(a, _)| a != account);
        self.failed.retain(|(a, _), _| a != account);
        self.dismissed.retain(|(a, _)| a != account);
        self.by_thread.retain(|k, _| k.account != account);
        self.prefetching.retain(|k, _| k.account != account);
        self.hourly.remove(account);
        for (k, r) in self.rev.iter_mut() {
            if k.account == account {
                *r += 1;
            }
        }
        self.ai_rev += 1;
    }

    /// Drops every result (the key was removed).
    fn forget_all(&mut self) {
        self.results.clear();
        self.results_order.clear();
        self.checking.clear();
        self.failed.clear();
        self.by_thread.clear();
        self.prefetching.clear();
        self.ai_rev += 1;
    }

    fn bump(&mut self, thread: &ThreadKey) {
        *self.rev.entry(thread.clone()).or_default() += 1;
    }

    /// Inserts a result, merging a labels-only one into the stored verdict, and bounds the map.
    fn insert_result(&mut self, key: MessageKey, result: AiResult) {
        let merged = match self.results.remove(&key) {
            Some(old) => AiResult {
                verdict: result.verdict.or(old.verdict),
                labels: result.labels.or(old.labels),
                label_fp: result.label_fp.or(old.label_fp),
            },
            None => result,
        };
        self.results.insert(key.clone(), merged);
        self.results_order.retain(|k| k != &key);
        self.results_order.push_back(key);
        while self.results.len() > AI_RESULTS {
            let Some(oldest) = self.results_order.pop_front() else {
                break;
            };
            self.results.remove(&oldest);
        }
    }
}

/// The kinds of optimistic mutation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MutationKind {
    Labels(LabelDelta),
    Trash,
    Untrash { restore: LabelDelta },
}

impl MutationKind {
    /// The label change the store applies optimistically. Trash is modelled as adding TRASH
    /// and removing INBOX (SPEC 15 item 8); the worker's refetch corrects it.
    pub fn effective(&self) -> LabelDelta {
        match self {
            MutationKind::Labels(d) => d.clone(),
            MutationKind::Trash => LabelDelta {
                add: vec![labels::TRASH.into()],
                remove: vec![labels::INBOX.into()],
            },
            MutationKind::Untrash { restore } => LabelDelta {
                add: restore.add.clone(),
                remove: std::iter::once(labels::TRASH.to_string())
                    .chain(restore.remove.iter().cloned())
                    .collect(),
            },
        }
    }

    /// The verb for "Couldn't <verb>: <error>".
    fn verb(&self) -> &'static str {
        match self {
            MutationKind::Trash => "move to Trash",
            MutationKind::Untrash { .. } => "restore",
            MutationKind::Labels(d) if *d == LabelDelta::archive() => "archive",
            MutationKind::Labels(d) if *d == LabelDelta::read() => "mark as read",
            MutationKind::Labels(d) if *d == LabelDelta::unread() => "mark as unread",
            MutationKind::Labels(d) if *d == LabelDelta::star() => "star",
            MutationKind::Labels(d) if *d == LabelDelta::unstar() => "unstar",
            MutationKind::Labels(d) if *d == LabelDelta::spam() => "report spam",
            MutationKind::Labels(d) if *d == LabelDelta::not_spam() => "mark as not spam",
            MutationKind::Labels(_) => "change labels",
        }
    }

    /// The undoable toast text, or `None` for kinds that post no undo (read, unread, star).
    fn undo_text(&self, n: usize) -> Option<String> {
        let what = if n == 1 {
            "1 conversation".to_string()
        } else {
            format!("{n} conversations")
        };
        let text = match self {
            MutationKind::Trash => format!("Moved {what} to Trash"),
            MutationKind::Untrash { .. } => format!("Restored {what}"),
            MutationKind::Labels(d) if *d == LabelDelta::archive() => format!("Archived {what}"),
            MutationKind::Labels(d) if *d == LabelDelta::spam() => {
                format!("Marked {what} as spam")
            }
            MutationKind::Labels(d) if *d == LabelDelta::not_spam() => {
                format!("Marked {what} as not spam")
            }
            MutationKind::Labels(d)
                if [
                    LabelDelta::read(),
                    LabelDelta::unread(),
                    LabelDelta::star(),
                    LabelDelta::unstar(),
                ]
                .contains(d) =>
            {
                return None;
            }
            MutationKind::Labels(_) => format!("Changed labels on {what}"),
        };
        Some(text)
    }
}

/// The state of one draft operation, kept until the window that started it has read it.
#[derive(Clone, Debug)]
pub enum DraftOp {
    Saving,
    /// Saved; the draft's id.
    Saved(String),
    Failed(String),
    Loading,
    /// Read for editing: its id and prefill, or why it cannot be edited.
    Loaded(Result<(String, Box<Outgoing>), String>),
}

/// The state of one send.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SendState {
    /// Held back for the undo window; nothing has gone to the worker yet.
    Scheduled,
    Sending,
    Sent,
    Failed(SendError),
}

/// A row of the thread list.
#[derive(Clone, Debug, PartialEq)]
pub enum Row<'a> {
    Thread(&'a ThreadSummary),
    /// The list has the id but its summary has not loaded yet.
    Skeleton(ThreadKey),
}

impl Row<'_> {
    /// The thread this row shows.
    pub fn key(&self) -> &ThreadKey {
        match self {
            Row::Thread(s) => &s.key,
            Row::Skeleton(k) => k,
        }
    }
}

/// One sidebar entry for an account.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SidebarEntry {
    pub view: ViewKind,
    /// The name to show: the system label's English name, or the last path segment of a
    /// nested user label.
    pub name: String,
    /// Indent level for nested user labels.
    pub depth: usize,
    pub count: Option<u32>,
    pub color: Option<(u32, u32)>,
}

/// Every list position each of some threads holds.
type Positions = Vec<(ThreadKey, Vec<(ViewKey, usize)>)>;

/// A thread as it was before a mutation, with every list position it held.
#[derive(Clone, Debug)]
struct Snapshot {
    key: ThreadKey,
    before: ThreadSummary,
    positions: Vec<(ViewKey, usize)>,
    /// The lists whose membership of this thread the mutation changed; a rollback revisits
    /// only these, so fresher membership elsewhere survives it.
    touched: Vec<ViewKey>,
}

#[derive(Clone, Debug)]
struct Pending {
    kind: MutationKind,
    account: AccountId,
    threads: Vec<Snapshot>,
}

/// What `z` / the toast's Undo reverses.
#[derive(Clone, Debug)]
struct UndoAction {
    kind: MutationKind,
    threads: Vec<Snapshot>,
}

/// System labels in sidebar order, with their names and which count they show (SPEC 4.1).
#[derive(Clone, Copy)]
enum Count {
    None,
    Unread,
    Total,
}

const SYSTEM_LABELS: &[(&str, &str, Count)] = &[
    (labels::INBOX, "Inbox", Count::Unread),
    (labels::STARRED, "Starred", Count::None),
    (labels::IMPORTANT, "Important", Count::None),
    (labels::SENT, "Sent", Count::None),
    (labels::DRAFT, "Drafts", Count::Total),
    // All Mail goes here; it is not a label.
    (labels::SPAM, "Spam", Count::Unread),
    (labels::TRASH, "Trash", Count::None),
];

pub struct Store {
    pub mode: Mode,
    sync: SyncHandle,
    ai_handle: AiHandle,
    /// The AI checks (SPEC 16.10).
    pub ai: AiState,
    pub prefs: Prefs,
    pub auth: AuthState,
    pub client_id: Option<String>,
    pub accounts: Vec<AccountId>,
    pub signing_in: Option<String>,
    pub busy: bool,
    pub last_sync: Option<Timestamp>,
    /// How many `Synced` events arrived; the demo script's `expect:syncs=` reads it.
    pub syncs: u32,
    pub problem: Option<String>,
    pub notice: Option<Notice>,
    pub labels: HashMap<AccountId, Vec<Label>>,
    /// Each account's send-as addresses and their signatures, as last fetched.
    pub send_as: HashMap<AccountId, Vec<SendAs>>,
    pub threads: HashMap<ThreadKey, ThreadSummary>,
    pub lists: HashMap<ViewKey, ListState>,
    /// The view the thread list shows.
    pub view: Option<ViewKey>,
    /// The view a search replaced, which the search's ✕ returns to.
    search_return: Option<ViewKey>,
    /// The highlighted row.
    pub cursor: Option<ThreadKey>,
    /// The x-selection.
    pub selected: BTreeSet<ThreadKey>,
    /// The thread in the reading pane.
    pub open: Option<ThreadKey>,
    pub details: HashMap<ThreadKey, DetailState>,
    /// The summary's history id when each detail loaded, to tell whether it is stale.
    detail_history: HashMap<ThreadKey, u64>,
    /// Least recently used first.
    detail_order: VecDeque<ThreadKey>,
    pending: BTreeMap<OpId, Pending>,
    undo: Option<(OpId, UndoAction)>,
    sends: HashMap<OpId, SendState>,
    /// Messages waiting out the undo window, by op; they go to the worker when it ends.
    scheduled: HashMap<OpId, Outgoing>,
    /// Draft saves and loads by op, until their window has read the outcome.
    drafts: HashMap<OpId, DraftOp>,
    /// Draft saves still running whose message was discarded: the account, and the draft's
    /// id from before the save. Their draft is deleted when the save ends.
    discard_saves: HashMap<OpId, (AccountId, Option<String>)>,
    /// The load of the draft the user opened in the Drafts view.
    edit_draft: Option<OpId>,
    /// The Gmail draft behind a send, deleted once the send succeeds.
    send_drafts: HashMap<OpId, (AccountId, String)>,
    /// Sends still in flight whose compose window has closed: their outcome becomes a
    /// notice, since no window is left to show it.
    abandoned: HashSet<OpId>,
    /// The thread each reply or forward went into.
    send_threads: HashMap<OpId, ThreadKey>,
    next_op: OpId,
    new_mail: Vec<ThreadSummary>,
}

/// The toast for a saved attachment: "Saved <file> to <dir>", with the home folder shown as
/// `~`.
fn saved_text(path: &Path) -> String {
    let file = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let dir = path.parent().unwrap_or(Path::new("/"));
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let dir = match home.as_deref().and_then(|h| dir.strip_prefix(h).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Some(rest) => format!("~/{}", rest.display()),
        None => dir.display().to_string(),
    };
    format!("Saved {file} to {dir}")
}

/// Whether a thread with `delta` applied leaves a list of this kind. `all_labels` are the
/// account's labels, to tell whether a search asks for Spam or Trash by `label:` name.
fn removed_by(kind: &ViewKind, delta: &LabelDelta, all_labels: &[Label]) -> bool {
    let adds = |id: &str| delta.add.iter().any(|a| a == id);
    match kind {
        ViewKind::Label(l) => {
            delta.remove.contains(l)
                || (adds(labels::TRASH) && l != labels::TRASH)
                || (adds(labels::SPAM) && l != labels::SPAM)
        }
        ViewKind::AllMail => adds(labels::TRASH) || adds(labels::SPAM),
        // Like Gmail, a search leaves out Spam and Trash unless it asks for them, so moving a
        // result there takes it out. Other label changes leave the results alone until the
        // search runs again, since the query may not even look at labels.
        ViewKind::Search(q) => {
            (adds(labels::TRASH) || adds(labels::SPAM))
                && !Query::parse(q).wants_spam_or_trash(all_labels)
        }
    }
}

/// Whether a thread with `delta` applied (and now labelled `after`) joins a list of this kind.
fn gained_by(kind: &ViewKind, delta: &LabelDelta, after: &ThreadSummary) -> bool {
    let hidden = |except: &str| {
        (after.in_label(labels::TRASH) && except != labels::TRASH)
            || (after.in_label(labels::SPAM) && except != labels::SPAM)
    };
    match kind {
        ViewKind::Label(l) => after.in_label(l) && delta.add.contains(l) && !hidden(l),
        ViewKind::AllMail => {
            delta
                .remove
                .iter()
                .any(|r| r == labels::TRASH || r == labels::SPAM)
                && !hidden("")
        }
        ViewKind::Search(_) => false,
    }
}

/// Labels an untrash should add back so a trashed thread returns where it was: everything it
/// carried except the state Gmail manages itself.
fn restorable(labels_before: &BTreeSet<String>) -> Vec<String> {
    labels_before
        .iter()
        .filter(|l| {
            !matches!(
                l.as_str(),
                labels::TRASH | labels::SPAM | labels::SENT | labels::DRAFT | "CHAT"
            ) && !l.starts_with("CATEGORY_")
        })
        .cloned()
        .collect()
}

/// The `Refresh` merge of SPEC 8.1: the page replaces the old prefix up to the old position
/// of its last id, and the old tail after that is kept minus anything now in the page.
fn merge_refresh(old: &[String], page: &[String]) -> Vec<String> {
    let Some(last) = page.last() else {
        return Vec::new();
    };
    let cut = match old.iter().position(|id| id == last) {
        Some(p) => p + 1,
        None => page.len().min(old.len()),
    };
    let mut seen: HashSet<&str> = HashSet::new();
    let mut out = Vec::with_capacity(page.len() + old.len() - cut);
    for id in page.iter().chain(&old[cut..]) {
        if seen.insert(id) {
            out.push(id.clone());
        }
    }
    out
}

/// The row to open when the open row at `old_ix` left the list: the next remaining row,
/// failing that the previous one.
fn successor(old: &[String], new: &[String], old_ix: usize) -> Option<String> {
    let remaining: HashSet<&str> = new.iter().map(String::as_str).collect();
    old.get(old_ix + 1..)
        .unwrap_or_default()
        .iter()
        .find(|id| remaining.contains(id.as_str()))
        .or_else(|| {
            old[..old_ix.min(old.len())]
                .iter()
                .rev()
                .find(|id| remaining.contains(id.as_str()))
        })
        .cloned()
}

impl Store {
    /// The store the app runs on. In demo mode both demo accounts are opted in to the AI
    /// checks, with background checks on, so every path shows without a click; nothing is
    /// written (SPEC 16.9). Accounts whose consent is for an older disclosure are forgotten
    /// by the AI worker at once.
    pub fn new(sync: SyncHandle, ai: AiHandle, mode: Mode, mut prefs: Prefs) -> Self {
        if mode == Mode::Demo {
            for account in [crate::demo_data::PERSONAL, crate::demo_data::WORK] {
                prefs
                    .ai
                    .accounts
                    .insert(account.to_string(), DISCLOSURE_VERSION);
            }
            prefs.ai.background = true;
        }
        Self::build(sync, ai, mode, prefs)
    }

    fn build(sync: SyncHandle, ai: AiHandle, mode: Mode, prefs: Prefs) -> Self {
        for (account, version) in &prefs.ai.accounts {
            if *version != DISCLOSURE_VERSION {
                ai.send(AiCommand::ForgetAccount(account.clone()));
            }
        }
        Self {
            mode,
            sync,
            ai_handle: ai,
            ai: AiState::default(),
            prefs,
            auth: AuthState::Starting,
            client_id: None,
            accounts: vec![],
            signing_in: None,
            busy: false,
            last_sync: None,
            syncs: 0,
            problem: None,
            notice: None,
            labels: HashMap::new(),
            send_as: HashMap::new(),
            threads: HashMap::new(),
            lists: HashMap::new(),
            view: None,
            search_return: None,
            cursor: None,
            selected: BTreeSet::new(),
            open: None,
            details: HashMap::new(),
            detail_history: HashMap::new(),
            detail_order: VecDeque::new(),
            pending: BTreeMap::new(),
            undo: None,
            sends: HashMap::new(),
            scheduled: HashMap::new(),
            drafts: HashMap::new(),
            edit_draft: None,
            send_drafts: HashMap::new(),
            discard_saves: HashMap::new(),
            abandoned: HashSet::new(),
            send_threads: HashMap::new(),
            next_op: 1,
            new_mail: Vec::new(),
        }
    }

    /// A demo-mode store with ephemeral prefs, talking to whatever holds `sync`'s receiver.
    /// Its AI handle goes nowhere and no account is opted in, so it sends no prefetch.
    #[cfg(test)]
    pub fn for_tests(sync: SyncHandle) -> Self {
        let prefs = Prefs {
            ephemeral: true,
            ..Prefs::default()
        };
        Self::build(sync, AiHandle::detached(), Mode::Demo, prefs)
    }

    /// A store for the AI tests: the mode and (ephemeral) prefs are given, with no demo
    /// opt-in.
    #[cfg(test)]
    pub fn for_tests_with_ai(sync: SyncHandle, ai: AiHandle, mode: Mode, prefs: Prefs) -> Self {
        Self::build(sync, ai, mode, prefs)
    }

    pub fn send_command(&self, cmd: Command) {
        self.sync.send(cmd);
    }

    pub fn send_ai(&self, cmd: AiCommand) {
        self.ai_handle.send(cmd);
    }

    /// Ctrl+R, the header's Refresh and the locked card's Retry: re-lists, retries a locked
    /// keyring, and reloads the OpenRouter key too.
    pub fn refresh(&self) {
        self.send_command(Command::Refresh);
        self.send_ai(AiCommand::Reload);
    }

    /// Signs an account out from Settings. Only this (and removing the client) forgets the
    /// account's AI consent; an account that merely drops out of `Accounts` keeps it.
    pub fn remove_account(&mut self, account: &str) {
        self.send_command(Command::RemoveAccount(account.to_string()));
        if self.prefs.ai.accounts.contains_key(account) {
            self.change_prefs(|p| {
                p.ai.accounts.remove(account);
            });
        }
    }

    /// Removes the saved OAuth client, which signs every account out.
    pub fn clear_client(&mut self) {
        self.send_command(Command::ClearClient);
        if !self.prefs.ai.accounts.is_empty() {
            self.change_prefs(|p| p.ai.accounts.clear());
        }
    }

    /// Changes preferences from Settings: applies `f`, pulls numbers back into range and
    /// saves, except when the prefs are ephemeral (demo mode, tests).
    /// A new refresh interval or page size reaches the worker straight away, and a change to
    /// the AI prefs reaches the AI worker (SPEC 16.9).
    pub fn change_prefs(&mut self, f: impl FnOnce(&mut Prefs)) {
        let before = (self.prefs.refresh_secs, self.prefs.page_size);
        let ai_before = self.prefs.ai.clone();
        f(&mut self.prefs);
        self.prefs = std::mem::take(&mut self.prefs).clamped();
        if !self.prefs.ephemeral {
            self.prefs.save();
        }
        let (refresh_secs, page_size) = (self.prefs.refresh_secs, self.prefs.page_size);
        if (refresh_secs, page_size) != before {
            self.send_command(Command::Tune {
                refresh_secs,
                page_size,
            });
        }
        if self.prefs.ai != ai_before {
            self.ai.ai_rev += 1;
            for (account, version) in &ai_before.accounts {
                let now = self.prefs.ai.accounts.get(account);
                if *version == DISCLOSURE_VERSION && now != Some(&DISCLOSURE_VERSION) {
                    self.ai_handle
                        .send(AiCommand::ForgetAccount(account.clone()));
                    self.ai.forget_account(account);
                }
            }
            if ai_before.background && !self.prefs.ai.background {
                self.ai.prefetching.clear();
            }
            if ai_before.spend_limit_cents != self.prefs.ai.spend_limit_cents {
                self.send_ai(AiCommand::SpendLimit(self.prefs.ai.spend_limit_cents));
            }
            if (ai_before.backend, &ai_before.cloudflare_account_id)
                != (self.prefs.ai.backend, &self.prefs.ai.cloudflare_account_id)
            {
                if ai_before.backend != self.prefs.ai.backend && self.mode != Mode::Demo {
                    self.ai.key = KeyState::Unknown;
                }
                self.ai.forget_all();
                self.send_ai(AiCommand::Backend {
                    backend: self.prefs.ai.backend,
                    account_id: self.prefs.ai.cloudflare_account_id.clone(),
                });
            }
            // An already open message needs a check when consent or label suggestions are
            // enabled; waiting for the user to reopen it makes the feature appear inert.
            if let Some(key) = self.open.clone()
                && ((!ai_before.accounts.contains_key(&key.account)
                    || ai_before.accounts.get(&key.account) != Some(&DISCLOSURE_VERSION))
                    && self.ai_opted_in(&key.account)
                    || !ai_before.suggest_labels && self.prefs.ai.suggest_labels
                    || ai_before.label_guidance != self.prefs.ai.label_guidance
                    || ai_before.required_label_group != self.prefs.ai.required_label_group)
            {
                self.check_open(&key);
            }
        }
    }

    fn alloc_op(&mut self) -> OpId {
        let op = self.next_op;
        self.next_op += 1;
        op
    }

    /// Posts a toast. A toast without Undo replaces any that offered it, so the undo slot
    /// goes too: `z` undoes only while its notice is showing (SPEC 8.1).
    fn post_notice(&mut self, text: String, action: Option<NoticeAction>) {
        if action != Some(NoticeAction::Undo) {
            self.undo = None;
        }
        self.notice = Some(Notice {
            text,
            action,
            posted_at: Instant::now(),
        });
    }

    /// Posts a plain toast from the UI, such as a feature that is not available yet.
    pub fn inform(&mut self, text: impl Into<String>) {
        self.post_notice(text.into(), None);
    }

    /// The link guard's question: a toast whose "Open link" button opens `url`.
    pub fn ask_open_link(&mut self, text: String, url: String) {
        self.post_notice(text, Some(NoticeAction::OpenLink(url)));
    }

    /// Clears the toast (it expired or was dismissed); its undo goes with it.
    pub fn clear_notice(&mut self) {
        self.notice = None;
        self.undo = None;
    }
}

mod ai;
mod events;
mod label_ops;
mod mutations;
mod navigation;
mod queries;
#[cfg(test)]
mod tests;
