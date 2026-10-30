//! Everything the UI knows, built only from [`SyncEvent`]s (SPEC 8.1). A plain struct that
//! the app wraps in a GPUI entity, so it is tested here without a window.
//!
//! The store owns the thread lists and their merging, the cursor and selection (always by
//! key, never by index), the reading pane, and optimistic mutations: a change is applied
//! straight away, re-applied over any fresh data that arrives while it is pending, rolled back
//! if the worker reports a failure, and (for the "move" kinds) undoable from the toast.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Instant;

use jiff::Timestamp;

use crate::ai::jev::{Level, Suggestion, Verdict, select_suggestions};
use crate::ai::prompt::{self, AiInput, Redact};
use crate::ai::{
    AiCommand, AiEvent, AiHandle, CheckRequest, DISCLOSURE_VERSION, KeyState, LabelAsk, Priority,
};
use crate::config::Prefs;
use crate::mail::address::{self, ParticipantsLabel};
use crate::mail::query::Query;
use crate::model::{
    AccountId, Label, LabelDelta, LabelKind, Message, Outgoing, ThreadDetail, ThreadKey,
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

/// The state of one send.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SendState {
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

    // ----- queries -----

    fn current_ids(&self) -> Vec<String> {
        self.view
            .as_ref()
            .and_then(|v| self.lists.get(v))
            .map(|l| l.ids.clone())
            .unwrap_or_default()
    }

    /// The current view's list state.
    pub fn current_list(&self) -> Option<&ListState> {
        self.view.as_ref().and_then(|v| self.lists.get(v))
    }

    /// The current list's rows; ids without a summary are skeletons.
    pub fn visible_rows(&self) -> Vec<Row<'_>> {
        let Some(view) = &self.view else {
            return Vec::new();
        };
        let Some(list) = self.lists.get(view) else {
            return Vec::new();
        };
        list.ids
            .iter()
            .map(|id| {
                let key = ThreadKey::new(view.account.as_str(), id.as_str());
                match self.threads.get(&key) {
                    Some(s) => Row::Thread(s),
                    None => Row::Skeleton(key),
                }
            })
            .collect()
    }

    /// The sidebar entries for an account: the system labels in Gmail's order (with All Mail
    /// after Drafts), then visible user labels sorted by name and nested on `/`.
    pub fn labels_for(&self, account: &str) -> Vec<SidebarEntry> {
        let all = self.labels.get(account).map(Vec::as_slice).unwrap_or(&[]);
        let mut out = Vec::new();
        for (id, name, _) in SYSTEM_LABELS {
            if *id == labels::SPAM {
                out.push(SidebarEntry {
                    view: ViewKind::AllMail,
                    name: "All Mail".into(),
                    depth: 0,
                    count: None,
                    color: None,
                });
            }
            out.push(SidebarEntry {
                view: ViewKind::Label((*id).to_string()),
                name: (*name).to_string(),
                depth: 0,
                count: self.unread_badge(account, id),
                color: None,
            });
        }
        let mut user: Vec<&Label> = all
            .iter()
            .filter(|l| l.kind == LabelKind::User && !l.hidden)
            .collect();
        user.sort_by_key(|l| l.name.to_lowercase());
        let names: HashSet<&str> = user.iter().map(|l| l.name.as_str()).collect();
        for l in user {
            // Nest under the longest existing ancestor; without one, show the full name.
            let mut depth = 0;
            let mut shown = l.name.as_str();
            let mut prefix_end = 0;
            for (i, _) in l.name.match_indices('/') {
                if names.contains(&l.name[..i]) {
                    depth += 1;
                    prefix_end = i + 1;
                }
            }
            if prefix_end > 0 {
                shown = &l.name[prefix_end..];
            }
            out.push(SidebarEntry {
                view: ViewKind::Label(l.id.clone()),
                name: shown.to_string(),
                depth,
                count: self.unread_badge(account, &l.id),
                color: l.color,
            });
        }
        out
    }

    /// The count a sidebar label shows, per SPEC 4.1; `None` when it shows none or it is zero.
    pub fn unread_badge(&self, account: &str, label: &str) -> Option<u32> {
        let l = self.labels.get(account)?.iter().find(|l| l.id == label)?;
        let count = match SYSTEM_LABELS.iter().find(|(id, _, _)| *id == label) {
            Some((_, _, Count::Unread)) => l.threads_unread,
            Some((_, _, Count::Total)) => l.threads_total,
            Some((_, _, Count::None)) => 0,
            None if l.kind == LabelKind::User => l.threads_unread,
            None => 0,
        };
        (count > 0).then_some(count)
    }

    /// The cursor's row index in the current list.
    pub fn cursor_index(&self) -> Option<usize> {
        let cursor = self.cursor.as_ref()?;
        let view = self.view.as_ref()?;
        if cursor.account != view.account {
            return None;
        }
        self.lists
            .get(view)?
            .ids
            .iter()
            .position(|id| *id == cursor.id)
    }

    /// The sender label for a thread row, with the thread's account as "me".
    pub fn participants_label(&self, s: &ThreadSummary) -> ParticipantsLabel {
        address::participants_label(&s.participants, &s.key.account, s.message_count)
    }

    /// What a mutation acts on: the selection, else the open thread, else the cursor row.
    pub fn targets(&self) -> Vec<ThreadKey> {
        if !self.selected.is_empty() {
            self.selected.iter().cloned().collect()
        } else if let Some(open) = &self.open {
            vec![open.clone()]
        } else {
            self.cursor.iter().cloned().collect()
        }
    }

    /// Whether a mutation is waiting for the worker.
    pub fn is_pending(&self, key: &ThreadKey) -> bool {
        self.pending
            .values()
            .any(|p| p.threads.iter().any(|s| s.key == *key))
    }

    pub fn send_state(&self, op: OpId) -> Option<&SendState> {
        self.sends.get(&op)
    }

    /// The compose window that started `op` has seen its outcome: forget it.
    pub fn forget_send(&mut self, op: OpId) {
        self.sends.remove(&op);
    }

    /// The compose window that started `op` closed: a send still in flight reports its
    /// outcome as a notice instead, and a settled one is forgotten.
    pub fn abandon_send(&mut self, op: OpId) {
        if self.sends.get(&op) == Some(&SendState::Sending) {
            self.abandoned.insert(op);
        }
        self.sends.remove(&op);
    }

    /// Hands new-mail summaries to the notifier and forgets them.
    pub fn take_new_mail(&mut self) -> Vec<ThreadSummary> {
        std::mem::take(&mut self.new_mail)
    }

    // ----- navigation -----

    /// Shows a view: asks the worker for page 1, puts the cursor on the first row, clears the
    /// selection and the reading pane, and remembers a label view in `last_view`.
    pub fn set_view(&mut self, view: ViewKey) {
        self.selected.clear();
        self.open = None;
        let list = self.lists.entry(view.clone()).or_default();
        list.loading = true;
        self.cursor = list
            .ids
            .first()
            .map(|id| ThreadKey::new(view.account.as_str(), id.as_str()));
        let remembered = match &view.kind {
            ViewKind::Label(id) => Some(id.clone()),
            ViewKind::AllMail => Some(ALL_MAIL_ID.to_string()),
            ViewKind::Search(_) => None,
        };
        if let Some(id) = remembered {
            let last = Some((view.account.clone(), id));
            if self.prefs.last_view != last {
                self.prefs.last_view = last;
                self.prefs.save();
            }
        }
        self.view = Some(view.clone());
        self.send_command(Command::Want { view, more: false });
    }

    /// Moves the cursor by `delta` rows, clamped to the list. Landing in the last ten rows
    /// asks for the next page when there is one.
    pub fn move_cursor(&mut self, delta: isize) {
        let Some(view) = self.view.clone() else {
            return;
        };
        let ids = self.current_ids();
        if ids.is_empty() {
            return;
        }
        let ix = match self.cursor_index() {
            Some(i) => (i as isize + delta).clamp(0, ids.len() as isize - 1) as usize,
            None => 0,
        };
        self.cursor = Some(ThreadKey::new(view.account.as_str(), ids[ix].as_str()));
        if ix + MORE_THRESHOLD >= ids.len() {
            self.request_more();
        }
    }

    /// Puts the cursor on row `ix` (clamped).
    pub fn set_cursor_index(&mut self, ix: usize) {
        let Some(view) = &self.view else { return };
        let ids = self.current_ids();
        if let Some(id) = ids.get(ix.min(ids.len().saturating_sub(1))) {
            self.cursor = Some(ThreadKey::new(view.account.as_str(), id.as_str()));
        }
    }

    /// Asks for the current view's next page, once.
    pub fn request_more(&mut self) {
        let Some(view) = self.view.clone() else {
            return;
        };
        let Some(list) = self.lists.get_mut(&view) else {
            return;
        };
        if list.has_more && !list.loading {
            list.loading = true;
            self.send_command(Command::Want { view, more: true });
        }
    }

    /// Opens the cursor row in the reading pane.
    pub fn open_cursor(&mut self) {
        if let Some(key) = self.cursor.clone() {
            self.open_thread(key);
        }
    }

    /// Opens a thread: fetches its detail unless a fresh one is cached, and marks it read
    /// when that preference is on (not undoable, no toast). A cached detail is checked by
    /// the AI at once when the conditions of SPEC 16.10 hold.
    pub fn open_thread(&mut self, key: ThreadKey) {
        self.open = Some(key.clone());
        let summary_history = self.threads.get(&key).map(|s| s.history_id);
        let fresh = match self.details.get(&key) {
            Some(DetailState::Loading) => true,
            Some(DetailState::Loaded(_)) => {
                match (self.detail_history.get(&key), summary_history) {
                    (Some(h), Some(s)) => *h >= s,
                    _ => true,
                }
            }
            Some(DetailState::Failed(_)) | None => false,
        };
        if !fresh {
            self.details.insert(key.clone(), DetailState::Loading);
            self.send_command(Command::OpenThread(key.clone()));
        } else {
            self.check_open(&key);
        }
        self.touch_detail(&key);
        let unread = self.threads.get(&key).is_some_and(|s| s.unread());
        if self.prefs.mark_read_on_open && unread {
            self.mutate_keys(
                vec![key],
                MutationKind::Labels(LabelDelta::read()),
                false,
                None,
            );
        }
    }

    /// Closes the reading pane.
    pub fn close_thread(&mut self) {
        self.open = None;
    }

    fn touch_detail(&mut self, key: &ThreadKey) {
        self.detail_order.retain(|k| k != key);
        self.detail_order.push_back(key.clone());
        while self.detail_order.len() > DETAIL_CACHE {
            let Some(oldest) = self.detail_order.pop_front() else {
                break;
            };
            if Some(&oldest) == self.open.as_ref() {
                self.detail_order.push_back(oldest);
                continue;
            }
            self.details.remove(&oldest);
            self.detail_history.remove(&oldest);
        }
    }

    /// Toggles a thread in the x-selection.
    pub fn toggle_select(&mut self, key: ThreadKey) {
        if !self.selected.remove(&key) {
            self.selected.insert(key);
        }
    }

    pub fn toggle_select_cursor(&mut self) {
        if let Some(key) = self.cursor.clone() {
            self.toggle_select(key);
        }
    }

    pub fn clear_selection(&mut self) {
        self.selected.clear();
    }

    /// The account the current view belongs to, else the first account.
    pub fn current_account(&self) -> Option<AccountId> {
        self.view
            .as_ref()
            .map(|v| v.account.clone())
            .or_else(|| self.accounts.first().cloned())
    }

    /// Shows a label (or All Mail) of the current account: the `g` keys.
    pub fn go_to(&mut self, kind: ViewKind) {
        if let Some(account) = self.current_account() {
            self.set_view(ViewKey { account, kind });
        }
    }

    /// Switches to the inbox of the account `delta` places along, wrapping round: `g n` and
    /// `g p`. With one account it shows that account's inbox.
    pub fn switch_account(&mut self, delta: isize) {
        let n = self.accounts.len() as isize;
        if n == 0 {
            return;
        }
        let at = self
            .view
            .as_ref()
            .and_then(|v| self.accounts.iter().position(|a| *a == v.account))
            .map_or(0, |i| i as isize + delta);
        let account = self.accounts[at.rem_euclid(n) as usize].clone();
        self.set_view(ViewKey {
            account,
            kind: ViewKind::Label(labels::INBOX.into()),
        });
    }

    // ----- search -----

    /// Shows the results of `query` (sent to Gmail verbatim, trimmed) for the current account,
    /// remembering the view it replaces for [`Store::end_search`]. A query with no terms ends
    /// the search instead.
    pub fn search(&mut self, query: &str) {
        let query = query.trim();
        // Nothing but whitespace or empty quotes: nothing to search for.
        if Query::parse(query).is_empty() {
            self.end_search();
            return;
        }
        let Some(account) = self.current_account() else {
            return;
        };
        if let Some(view) = &self.view
            && !matches!(view.kind, ViewKind::Search(_))
        {
            self.search_return = Some(view.clone());
        }
        self.set_view(ViewKey {
            account,
            kind: ViewKind::Search(query.to_string()),
        });
    }

    /// The query of the search being shown.
    pub fn search_query(&self) -> Option<&str> {
        match &self.view.as_ref()?.kind {
            ViewKind::Search(q) => Some(q),
            _ => None,
        }
    }

    /// Leaves a search for the view it replaced (the account's inbox when that view belonged
    /// to another account or is unknown). False when no search is shown.
    pub fn end_search(&mut self) -> bool {
        let Some(view) = self.view.clone() else {
            return false;
        };
        if !matches!(view.kind, ViewKind::Search(_)) {
            return false;
        }
        let back = self
            .search_return
            .take()
            .filter(|v| v.account == view.account)
            .unwrap_or(ViewKey {
                account: view.account,
                kind: ViewKind::Label(labels::INBOX.into()),
            });
        self.set_view(back);
        true
    }

    // ----- labels -----

    /// A label's name in an account, or its id when the label is unknown.
    pub fn label_name(&self, account: &str, id: &str) -> String {
        self.labels
            .get(account)
            .and_then(|ls| ls.iter().find(|l| l.id == id))
            .map_or_else(|| id.to_string(), |l| l.name.clone())
    }

    /// Adds (`on`) or removes a label on the given threads: the label picker's toggle. It is
    /// undoable, and the toast names the label.
    pub fn set_label(&mut self, keys: Vec<ThreadKey>, label: &str, on: bool) -> Option<OpId> {
        let account = keys.first()?.account.clone();
        let delta = if on {
            LabelDelta {
                add: vec![label.to_string()],
                remove: vec![],
            }
        } else {
            LabelDelta {
                add: vec![],
                remove: vec![label.to_string()],
            }
        };
        let op = self.mutate_keys(keys, MutationKind::Labels(delta), true, None)?;
        let n = self.pending.get(&op).map_or(0, |p| p.threads.len());
        let what = if n == 1 {
            "1 conversation".to_string()
        } else {
            format!("{n} conversations")
        };
        let name = self.label_name(&account, label);
        if let Some(notice) = &mut self.notice
            && notice.action == Some(NoticeAction::Undo)
        {
            notice.text = if on {
                format!("Added \u{201c}{name}\u{201d} to {what}")
            } else {
                format!("Removed \u{201c}{name}\u{201d} from {what}")
            };
        }
        Some(op)
    }

    // ----- view-aware actions (keys and the reader's action bar) -----

    /// Whether the current view is this label.
    pub fn viewing_label(&self, id: &str) -> bool {
        self.view
            .as_ref()
            .is_some_and(|v| v.kind == ViewKind::Label(id.to_string()))
    }

    /// `e` and Archive: removes INBOX, or does Not spam in the Spam view.
    pub fn archive_kind(&self) -> MutationKind {
        if self.viewing_label(labels::SPAM) {
            MutationKind::Labels(LabelDelta::not_spam())
        } else {
            MutationKind::Labels(LabelDelta::archive())
        }
    }

    /// `#` and Trash: moves to Trash, or restores to the inbox in the Trash view.
    pub fn trash_kind(&self) -> MutationKind {
        if self.viewing_label(labels::TRASH) {
            MutationKind::Untrash {
                restore: LabelDelta {
                    add: vec![labels::INBOX.into()],
                    remove: vec![],
                },
            }
        } else {
            MutationKind::Trash
        }
    }

    /// `!`: reports spam, or does Not spam in the Spam view.
    pub fn spam_kind(&self) -> MutationKind {
        if self.viewing_label(labels::SPAM) {
            MutationKind::Labels(LabelDelta::not_spam())
        } else {
            MutationKind::Labels(LabelDelta::spam())
        }
    }

    /// `s` and Star: stars the targets unless every one is already starred, which unstars.
    pub fn star_kind(&self) -> MutationKind {
        let targets = self.targets();
        let all_starred = !targets.is_empty()
            && targets
                .iter()
                .all(|k| self.threads.get(k).is_some_and(ThreadSummary::starred));
        if all_starred {
            MutationKind::Labels(LabelDelta::unstar())
        } else {
            MutationKind::Labels(LabelDelta::star())
        }
    }

    // ----- mutations -----

    /// Applies `kind` to the targets (selection, open thread or cursor row) optimistically
    /// and sends it to the worker. Returns the op, or `None` when there was nothing to act on.
    pub fn mutate(&mut self, kind: MutationKind) -> Option<OpId> {
        let targets = self.targets();
        self.mutate_keys(targets, kind, true, None)
    }

    /// Applies `kind` to the given threads only (the star on a list row), whatever the
    /// selection holds.
    pub fn mutate_threads(&mut self, keys: Vec<ThreadKey>, kind: MutationKind) -> Option<OpId> {
        self.mutate_keys(keys, kind, true, None)
    }

    /// Every list position a thread holds.
    fn positions_of(&self, key: &ThreadKey) -> Vec<(ViewKey, usize)> {
        self.lists
            .iter()
            .filter(|(v, _)| v.account == key.account)
            .filter_map(|(v, l)| {
                l.ids
                    .iter()
                    .position(|id| *id == key.id)
                    .map(|i| (v.clone(), i))
            })
            .collect()
    }

    /// Moves a label's counts from a thread's `before` state to its `after` state.
    fn adjust_counts(&mut self, before: &ThreadSummary, after: &ThreadSummary) {
        let Some(account_labels) = self.labels.get_mut(&before.key.account) else {
            return;
        };
        let shift = |v: u32, d: i64| (v as i64 + d).max(0) as u32;
        for l in account_labels.iter_mut() {
            let (b, a) = (before.in_label(&l.id), after.in_label(&l.id));
            let unread = |s: &ThreadSummary, on: bool| i64::from(on && s.unread());
            let msgs = |s: &ThreadSummary, on: bool| if on { s.message_count as i64 } else { 0 };
            l.threads_total = shift(l.threads_total, i64::from(a) - i64::from(b));
            l.threads_unread = shift(l.threads_unread, unread(after, a) - unread(before, b));
            l.messages_total = shift(l.messages_total, msgs(after, a) - msgs(before, b));
        }
    }

    /// Inserts a thread into a list by date, unless it would land past a loaded page that
    /// has more after it.
    fn insert_sorted(&mut self, view: &ViewKey, key: &ThreadKey) {
        let Some(date) = self.threads.get(key).map(|s| s.last_date) else {
            return;
        };
        let threads = &self.threads;
        let Some(list) = self.lists.get_mut(view) else {
            return;
        };
        if list.ids.contains(&key.id) {
            return;
        }
        let at = list.ids.iter().position(|id| {
            threads
                .get(&ThreadKey::new(view.account.as_str(), id.as_str()))
                .is_some_and(|s| s.last_date < date)
        });
        match at {
            Some(i) => list.ids.insert(i, key.id.clone()),
            None if !list.has_more => list.ids.push(key.id.clone()),
            None => {}
        }
    }

    /// Puts threads back at recorded list positions, in ascending index order per list.
    fn reinsert(&mut self, positions: &[(ThreadKey, Vec<(ViewKey, usize)>)]) {
        let mut per_view: HashMap<ViewKey, Vec<(usize, String)>> = HashMap::new();
        for (key, places) in positions {
            for (view, ix) in places {
                per_view
                    .entry(view.clone())
                    .or_default()
                    .push((*ix, key.id.clone()));
            }
        }
        for (view, mut items) in per_view {
            items.sort();
            if let Some(list) = self.lists.get_mut(&view) {
                for (ix, id) in items {
                    if !list.ids.contains(&id) {
                        let at = ix.min(list.ids.len());
                        list.ids.insert(at, id);
                    }
                }
            }
        }
    }

    /// The core of every mutation. `undoable` posts the undo toast for kinds that have one;
    /// `restore_positions` (undo) puts threads back where they were before the undone op.
    fn mutate_keys(
        &mut self,
        keys: Vec<ThreadKey>,
        kind: MutationKind,
        undoable: bool,
        restore_positions: Option<Positions>,
    ) -> Option<OpId> {
        let account = keys.first()?.account.clone();
        let mut keys: Vec<ThreadKey> = keys
            .into_iter()
            .filter(|k| k.account == account && self.threads.contains_key(k))
            .collect();
        keys.dedup();
        if keys.is_empty() {
            return None;
        }
        let op = self.alloc_op();
        let delta = kind.effective();
        let old_ids = self.current_ids();
        let snapshots: Vec<Snapshot> = keys
            .iter()
            .filter_map(|k| {
                Some(Snapshot {
                    key: k.clone(),
                    before: self.threads.get(k)?.clone(),
                    positions: self.positions_of(k),
                    touched: Vec::new(),
                })
            })
            .collect();
        if let Some(positions) = &restore_positions {
            self.reinsert(positions);
        }
        for snap in &snapshots {
            let mut after = snap.before.clone();
            delta.apply_to(&mut after.label_ids);
            self.adjust_counts(&snap.before, &after);
            self.threads.insert(snap.key.clone(), after);
        }
        let views: Vec<ViewKey> = self
            .lists
            .keys()
            .filter(|v| v.account == account)
            .cloned()
            .collect();
        for view in views {
            let all_labels = self.labels.get(&account).map(Vec::as_slice).unwrap_or(&[]);
            if removed_by(&view.kind, &delta, all_labels) {
                if let Some(list) = self.lists.get_mut(&view) {
                    list.ids.retain(|id| !keys.iter().any(|k| k.id == *id));
                }
            } else {
                for key in &keys {
                    let gained = self
                        .threads
                        .get(key)
                        .is_some_and(|s| gained_by(&view.kind, &delta, s));
                    if gained {
                        self.insert_sorted(&view, key);
                    }
                }
            }
        }
        let mut snapshots = snapshots;
        for snap in &mut snapshots {
            let was: HashSet<ViewKey> = snap.positions.iter().map(|(v, _)| v.clone()).collect();
            let now: HashSet<ViewKey> = self
                .positions_of(&snap.key)
                .into_iter()
                .map(|(v, _)| v)
                .collect();
            snap.touched = was.symmetric_difference(&now).cloned().collect();
        }
        let thread_ids: Vec<String> = keys.iter().map(|k| k.id.clone()).collect();
        let n = thread_ids.len();
        self.send_command(match &kind {
            MutationKind::Labels(d) => Command::Modify {
                op,
                account: account.clone(),
                thread_ids,
                delta: d.clone(),
            },
            MutationKind::Trash => Command::Trash {
                op,
                account: account.clone(),
                thread_ids,
            },
            MutationKind::Untrash { restore } => Command::Untrash {
                op,
                account: account.clone(),
                thread_ids,
                restore: restore.clone(),
            },
        });
        if undoable && let Some(text) = kind.undo_text(n) {
            self.undo = Some((
                op,
                UndoAction {
                    kind: kind.clone(),
                    threads: snapshots.clone(),
                },
            ));
            self.post_notice(text, Some(NoticeAction::Undo));
        }
        self.pending.insert(
            op,
            Pending {
                kind,
                account,
                threads: snapshots,
            },
        );
        self.settle(&old_ids, true);
        Some(op)
    }

    /// Reverses the last undoable mutation as a new op. False when there is nothing to undo.
    pub fn undo(&mut self) -> bool {
        let Some((_, action)) = self.undo.take() else {
            return false;
        };
        let positions: Positions = action
            .threads
            .iter()
            .map(|s| (s.key.clone(), s.positions.clone()))
            .collect();
        let keys: Vec<ThreadKey> = action.threads.iter().map(|s| s.key.clone()).collect();
        match &action.kind {
            MutationKind::Labels(d) => {
                self.mutate_keys(
                    keys,
                    MutationKind::Labels(d.inverse()),
                    false,
                    Some(positions),
                );
            }
            MutationKind::Untrash { .. } => {
                self.mutate_keys(keys, MutationKind::Trash, false, Some(positions));
            }
            MutationKind::Trash => {
                // One untrash per distinct set of labels to restore, so no thread gains a
                // label it never had.
                let mut groups: BTreeMap<Vec<String>, Positions> = BTreeMap::new();
                for s in &action.threads {
                    groups
                        .entry(restorable(&s.before.label_ids))
                        .or_default()
                        .push((s.key.clone(), s.positions.clone()));
                }
                // Each op puts back only its own threads, so a failed group rolls back alone.
                // Positions were recorded with every thread present: an index shrinks by the
                // threads of groups not yet restored that sit before it in the same list.
                let groups: Vec<(Vec<String>, Positions)> = groups.into_iter().collect();
                for (gi, (add, group)) in groups.iter().enumerate() {
                    let later: Vec<&(ViewKey, usize)> = groups[gi + 1..]
                        .iter()
                        .flat_map(|(_, g)| g.iter().flat_map(|(_, places)| places))
                        .collect();
                    let adjusted: Positions = group
                        .iter()
                        .map(|(key, places)| {
                            let places = places
                                .iter()
                                .map(|(view, ix)| {
                                    let before =
                                        later.iter().filter(|(v, i)| v == view && i < ix).count();
                                    (view.clone(), ix - before)
                                })
                                .collect();
                            (key.clone(), places)
                        })
                        .collect();
                    let restore = LabelDelta {
                        add: add.clone(),
                        remove: vec![],
                    };
                    self.mutate_keys(
                        group.iter().map(|(k, _)| k.clone()).collect(),
                        MutationKind::Untrash { restore },
                        false,
                        Some(adjusted),
                    );
                }
            }
        }
        self.post_notice("Action undone".into(), None);
        true
    }

    /// Sends a message; the outcome arrives as `SendFinished`. A reply's thread is
    /// remembered so its detail is fetched again once the message is in it.
    pub fn send(&mut self, outgoing: Outgoing) -> OpId {
        let op = self.alloc_op();
        self.sends.insert(op, SendState::Sending);
        if let Some(id) = &outgoing.thread_id {
            self.send_threads
                .insert(op, ThreadKey::new(outgoing.account.as_str(), id.as_str()));
        }
        self.send_command(Command::Send { op, outgoing });
        op
    }

    /// After the current list changed: auto-advances the reading pane off a removed thread
    /// (when `advance`), moves a removed cursor to the row at its old index, and drops
    /// selected threads that left the list.
    fn settle(&mut self, old_ids: &[String], advance: bool) {
        let Some(view) = self.view.clone() else {
            return;
        };
        let new_ids = self.current_ids();
        let in_new = |id: &str| new_ids.iter().any(|n| n == id);
        if advance
            && let Some(open) = self.open.clone()
            && open.account == view.account
            && !in_new(&open.id)
            && let Some(ix) = old_ids.iter().position(|id| *id == open.id)
        {
            match successor(old_ids, &new_ids, ix) {
                Some(id) => {
                    let key = ThreadKey::new(view.account.as_str(), id.as_str());
                    self.cursor = Some(key.clone());
                    self.open_thread(key);
                }
                None => self.open = None,
            }
        }
        let new_ids = self.current_ids();
        let in_new = |id: &str| new_ids.iter().any(|n| n == id);
        let cursor_ok = self
            .cursor
            .as_ref()
            .is_some_and(|c| c.account == view.account && in_new(&c.id));
        if !cursor_ok {
            let old_ix = self
                .cursor
                .as_ref()
                .and_then(|c| old_ids.iter().position(|id| *id == c.id))
                .unwrap_or(0);
            self.cursor = new_ids
                .get(old_ix.min(new_ids.len().saturating_sub(1)))
                .map(|id| ThreadKey::new(view.account.as_str(), id.as_str()));
        }
        self.selected
            .retain(|k| k.account == view.account && in_new(&k.id));
    }

    // ----- events -----

    /// Applies pending optimistic deltas over a fresh summary, in op order.
    fn reapply_pending(&self, mut s: ThreadSummary) -> ThreadSummary {
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
    fn pending_membership(&self, view: &ViewKey, key: &ThreadKey) -> Option<bool> {
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

    fn upsert(&mut self, s: ThreadSummary) {
        let s = self.reapply_pending(s);
        self.threads.insert(s.key.clone(), s);
    }

    fn on_page(
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

    fn on_mutation_finished(&mut self, op: OpId, error: Option<String>) {
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
    fn drop_threads(&mut self, gone: impl Fn(&ThreadKey) -> bool) {
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

    fn on_accounts(&mut self, accounts: Vec<AccountId>) {
        let known: HashSet<AccountId> = accounts.iter().cloned().collect();
        if accounts.is_empty() {
            self.last_sync = None;
        }
        self.labels.retain(|a, _| known.contains(a));
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

    // ----- AI triage (SPEC 16.10) ------------------------------------------------------------

    /// Every signed-in account, lowercased: their mail is never checked or sent as a field.
    fn own_accounts(&self) -> Vec<String> {
        self.accounts.iter().map(|a| a.to_lowercase()).collect()
    }

    /// Whether `account` accepted the current disclosure.
    pub fn ai_opted_in(&self, account: &str) -> bool {
        self.prefs.ai.accounts.get(account) == Some(&DISCLOSURE_VERSION)
    }

    /// A key is usable (or this is the demo) and nothing is paused.
    fn ai_ready(&self) -> bool {
        (self.mode == Mode::Demo || matches!(self.ai.key, KeyState::Saved { .. }))
            && self.ai.paused.is_none()
    }

    /// The message a thread's verdict is about (SPEC 16.6.3).
    pub fn ai_target<'a>(&self, detail: &'a ThreadDetail) -> Option<&'a Message> {
        prompt::target(detail, &self.own_accounts())
    }

    fn loaded(&self, key: &ThreadKey) -> Option<&ThreadDetail> {
        match self.details.get(key) {
            Some(DetailState::Loaded(d)) => Some(d),
            _ => None,
        }
    }

    /// The open thread's (or any loaded thread's) target message id.
    fn target_id(&self, key: &ThreadKey) -> Option<String> {
        match self.loaded(key) {
            Some(d) => self.ai_target(d).map(|m| m.id.clone()),
            None => self.ai.by_thread.get(key).cloned(),
        }
    }

    /// The check for `detail`'s target, when SPEC 16.10's conditions hold.
    fn check_request(&self, detail: &ThreadDetail, priority: Priority) -> Option<CheckRequest> {
        let account = &detail.key.account;
        if !self.ai_ready() || !self.ai_opted_in(account) {
            return None;
        }
        let own = self.own_accounts();
        let target = prompt::target(detail, &own)?;
        let id: MessageKey = (account.clone(), target.id.clone());
        if self.ai.checking.contains(&id)
            || self
                .ai
                .failed
                .get(&id)
                .is_some_and(|(_, retryable)| !retryable)
        {
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
                _ => return None,
            },
        };
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
    fn label_ask(&self, account: &str) -> Option<LabelAsk> {
        if !self.prefs.ai.suggest_labels {
            return None;
        }
        let labels = self.labels.get(account)?;
        let candidates = prompt::label_candidates(labels);
        (!candidates.is_empty()).then(|| LabelAsk {
            fingerprint: prompt::label_fingerprint(labels),
            candidates,
        })
    }

    /// Checks the open thread's target when it is due (SPEC 16.10).
    fn check_open(&mut self, key: &ThreadKey) {
        if self.open.as_ref() != Some(key) {
            return;
        }
        let Some(detail) = self.loaded(key) else {
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

    fn suggestions_inner(&self, key: &ThreadKey) -> Option<Vec<Suggestion>> {
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
        if result.verdict.is_some_and(|v| v.level >= Level::Warning)
            || crate::ui::triage::local_caution(&target.signals).is_some()
        {
            return None;
        }
        let probs = result.labels.as_ref()?;
        let names = prompt::candidate_labels(self.labels.get(&key.account)?);
        let applied = match self.threads.get(key) {
            Some(s) => &s.label_ids,
            None => &detail.label_ids,
        };
        Some(select_suggestions(probs, &names, applied))
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

    fn on_detail(&mut self, detail: ThreadDetail) {
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
    fn prefetch_new_mail(&mut self, account: &str, threads: &[ThreadSummary]) {
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

    fn result_for(&self, key: &ThreadKey) -> Option<(&String, &AiResult)> {
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
        match ev {
            AiEvent::Key(k) => {
                if !matches!(k, KeyState::Saved { .. }) {
                    self.ai.checking.clear();
                }
                if k == KeyState::Missing {
                    self.ai.forget_all();
                }
                self.ai.key = k;
                self.ai.ai_rev += 1;
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
                if unlabelled {
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
                if reason.is_some() {
                    self.ai.checking.clear();
                }
                self.ai.paused = reason;
                self.ai.ai_rev += 1;
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
            SyncEvent::Saved(path) => {
                let text = saved_text(&path);
                self.post_notice(text, Some(NoticeAction::OpenFolder(path)));
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
            SyncEvent::SendFinished { op, error } => {
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

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;
    use crate::model::Address;

    const ME: &str = "you@example.com";
    const WORK: &str = "you@work.example";

    fn store() -> (Store, mpsc::Receiver<Command>) {
        let (tx, rx) = mpsc::channel();
        (Store::for_tests(SyncHandle::from_sender(tx)), rx)
    }

    fn view(label: &str) -> ViewKey {
        ViewKey {
            account: ME.into(),
            kind: ViewKind::Label(label.into()),
        }
    }

    fn all_mail() -> ViewKey {
        ViewKey {
            account: ME.into(),
            kind: ViewKind::AllMail,
        }
    }

    fn key(id: &str) -> ThreadKey {
        ThreadKey::new(ME, id)
    }

    /// Thread `t{n}`, `n` minutes old, so lower numbers sort first.
    fn summary(n: usize, labels: &[&str]) -> ThreadSummary {
        ThreadSummary {
            key: key(&format!("t{n}")),
            history_id: 1,
            subject: format!("Subject {n}"),
            snippet: String::new(),
            participants: vec![(Address::new(Some("Ann"), "ann@example.org"), false)],
            last_date: Timestamp::from_second(1_800_000_000 - 60 * n as i64).unwrap(),
            message_count: 1,
            label_ids: labels.iter().map(|s| s.to_string()).collect(),
            has_attachments: false,
        }
    }

    fn label(id: &str, kind: LabelKind, unread: u32, total: u32) -> Label {
        Label {
            account: ME.into(),
            id: id.into(),
            name: id.into(),
            kind,
            color: None,
            threads_unread: unread,
            threads_total: total,
            messages_total: total,
            hidden: false,
        }
    }

    fn ids(range: std::ops::Range<usize>) -> Vec<String> {
        range.map(|n| format!("t{n}")).collect()
    }

    fn page(
        s: &mut Store,
        v: &ViewKey,
        range: std::ops::Range<usize>,
        labels: &[&str],
        mode: PageMode,
        has_more: bool,
    ) {
        s.apply(SyncEvent::ThreadPage {
            view: v.clone(),
            ids: ids(range.clone()),
            threads: range.map(|n| summary(n, labels)).collect(),
            mode,
            has_more,
        });
    }

    /// A store showing INBOX with `n` threads (t0..tn) that are also in All Mail.
    fn inbox_store(n: usize) -> (Store, mpsc::Receiver<Command>) {
        let (mut s, rx) = store();
        s.apply(SyncEvent::Labels {
            account: ME.into(),
            labels: vec![
                label("INBOX", LabelKind::System, 0, n as u32),
                label("SPAM", LabelKind::System, 0, 0),
                label("TRASH", LabelKind::System, 0, 0),
            ],
        });
        s.apply(SyncEvent::Accounts(vec![ME.into()]));
        page(
            &mut s,
            &all_mail(),
            0..n,
            &["INBOX"],
            PageMode::Replace,
            false,
        );
        page(
            &mut s,
            &view("INBOX"),
            0..n,
            &["INBOX"],
            PageMode::Replace,
            false,
        );
        drain(&rx);
        (s, rx)
    }

    fn drain(rx: &mpsc::Receiver<Command>) -> Vec<Command> {
        rx.try_iter().collect()
    }

    fn list(s: &Store, v: &ViewKey) -> Vec<String> {
        s.lists.get(v).map(|l| l.ids.clone()).unwrap_or_default()
    }

    fn label_count(s: &Store, id: &str) -> (u32, u32) {
        let l = s.labels[ME].iter().find(|l| l.id == id).unwrap();
        (l.threads_unread, l.threads_total)
    }

    #[test]
    fn apply_every_event() {
        let (mut s, rx) = store();
        s.apply(SyncEvent::Auth(AuthState::Demo));
        assert_eq!(s.auth, AuthState::Demo);
        s.apply(SyncEvent::Client(Some("id".into())));
        assert_eq!(s.client_id.as_deref(), Some("id"));
        s.apply(SyncEvent::Accounts(vec![ME.into(), WORK.into()]));
        assert_eq!(s.accounts.len(), 2);
        assert_eq!(s.view, Some(view("INBOX")));
        assert!(matches!(
            drain(&rx).as_slice(),
            [Command::Want { more: false, .. }]
        ));
        s.apply(SyncEvent::SigningIn(Some(
            "https://accounts.example".into(),
        )));
        assert!(s.signing_in.is_some());
        s.apply(SyncEvent::Busy(true));
        assert!(s.busy);
        s.apply(SyncEvent::Synced(Timestamp::UNIX_EPOCH));
        assert_eq!(s.last_sync, Some(Timestamp::UNIX_EPOCH));
        s.apply(SyncEvent::Problem(Some("Offline".into())));
        assert_eq!(s.problem.as_deref(), Some("Offline"));
        s.apply(SyncEvent::Notice("Hello".into()));
        assert_eq!(s.notice.as_ref().map(|n| n.text.as_str()), Some("Hello"));
        let saved = std::path::PathBuf::from("/srv/dl/a (1).pdf");
        s.apply(SyncEvent::Saved(saved.clone()));
        let notice = s.notice.clone().unwrap();
        assert_eq!(notice.text, "Saved a (1).pdf to /srv/dl");
        assert_eq!(notice.action, Some(NoticeAction::OpenFolder(saved)));
        s.apply(SyncEvent::Labels {
            account: ME.into(),
            labels: vec![
                label("INBOX", LabelKind::System, 3, 10),
                label("Label_1", LabelKind::User, 1, 2),
            ],
        });
        assert_eq!(s.unread_badge(ME, "INBOX"), Some(3));
        s.apply(SyncEvent::LabelCounts {
            account: ME.into(),
            labels: vec![label("INBOX", LabelKind::System, 5, 11)],
        });
        assert_eq!(s.unread_badge(ME, "INBOX"), Some(5));
        assert_eq!(s.labels[ME].len(), 2);
        page(
            &mut s,
            &view("INBOX"),
            0..3,
            &["INBOX", "UNREAD"],
            PageMode::Replace,
            true,
        );
        assert_eq!(list(&s, &view("INBOX")), ids(0..3));
        assert_eq!(s.cursor, Some(key("t0")));
        assert_eq!(s.visible_rows().len(), 3);
        let mut updated = summary(1, &["INBOX"]);
        updated.subject = "Changed".into();
        s.apply(SyncEvent::ThreadsUpdated(vec![updated]));
        assert_eq!(s.threads[&key("t1")].subject, "Changed");
        let detail = ThreadDetail {
            key: key("t0"),
            subject: "Subject 0".into(),
            label_ids: BTreeSet::new(),
            messages: vec![],
        };
        s.apply(SyncEvent::ThreadDetail(detail));
        assert!(matches!(
            s.details.get(&key("t0")),
            Some(DetailState::Loaded(_))
        ));
        s.apply(SyncEvent::ThreadFailed {
            key: key("t1"),
            error: "Gone".into(),
        });
        assert_eq!(
            s.details.get(&key("t1")),
            Some(&DetailState::Failed("Gone".into()))
        );
        s.apply(SyncEvent::MutationFinished {
            op: 99,
            error: None,
        });
        s.apply(SyncEvent::SendFinished { op: 7, error: None });
        assert_eq!(s.send_state(7), Some(&SendState::Sent));
        assert_eq!(
            s.notice.as_ref().map(|n| n.text.as_str()),
            Some("Message sent")
        );
        let failure = SendError {
            message: "No".into(),
            maybe_sent: true,
        };
        s.apply(SyncEvent::SendFinished {
            op: 8,
            error: Some(failure.clone()),
        });
        assert_eq!(s.send_state(8), Some(&SendState::Failed(failure)));
        s.forget_send(8);
        assert_eq!(s.send_state(8), None);
        s.apply(SyncEvent::NewMail {
            account: ME.into(),
            threads: vec![summary(9, &["INBOX", "UNREAD"])],
        });
        assert_eq!(s.take_new_mail().len(), 1);
        s.apply(SyncEvent::ThreadsGone {
            account: ME.into(),
            ids: vec!["t0".into()],
        });
        assert_eq!(list(&s, &view("INBOX")), ids(1..3));
        assert!(!s.threads.contains_key(&key("t0")));
        assert_eq!(s.cursor, Some(key("t1")));
        s.apply(SyncEvent::AccountReset(ME.into()));
        assert!(s.threads.keys().all(|k| k.account != ME));
        assert!(s.lists.keys().all(|v| v.account != ME));
        assert!(s.details.keys().all(|k| k.account != ME));
    }

    #[test]
    fn cursor_moves_and_requests_more() {
        let (mut s, rx) = store();
        s.apply(SyncEvent::Accounts(vec![ME.into()]));
        drain(&rx);
        page(
            &mut s,
            &view("INBOX"),
            0..50,
            &["INBOX"],
            PageMode::Replace,
            true,
        );
        assert_eq!(s.cursor_index(), Some(0));
        s.move_cursor(-1);
        assert_eq!(s.cursor_index(), Some(0));
        s.move_cursor(1);
        assert_eq!(s.cursor_index(), Some(1));
        s.move_cursor(38);
        assert_eq!(s.cursor_index(), Some(39));
        assert!(drain(&rx).is_empty(), "row 39 of 50 is not in the last ten");
        s.move_cursor(1);
        assert_eq!(s.cursor_index(), Some(40));
        let cmds = drain(&rx);
        assert!(matches!(
            cmds.as_slice(),
            [Command::Want { more: true, .. }]
        ));
        s.move_cursor(1);
        assert!(
            drain(&rx).is_empty(),
            "the request is not repeated while loading"
        );
        s.move_cursor(100);
        assert_eq!(s.cursor_index(), Some(49));
        page(
            &mut s,
            &view("INBOX"),
            50..60,
            &["INBOX"],
            PageMode::Append,
            false,
        );
        assert_eq!(s.cursor_index(), Some(49));
        s.move_cursor(5);
        assert_eq!(s.cursor_index(), Some(54));
        assert!(drain(&rx).is_empty(), "no more pages");
    }

    #[test]
    fn archive_removes_from_inbox_and_auto_advances() {
        let (mut s, rx) = inbox_store(5);
        s.move_cursor(2);
        s.open_cursor();
        assert_eq!(s.open, Some(key("t2")));
        drain(&rx);
        let op = s
            .mutate(MutationKind::Labels(LabelDelta::archive()))
            .unwrap();
        assert_eq!(list(&s, &view("INBOX")), ["t0", "t1", "t3", "t4"]);
        assert_eq!(list(&s, &all_mail()), ids(0..5), "archive keeps All Mail");
        assert!(!s.threads[&key("t2")].in_label("INBOX"));
        assert_eq!(label_count(&s, "INBOX").1, 4);
        assert_eq!(s.open, Some(key("t3")));
        assert_eq!(s.cursor, Some(key("t3")));
        let cmds = drain(&rx);
        assert!(cmds.iter().any(|c| matches!(c,
            Command::Modify { op: o, thread_ids, delta, .. }
                if *o == op && thread_ids == &["t2"] && *delta == LabelDelta::archive())));
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Command::OpenThread(k) if *k == key("t3")))
        );
        let notice = s.notice.clone().unwrap();
        assert_eq!(notice.text, "Archived 1 conversation");
        assert_eq!(notice.action, Some(NoticeAction::Undo));
        s.apply(SyncEvent::MutationFinished { op, error: None });
        assert!(!s.is_pending(&key("t2")));

        // The last row falls back to the previous one, then the reader closes.
        s.set_cursor_index(3);
        s.open_cursor();
        s.mutate(MutationKind::Labels(LabelDelta::archive()));
        assert_eq!(s.open, Some(key("t3")));
        s.mutate(MutationKind::Labels(LabelDelta::archive()));
        s.mutate(MutationKind::Labels(LabelDelta::archive()));
        s.mutate(MutationKind::Labels(LabelDelta::archive()));
        assert!(list(&s, &view("INBOX")).is_empty());
        assert_eq!(s.open, None);
        assert_eq!(s.cursor, None);
    }

    #[test]
    fn rollback_restores_index() {
        let (mut s, rx) = inbox_store(5);
        s.set_cursor_index(2);
        let op = s.mutate(MutationKind::Trash).unwrap();
        assert!(matches!(drain(&rx).as_slice(), [Command::Trash { .. }]));
        assert_eq!(list(&s, &view("INBOX")), ["t0", "t1", "t3", "t4"]);
        assert_eq!(list(&s, &all_mail()), ["t0", "t1", "t3", "t4"]);
        assert_eq!(s.cursor, Some(key("t3")));
        assert_eq!(label_count(&s, "TRASH").1, 1);
        s.apply(SyncEvent::MutationFinished {
            op,
            error: Some("server said no".into()),
        });
        assert_eq!(list(&s, &view("INBOX")), ids(0..5));
        assert_eq!(list(&s, &all_mail()), ids(0..5));
        assert!(s.threads[&key("t2")].in_label("INBOX"));
        assert!(!s.threads[&key("t2")].in_label("TRASH"));
        assert_eq!(label_count(&s, "TRASH").1, 0);
        assert_eq!(label_count(&s, "INBOX").1, 5);
        let notice = s.notice.clone().unwrap();
        assert_eq!(notice.text, "Couldn't move to Trash: server said no");
        assert_eq!(notice.action, None);
        assert!(!s.undo(), "a rolled-back op cannot be undone");
    }

    #[test]
    fn undo_goes_with_its_toast() {
        let (mut s, rx) = inbox_store(4);
        s.set_cursor_index(1);
        s.mutate(MutationKind::Labels(LabelDelta::archive()));
        s.apply(SyncEvent::Notice("Synced".into()));
        assert!(
            !s.undo(),
            "a notice without Undo takes the undo slot with it"
        );

        // A different op's failure toast does the same.
        s.set_cursor_index(0);
        s.mutate(MutationKind::Labels(LabelDelta::archive()));
        s.set_cursor_index(0);
        let star = s.mutate(MutationKind::Labels(LabelDelta::star())).unwrap();
        s.apply(SyncEvent::MutationFinished {
            op: star,
            error: Some("nope".into()),
        });
        assert!(!s.undo());
        drain(&rx);
    }

    #[test]
    fn rollback_keeps_other_pending_ops() {
        let (mut s, rx) = inbox_store(4);
        s.set_cursor_index(1);
        let archive = s
            .mutate(MutationKind::Labels(LabelDelta::archive()))
            .unwrap();
        // Starred after the archive's snapshot was taken, still pending when it fails.
        let star = s
            .mutate_keys(
                vec![key("t1")],
                MutationKind::Labels(LabelDelta::star()),
                false,
                None,
            )
            .unwrap();
        drain(&rx);
        assert_eq!(list(&s, &view("INBOX")), ["t0", "t2", "t3"]);
        s.apply(SyncEvent::MutationFinished {
            op: archive,
            error: Some("server said no".into()),
        });
        let t1 = &s.threads[&key("t1")];
        assert!(t1.in_label("INBOX"));
        assert!(t1.starred(), "the still-pending star survives the rollback");
        assert_eq!(list(&s, &view("INBOX")), ids(0..4));
        assert_eq!(label_count(&s, "INBOX").1, 4);
        s.apply(SyncEvent::MutationFinished {
            op: star,
            error: None,
        });
        assert!(s.threads[&key("t1")].starred());
    }

    #[test]
    fn pending_join_survives_stale_page() {
        let (mut s, rx) = inbox_store(4);
        s.set_cursor_index(1);
        let archive = s
            .mutate(MutationKind::Labels(LabelDelta::archive()))
            .unwrap();
        s.apply(SyncEvent::MutationFinished {
            op: archive,
            error: None,
        });
        assert!(s.undo());
        drain(&rx);
        assert_eq!(list(&s, &view("INBOX")), ids(0..4));
        // Listed before the worker applied the undo: t1 is missing.
        s.apply(SyncEvent::ThreadPage {
            view: view("INBOX"),
            ids: vec!["t0".into(), "t2".into(), "t3".into()],
            threads: vec![],
            mode: PageMode::Replace,
            has_more: false,
        });
        assert_eq!(list(&s, &view("INBOX")), ids(0..4));
        s.apply(SyncEvent::ThreadPage {
            view: view("INBOX"),
            ids: vec!["t0".into(), "t2".into(), "t3".into()],
            threads: vec![],
            mode: PageMode::Refresh,
            has_more: false,
        });
        assert_eq!(list(&s, &view("INBOX")), ids(0..4));
    }

    #[test]
    fn grouped_untrash_restores_order_and_rolls_back_alone() {
        let (mut s, rx) = inbox_store(4);
        s.apply(SyncEvent::ThreadsUpdated(vec![summary(
            0,
            &["INBOX", "Label_1"],
        )]));
        s.toggle_select(key("t0"));
        s.toggle_select(key("t2"));
        s.mutate(MutationKind::Trash);
        assert_eq!(list(&s, &view("INBOX")), ["t1", "t3"]);
        drain(&rx);
        assert!(s.undo());
        assert_eq!(list(&s, &view("INBOX")), ids(0..4));
        assert_eq!(list(&s, &all_mail()), ids(0..4));
        let t0_op = drain(&rx)
            .into_iter()
            .find_map(|c| match c {
                Command::Untrash { op, thread_ids, .. } if thread_ids == ["t0"] => Some(op),
                _ => None,
            })
            .unwrap();
        s.apply(SyncEvent::MutationFinished {
            op: t0_op,
            error: Some("server said no".into()),
        });
        assert_eq!(list(&s, &view("INBOX")), ["t1", "t2", "t3"]);
        assert_eq!(list(&s, &all_mail()), ["t1", "t2", "t3"]);
        assert!(s.threads[&key("t0")].in_label("TRASH"));
        assert!(!s.threads[&key("t2")].in_label("TRASH"));
    }

    #[test]
    fn pending_delta_reapplied_over_fresh_data() {
        let (mut s, rx) = inbox_store(4);
        s.set_cursor_index(1);
        let op = s
            .mutate(MutationKind::Labels(LabelDelta::archive()))
            .unwrap();
        drain(&rx);
        // Stale data from before the change arrives while the op is pending.
        s.apply(SyncEvent::ThreadsUpdated(vec![summary(
            1,
            &["INBOX", "UNREAD"],
        )]));
        assert!(!s.threads[&key("t1")].in_label("INBOX"));
        assert!(
            s.threads[&key("t1")].unread(),
            "the rest of the fresh data is kept"
        );
        page(
            &mut s,
            &view("INBOX"),
            0..4,
            &["INBOX"],
            PageMode::Refresh,
            false,
        );
        assert_eq!(list(&s, &view("INBOX")), ["t0", "t2", "t3"]);
        page(
            &mut s,
            &view("INBOX"),
            0..4,
            &["INBOX"],
            PageMode::Replace,
            false,
        );
        assert_eq!(list(&s, &view("INBOX")), ["t0", "t2", "t3"]);
        s.apply(SyncEvent::MutationFinished { op, error: None });
        // Authoritative data after the op is taken as it is.
        s.apply(SyncEvent::ThreadsUpdated(vec![summary(1, &[])]));
        assert!(s.threads[&key("t1")].label_ids.is_empty());
        page(
            &mut s,
            &view("INBOX"),
            0..4,
            &["INBOX"],
            PageMode::Replace,
            false,
        );
        assert_eq!(list(&s, &view("INBOX")), ids(0..4));
    }

    #[test]
    fn undo_applies_inverse() {
        let (mut s, rx) = inbox_store(4);
        s.set_cursor_index(1);
        s.open_cursor();
        drain(&rx);
        s.mutate(MutationKind::Labels(LabelDelta::archive()));
        assert_eq!(s.open, Some(key("t2")));
        drain(&rx);
        assert!(s.undo());
        let cmds = drain(&rx);
        assert!(matches!(cmds.as_slice(),
            [Command::Modify { thread_ids, delta, .. }]
                if thread_ids == &["t1"] && *delta == LabelDelta::archive().inverse()));
        assert_eq!(list(&s, &view("INBOX")), ids(0..4));
        assert!(s.threads[&key("t1")].in_label("INBOX"));
        assert_eq!(label_count(&s, "INBOX").1, 4);
        assert_eq!(s.notice.as_ref().unwrap().text, "Action undone");
        assert!(!s.undo(), "undo is single-shot");

        // Read and star changes are not undoable.
        s.mutate(MutationKind::Labels(LabelDelta::star()));
        assert!(!s.undo());
    }

    #[test]
    fn undo_trash_untrashes_and_restores_labels() {
        let (mut s, rx) = inbox_store(3);
        s.apply(SyncEvent::ThreadsUpdated(vec![summary(
            0,
            &["INBOX", "Label_1"],
        )]));
        page(
            &mut s,
            &view("TRASH"),
            5..6,
            &["TRASH"],
            PageMode::Replace,
            false,
        );
        s.set_view(view("INBOX"));
        page(
            &mut s,
            &view("INBOX"),
            0..3,
            &["INBOX"],
            PageMode::Replace,
            false,
        );
        s.apply(SyncEvent::ThreadsUpdated(vec![summary(
            0,
            &["INBOX", "Label_1"],
        )]));
        drain(&rx);
        s.toggle_select(key("t0"));
        s.toggle_select(key("t1"));
        s.mutate(MutationKind::Trash);
        assert_eq!(list(&s, &view("INBOX")), ["t2"]);
        assert_eq!(list(&s, &view("TRASH")), ["t0", "t1", "t5"]);
        drain(&rx);
        assert!(s.undo());
        let mut restores: Vec<(Vec<String>, Vec<String>)> = drain(&rx)
            .into_iter()
            .filter_map(|c| match c {
                Command::Untrash {
                    thread_ids,
                    restore,
                    ..
                } => Some((thread_ids, restore.add)),
                _ => None,
            })
            .collect();
        restores.sort();
        assert_eq!(
            restores,
            vec![
                (
                    vec!["t0".to_string()],
                    vec!["INBOX".to_string(), "Label_1".to_string()]
                ),
                (vec!["t1".to_string()], vec!["INBOX".to_string()]),
            ]
        );
        assert_eq!(list(&s, &view("INBOX")), ids(0..3));
        assert_eq!(list(&s, &view("TRASH")), ["t5"]);
        assert!(!s.threads[&key("t0")].in_label("TRASH"));
        assert!(s.threads[&key("t0")].in_label("Label_1"));
    }

    #[test]
    fn spam_removes_from_label_views_and_all_mail() {
        let (mut s, rx) = inbox_store(3);
        let search = ViewKey {
            account: ME.into(),
            kind: ViewKind::Search("ann".into()),
        };
        page(&mut s, &search, 0..3, &["INBOX"], PageMode::Replace, false);
        let anywhere = ViewKey {
            account: ME.into(),
            kind: ViewKind::Search("ann in:anywhere".into()),
        };
        page(
            &mut s,
            &anywhere,
            0..3,
            &["INBOX"],
            PageMode::Replace,
            false,
        );
        page(
            &mut s,
            &view("Label_1"),
            0..3,
            &["INBOX"],
            PageMode::Replace,
            false,
        );
        page(
            &mut s,
            &view("SPAM"),
            7..8,
            &["SPAM"],
            PageMode::Replace,
            false,
        );
        s.set_view(view("INBOX"));
        drain(&rx);
        s.set_cursor_index(0);
        s.mutate(MutationKind::Labels(LabelDelta::spam()));
        assert_eq!(list(&s, &view("INBOX")), ["t1", "t2"]);
        assert_eq!(list(&s, &view("Label_1")), ["t1", "t2"]);
        assert_eq!(list(&s, &all_mail()), ["t1", "t2"]);
        assert_eq!(list(&s, &search), ["t1", "t2"], "search leaves out Spam");
        assert_eq!(list(&s, &anywhere), ids(0..3), "unless it asks for Spam");
        assert_eq!(list(&s, &view("SPAM")), ["t0", "t7"], "joins Spam by date");
        assert_eq!(
            s.notice.as_ref().unwrap().text,
            "Marked 1 conversation as spam"
        );
        assert!(s.undo());
        assert_eq!(list(&s, &search), ids(0..3), "undo puts the result back");
    }

    #[test]
    fn refresh_page_keeps_tail_and_cursor() {
        let (mut s, rx) = store();
        s.apply(SyncEvent::Accounts(vec![ME.into()]));
        drain(&rx);
        let inbox = view("INBOX");
        page(&mut s, &inbox, 0..5, &["INBOX"], PageMode::Replace, true);
        page(&mut s, &inbox, 5..10, &["INBOX"], PageMode::Append, false);
        s.set_cursor_index(7);
        // A new thread arrives at the top and t2 was archived elsewhere.
        let mut fresh = vec!["new".to_string()];
        fresh.extend(["t0", "t1", "t3", "t4"].map(String::from));
        s.apply(SyncEvent::ThreadPage {
            view: inbox.clone(),
            ids: fresh.clone(),
            threads: vec![],
            mode: PageMode::Refresh,
            has_more: true,
        });
        let mut want = fresh.clone();
        want.extend(ids(5..10));
        assert_eq!(list(&s, &inbox), want);
        assert_eq!(s.cursor, Some(key("t7")), "the cursor follows its key");
        assert!(
            !s.lists[&inbox].has_more,
            "has_more only changes for a single page"
        );
        assert!(matches!(s.visible_rows()[0], Row::Skeleton(_)));

        // The cursor's own row disappears: it moves to the row at its old index.
        s.set_cursor_index(1);
        s.apply(SyncEvent::ThreadPage {
            view: inbox.clone(),
            ids: ["new", "t1", "t3", "t4"].map(String::from).to_vec(),
            threads: vec![],
            mode: PageMode::Refresh,
            has_more: true,
        });
        assert_eq!(s.cursor, Some(key("t1")));

        // A page whose last id is unknown replaces that many rows and keeps the rest.
        assert_eq!(
            merge_refresh(&ids(0..5), &["x".to_string()]),
            ["x", "t1", "t2", "t3", "t4"]
        );
        assert_eq!(
            merge_refresh(
                &ids(0..5),
                &["x".to_string(), "t0".to_string(), "y".to_string()]
            ),
            ["x", "t0", "y", "t3", "t4"]
        );
        assert!(merge_refresh(&ids(0..5), &[]).is_empty());
    }

    #[test]
    fn append_dedupes() {
        let (mut s, rx) = store();
        s.apply(SyncEvent::Accounts(vec![ME.into()]));
        drain(&rx);
        let inbox = view("INBOX");
        page(&mut s, &inbox, 0..5, &["INBOX"], PageMode::Replace, true);
        page(&mut s, &inbox, 3..8, &["INBOX"], PageMode::Append, true);
        assert_eq!(list(&s, &inbox), ids(0..8));
        s.apply(SyncEvent::ThreadPage {
            view: inbox.clone(),
            ids: ["t8", "t8", "t0"].map(String::from).to_vec(),
            threads: vec![],
            mode: PageMode::Append,
            has_more: false,
        });
        assert_eq!(list(&s, &inbox), ids(0..9));
        assert!(!s.lists[&inbox].has_more);
    }

    #[test]
    fn new_mail_only_when_pref_on() {
        let (mut s, _rx) = store();
        let ev = || SyncEvent::NewMail {
            account: ME.into(),
            threads: vec![summary(0, &["INBOX", "UNREAD"])],
        };
        s.apply(ev());
        assert_eq!(s.take_new_mail().len(), 1);
        assert!(s.take_new_mail().is_empty());
        s.prefs.notifications = false;
        s.apply(ev());
        assert!(s.take_new_mail().is_empty());
    }

    #[test]
    fn new_mail_keeps_only_the_named_accounts_summaries() {
        let (mut s, _rx) = store();
        let mut theirs = summary(1, &["INBOX", "UNREAD"]);
        theirs.key.account = "someone@example.org".into();
        s.apply(SyncEvent::NewMail {
            account: ME.into(),
            threads: vec![theirs.clone()],
        });
        assert!(s.take_new_mail().is_empty(), "another account's summary");
        s.apply(SyncEvent::NewMail {
            account: ME.into(),
            threads: vec![theirs, summary(0, &["INBOX", "UNREAD"])],
        });
        let kept = s.take_new_mail();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].key.account, ME);
    }

    #[test]
    fn settings_changes_prefs_in_range_and_stays_ephemeral() {
        let (mut s, rx) = store();
        s.change_prefs(|p| {
            p.clock_24h = false;
            p.refresh_secs = 5;
        });
        assert!(!s.prefs.clock_24h);
        assert_eq!(s.prefs.refresh_secs, crate::config::REFRESH_SECS_MIN);
        // The worker hears about the new interval, clamped.
        assert!(matches!(
            drain(&rx).as_slice(),
            [Command::Tune { refresh_secs, .. }] if *refresh_secs == crate::config::REFRESH_SECS_MIN
        ));
        s.change_prefs(|p| p.clock_24h = true);
        assert!(
            drain(&rx).is_empty(),
            "other prefs do not retune the worker"
        );
        // Ephemeral prefs (demo, tests) stay ephemeral, so `save` can never write them.
        assert!(s.prefs.ephemeral);
    }

    #[test]
    fn abandoned_send_failure_becomes_a_notice() {
        let (mut s, _rx) = store();
        let op = s.send(crate::ui::compose::blank(&ME.to_string()));
        assert_eq!(s.send_state(op), Some(&SendState::Sending));
        s.abandon_send(op);
        assert_eq!(s.send_state(op), None);
        s.apply(SyncEvent::SendFinished {
            op,
            error: Some(SendError {
                message: "No".into(),
                maybe_sent: false,
            }),
        });
        assert_eq!(s.send_state(op), None, "nothing is left behind");
        assert_eq!(
            s.notice.as_ref().map(|n| n.text.as_str()),
            Some("Couldn't send the message")
        );
        // A settled send that is abandoned is just forgotten.
        let op = s.send(crate::ui::compose::blank(&ME.to_string()));
        s.apply(SyncEvent::SendFinished { op, error: None });
        s.abandon_send(op);
        assert_eq!(s.send_state(op), None);
    }

    #[test]
    fn account_removal_prunes() {
        let (mut s, rx) = store();
        s.prefs.last_view = Some((WORK.into(), "Label_9".into()));
        s.apply(SyncEvent::Accounts(vec![ME.into(), WORK.into()]));
        let work_view = ViewKey {
            account: WORK.into(),
            kind: ViewKind::Label("Label_9".into()),
        };
        assert_eq!(s.view, Some(work_view.clone()), "last_view is restored");
        let mut w = summary(1, &["Label_9"]);
        w.key = ThreadKey::new(WORK, "w1");
        s.apply(SyncEvent::ThreadPage {
            view: work_view.clone(),
            ids: vec!["w1".into()],
            threads: vec![w],
            mode: PageMode::Replace,
            has_more: false,
        });
        s.apply(SyncEvent::Labels {
            account: WORK.into(),
            labels: vec![],
        });
        page(
            &mut s,
            &view("INBOX"),
            0..2,
            &["INBOX"],
            PageMode::Replace,
            false,
        );
        s.open_cursor();
        drain(&rx);
        s.apply(SyncEvent::Accounts(vec![ME.into()]));
        assert!(s.threads.keys().all(|k| k.account == ME));
        assert!(s.lists.keys().all(|v| v.account == ME));
        assert!(!s.labels.contains_key(WORK));
        assert_eq!(s.open, None);
        assert_eq!(s.view, Some(view("INBOX")), "falls back to the first inbox");
        assert_eq!(s.cursor, Some(key("t0")));
        assert!(matches!(
            drain(&rx).as_slice(),
            [Command::Want { more: false, .. }]
        ));
        s.apply(SyncEvent::Accounts(vec![]));
        assert_eq!(s.view, None);
        assert!(s.threads.is_empty());
    }

    #[test]
    fn selection_targets_bulk_mutation() {
        let (mut s, rx) = inbox_store(5);
        s.set_cursor_index(0);
        s.open_cursor();
        drain(&rx);
        s.toggle_select(key("t1"));
        s.toggle_select(key("t3"));
        s.toggle_select(key("t4"));
        s.toggle_select(key("t4"));
        assert_eq!(s.targets(), [key("t1"), key("t3")]);
        s.mutate(MutationKind::Labels(LabelDelta::archive()));
        let cmds = drain(&rx);
        assert!(matches!(cmds.as_slice(),
            [Command::Modify { thread_ids, .. }] if thread_ids == &["t1", "t3"]));
        assert_eq!(list(&s, &view("INBOX")), ["t0", "t2", "t4"]);
        assert!(s.selected.is_empty(), "archived rows leave the selection");
        assert_eq!(s.open, Some(key("t0")), "the open thread was not a target");
        assert_eq!(s.notice.as_ref().unwrap().text, "Archived 2 conversations");
        // Without a selection the open thread is the target.
        assert_eq!(s.targets(), [key("t0")]);
    }

    #[test]
    fn view_aware_kinds_follow_the_view() {
        let (mut s, rx) = inbox_store(3);
        assert_eq!(
            s.archive_kind(),
            MutationKind::Labels(LabelDelta::archive())
        );
        assert_eq!(s.trash_kind(), MutationKind::Trash);
        assert_eq!(s.spam_kind(), MutationKind::Labels(LabelDelta::spam()));
        s.set_view(view("SPAM"));
        assert_eq!(
            s.archive_kind(),
            MutationKind::Labels(LabelDelta::not_spam())
        );
        assert_eq!(s.spam_kind(), MutationKind::Labels(LabelDelta::not_spam()));
        s.set_view(view("TRASH"));
        assert_eq!(
            s.trash_kind(),
            MutationKind::Untrash {
                restore: LabelDelta {
                    add: vec!["INBOX".into()],
                    remove: vec![],
                }
            }
        );
        drain(&rx);
    }

    #[test]
    fn star_kind_unstars_only_when_every_target_is_starred() {
        let (mut s, _rx) = inbox_store(3);
        s.set_cursor_index(0);
        assert_eq!(s.star_kind(), MutationKind::Labels(LabelDelta::star()));
        s.mutate(MutationKind::Labels(LabelDelta::star()));
        assert_eq!(s.star_kind(), MutationKind::Labels(LabelDelta::unstar()));
        s.toggle_select(key("t0"));
        s.toggle_select(key("t1"));
        assert_eq!(s.star_kind(), MutationKind::Labels(LabelDelta::star()));
    }

    #[test]
    fn go_to_and_switch_account() {
        let (mut s, rx) = inbox_store(2);
        s.apply(SyncEvent::Accounts(vec![ME.into(), WORK.into()]));
        drain(&rx);
        s.go_to(ViewKind::Label("STARRED".into()));
        assert_eq!(s.view, Some(view("STARRED")));
        s.go_to(ViewKind::AllMail);
        assert_eq!(s.view, Some(all_mail()));
        let inbox_of = |a: &str| ViewKey {
            account: a.into(),
            kind: ViewKind::Label("INBOX".into()),
        };
        s.switch_account(1);
        assert_eq!(s.view, Some(inbox_of(WORK)));
        s.switch_account(1);
        assert_eq!(s.view, Some(inbox_of(ME)), "wraps round");
        s.switch_account(-1);
        assert_eq!(s.view, Some(inbox_of(WORK)));
        assert!(
            matches!(drain(&rx).last(), Some(Command::Want { view, more: false })
            if *view == inbox_of(WORK))
        );
    }

    #[test]
    fn search_shows_results_and_returns_to_the_label() {
        let (mut s, rx) = inbox_store(3);
        s.go_to(ViewKind::Label("STARRED".into()));
        drain(&rx);
        s.search("  from:zoë ");
        let search = ViewKey {
            account: ME.into(),
            kind: ViewKind::Search("from:zoë".into()),
        };
        assert_eq!(s.view, Some(search.clone()), "trimmed, sent verbatim");
        assert_eq!(s.search_query(), Some("from:zoë"));
        assert!(
            matches!(drain(&rx).last(), Some(Command::Want { view, more: false }) if *view == search)
        );
        assert_eq!(
            s.prefs.last_view,
            Some((ME.into(), "STARRED".into())),
            "searches are not remembered"
        );
        // A second search still returns to the label, not to the first search.
        s.search("ann");
        assert!(s.end_search());
        assert_eq!(s.view, Some(view("STARRED")));
        assert_eq!(s.search_query(), None);
        assert!(!s.end_search(), "nothing to leave");
        // An empty query leaves the search.
        s.search("x");
        s.search("   ");
        assert_eq!(s.view, Some(view("STARRED")));
        // Without a remembered view it falls back to the inbox.
        s.search("x");
        s.search_return = None;
        assert!(s.end_search());
        assert_eq!(s.view, Some(view("INBOX")));
    }

    #[test]
    fn set_label_toggles_and_updates_counts() {
        let (mut s, rx) = inbox_store(3);
        let mut travel = label("Label_2", LabelKind::User, 0, 0);
        travel.name = "Travel".into();
        let mut labels = s.labels[ME].clone();
        labels.push(travel);
        s.apply(SyncEvent::Labels {
            account: ME.into(),
            labels,
        });
        // t0 is unread.
        s.threads
            .get_mut(&key("t0"))
            .unwrap()
            .label_ids
            .insert("UNREAD".into());
        drain(&rx);
        let op = s.set_label(vec![key("t0")], "Label_2", true).unwrap();
        assert!(s.threads[&key("t0")].in_label("Label_2"));
        assert_eq!(label_count(&s, "Label_2"), (1, 1), "sidebar count follows");
        assert_eq!(s.unread_badge(ME, "Label_2"), Some(1));
        assert_eq!(
            s.notice.as_ref().map(|n| n.text.as_str()),
            Some("Added \u{201c}Travel\u{201d} to 1 conversation")
        );
        assert!(matches!(
            drain(&rx).as_slice(),
            [Command::Modify { op: o, delta, thread_ids, .. }]
                if *o == op && delta.add == ["Label_2"] && delta.remove.is_empty()
                    && thread_ids == &["t0"]
        ));
        assert_eq!(list(&s, &view("INBOX")), ids(0..3), "stays in the inbox");
        s.apply(SyncEvent::MutationFinished { op, error: None });
        // Removing it, then undoing the removal.
        s.set_label(vec![key("t0")], "Label_2", false).unwrap();
        assert!(!s.threads[&key("t0")].in_label("Label_2"));
        assert_eq!(label_count(&s, "Label_2"), (0, 0));
        assert_eq!(
            s.notice.as_ref().map(|n| n.text.as_str()),
            Some("Removed \u{201c}Travel\u{201d} from 1 conversation")
        );
        assert!(s.undo());
        assert!(s.threads[&key("t0")].in_label("Label_2"));
        // A failure rolls back.
        let (mut s2, _rx2) = inbox_store(2);
        let op = s2.set_label(vec![key("t1")], "Label_9", true).unwrap();
        s2.apply(SyncEvent::MutationFinished {
            op,
            error: Some("no".into()),
        });
        assert!(!s2.threads[&key("t1")].in_label("Label_9"));
        assert_eq!(
            s2.notice.as_ref().map(|n| n.text.as_str()),
            Some("Couldn't change labels: no")
        );
    }

    #[test]
    fn synced_events_are_counted() {
        let (mut s, _rx) = store();
        s.apply(SyncEvent::Synced(Timestamp::UNIX_EPOCH));
        s.apply(SyncEvent::Synced(Timestamp::UNIX_EPOCH));
        assert_eq!(s.syncs, 2);
    }

    #[test]
    fn mutate_threads_ignores_the_selection() {
        let (mut s, rx) = inbox_store(4);
        drain(&rx);
        s.toggle_select(key("t1"));
        s.toggle_select(key("t2"));
        // The row star acts on its own thread only.
        s.mutate_threads(vec![key("t3")], MutationKind::Labels(LabelDelta::star()));
        assert!(matches!(drain(&rx).as_slice(),
            [Command::Modify { thread_ids, delta, .. }]
                if thread_ids == &["t3"] && *delta == LabelDelta::star()));
        assert!(s.threads[&key("t3")].starred());
        assert!(!s.threads[&key("t1")].starred());
        assert!(!s.threads[&key("t2")].starred());
        assert_eq!(s.targets(), [key("t1"), key("t2")], "the selection is kept");

        // An undoable kind posts its undo for that thread alone.
        s.mutate_threads(vec![key("t0")], MutationKind::Labels(LabelDelta::archive()));
        drain(&rx);
        assert_eq!(list(&s, &view("INBOX")), ["t1", "t2", "t3"]);
        assert_eq!(s.notice.as_ref().unwrap().action, Some(NoticeAction::Undo));
        assert!(s.undo());
        assert!(matches!(drain(&rx).as_slice(),
            [Command::Modify { thread_ids, .. }] if thread_ids == &["t0"]));
        assert_eq!(list(&s, &view("INBOX")), ids(0..4));
    }

    #[test]
    fn row_key_is_the_thread_shown() {
        let t = summary(7, &["INBOX"]);
        assert_eq!(Row::Thread(&t).key(), &key("t7"));
        assert_eq!(Row::Skeleton(key("t8")).key(), &key("t8"));
    }

    #[test]
    fn mark_read_on_open_is_not_undoable() {
        let (mut s, rx) = inbox_store(2);
        s.apply(SyncEvent::ThreadsUpdated(vec![summary(
            0,
            &["INBOX", "UNREAD"],
        )]));
        s.open_cursor();
        let cmds = drain(&rx);
        assert!(cmds.iter().any(|c| matches!(c, Command::OpenThread(_))));
        assert!(cmds.iter().any(|c| matches!(c,
            Command::Modify { delta, .. } if *delta == LabelDelta::read())));
        assert!(!s.threads[&key("t0")].unread());
        assert!(s.notice.is_none());
        // A fresh cached detail is not fetched again.
        s.apply(SyncEvent::ThreadDetail(ThreadDetail {
            key: key("t0"),
            subject: String::new(),
            label_ids: BTreeSet::new(),
            messages: vec![],
        }));
        s.open_cursor();
        assert!(drain(&rx).is_empty());
    }

    #[test]
    fn labels_for_orders_and_nests() {
        let (mut s, _rx) = store();
        let user = |id: &str, name: &str, unread| {
            let mut l = label(id, LabelKind::User, unread, 1);
            l.name = name.into();
            l
        };
        let mut hidden = user("Label_9", "Hidden", 0);
        hidden.hidden = true;
        s.apply(SyncEvent::Labels {
            account: ME.into(),
            labels: vec![
                label("TRASH", LabelKind::System, 0, 1),
                label("INBOX", LabelKind::System, 2, 9),
                label("DRAFT", LabelKind::System, 0, 3),
                label("CATEGORY_SOCIAL", LabelKind::System, 4, 4),
                user("Label_3", "Projects/Website", 0),
                user("Label_2", "Projects", 0),
                user("Label_1", "Receipts", 1),
                user("Label_4", "Orphan/Child", 0),
                hidden,
            ],
        });
        let entries = s.labels_for(ME);
        let names: Vec<(&str, usize, Option<u32>)> = entries
            .iter()
            .map(|e| (e.name.as_str(), e.depth, e.count))
            .collect();
        assert_eq!(
            names,
            [
                ("Inbox", 0, Some(2)),
                ("Starred", 0, None),
                ("Important", 0, None),
                ("Sent", 0, None),
                ("Drafts", 0, Some(3)),
                ("All Mail", 0, None),
                ("Spam", 0, None),
                ("Trash", 0, None),
                ("Orphan/Child", 0, None),
                ("Projects", 0, None),
                ("Website", 1, None),
                ("Receipts", 0, Some(1)),
            ]
        );
    }

    // ----- AI triage (SPEC 16.10, 16.15) ----------------------------------------------------

    mod ai {
        use std::sync::mpsc;

        use super::*;
        use crate::ai::jev::{Kind, THRESHOLDS, verdict_from};
        use crate::ai::{AiCommand, AiEvent, AiHandle, KeyState, Outcome, Priority};
        use crate::demo_data::{self, PERSONAL, WORK};

        struct Rig {
            s: Store,
            sync: mpsc::Receiver<Command>,
            ai: mpsc::Receiver<AiCommand>,
        }

        fn prefs(opted: &[&str], background: bool) -> Prefs {
            let mut p = Prefs {
                ephemeral: true,
                ..Prefs::default()
            };
            for a in opted {
                p.ai.accounts.insert(a.to_string(), DISCLOSURE_VERSION);
            }
            p.ai.background = background;
            p
        }

        fn rig(mode: Mode, prefs: Prefs) -> Rig {
            let (tx, sync) = mpsc::channel();
            let (atx, ai) = mpsc::channel();
            let mut s = Store::for_tests_with_ai(
                SyncHandle::from_sender(tx),
                AiHandle::from_sender(atx),
                mode,
                prefs,
            );
            s.apply(SyncEvent::Accounts(vec![PERSONAL.into(), WORK.into()]));
            let r = Rig { s, sync, ai };
            r.drain();
            r
        }

        /// A Google-mode store with a saved key and these accounts opted in.
        fn ready(opted: &[&str], background: bool) -> Rig {
            let mut r = rig(Mode::Google, prefs(opted, background));
            r.s.apply_ai(AiEvent::Key(KeyState::Saved {
                hint: "sk-or-…abcd".into(),
            }));
            r
        }

        impl Rig {
            fn drain(&self) -> (Vec<Command>, Vec<AiCommand>) {
                (self.sync.try_iter().collect(), self.ai.try_iter().collect())
            }

            fn checks(&self) -> Vec<CheckRequest> {
                self.ai
                    .try_iter()
                    .filter_map(|c| match c {
                        AiCommand::Check(req) => Some(*req),
                        _ => None,
                    })
                    .collect()
            }

            fn prefetches(&self) -> Vec<ThreadKey> {
                self.sync
                    .try_iter()
                    .filter_map(|c| match c {
                        Command::Prefetch(k) => Some(k),
                        _ => None,
                    })
                    .collect()
            }

            /// Opens a demo thread and delivers its detail.
            fn open(&mut self, id: &str) -> ThreadDetail {
                let d = demo(id);
                self.s.open_thread(d.key.clone());
                self.s.apply(SyncEvent::ThreadDetail(d.clone()));
                d
            }

            fn checked(&mut self, d: &ThreadDetail, p: (f32, f32, f32)) {
                let target = self.s.ai_target(d).unwrap().id.clone();
                self.s.apply_ai(AiEvent::Checked {
                    thread: d.key.clone(),
                    message_id: target,
                    outcome: Outcome {
                        verdict: Some(verdict_from(p.0, p.1, p.2, &THRESHOLDS)),
                        labels: None,
                    },
                });
            }
        }

        fn demo(id: &str) -> ThreadDetail {
            demo_data::generate(Timestamp::now())
                .threads
                .into_iter()
                .find(|t| t.0.key.id == id)
                .unwrap_or_else(|| panic!("no {id}"))
                .1
        }

        fn new_mail(r: &mut Rig, ids: &[&str]) {
            let threads = ids
                .iter()
                .map(|id| demo_data::summary_of(&demo(id), 1))
                .collect();
            r.s.apply(SyncEvent::NewMail {
                account: PERSONAL.into(),
                threads,
            });
        }

        #[test]
        fn ai_checks_only_when_opted_in_with_key() {
            // No account opted in: no Check, even with a key.
            let mut r = ready(&[], false);
            r.open("p-ai-phish");
            assert!(r.checks().is_empty());
            // Opted in but no key yet.
            let mut r = rig(Mode::Google, prefs(&[PERSONAL], false));
            r.open("p-ai-phish");
            assert!(r.checks().is_empty());
            // Both: one interactive check of the target.
            let mut r = ready(&[PERSONAL], false);
            let d = r.open("p-ai-phish");
            let checks = r.checks();
            assert_eq!(checks.len(), 1);
            assert_eq!(checks[0].priority, Priority::Interactive);
            assert_eq!(checks[0].message_id, d.messages[0].id);
            assert!(checks[0].verdict && checks[0].labels.is_none());
            // Another account's thread is not checked.
            let mut work = ready(&[WORK], false);
            work.open("p-ai-phish");
            assert!(work.checks().is_empty());
        }

        /// A ready store with PERSONAL opted in, its demo labels loaded and `id` open with its
        /// summary listed, so label changes apply optimistically.
        fn labelled(id: &str) -> (Rig, ThreadDetail) {
            let mut r = ready(&[PERSONAL], false);
            r.s.apply(SyncEvent::Labels {
                account: PERSONAL.into(),
                labels: demo_data::label_defs(PERSONAL),
            });
            let d = demo(id);
            r.s.apply(SyncEvent::ThreadsUpdated(vec![demo_data::summary_of(
                &d, 1,
            )]));
            r.drain();
            let d = r.open(id);
            (r, d)
        }

        fn fp(r: &Rig) -> u64 {
            prompt::label_fingerprint(&r.s.labels[PERSONAL])
        }

        fn checked_labels(
            r: &mut Rig,
            d: &ThreadDetail,
            p: (f32, f32, f32),
            labels: &[(&str, f32)],
        ) {
            let target = r.s.ai_target(d).unwrap().id.clone();
            let fp = fp(r);
            r.s.apply_ai(AiEvent::Checked {
                thread: d.key.clone(),
                message_id: target,
                outcome: Outcome {
                    verdict: Some(verdict_from(p.0, p.1, p.2, &THRESHOLDS)),
                    labels: Some((
                        Some(fp),
                        labels.iter().map(|(id, p)| (id.to_string(), *p)).collect(),
                    )),
                },
            });
        }

        fn names(s: &Store, key: &ThreadKey) -> Vec<String> {
            s.suggestions(key).into_iter().map(|s| s.name).collect()
        }

        #[test]
        fn open_asks_labels_while_suggestions_are_on() {
            let (r, _) = labelled("p-ai-order");
            let checks = r.checks();
            assert_eq!(checks.len(), 1);
            let ask = checks[0].labels.as_ref().expect("labels asked on open");
            assert!(checks[0].verdict);
            assert_eq!(ask.fingerprint, fp(&r));
            let names: Vec<&str> = ask.candidates.iter().map(|c| c.name.as_str()).collect();
            assert!(names.contains(&"Receipts") && names.contains(&"Projects/Eloquawk"));
            // Off: no label names leave.
            let mut p = prefs(&[PERSONAL], false);
            p.ai.suggest_labels = false;
            let mut r = rig(Mode::Google, p);
            r.s.apply_ai(AiEvent::Key(KeyState::Saved { hint: "h".into() }));
            r.s.apply(SyncEvent::Labels {
                account: PERSONAL.into(),
                labels: demo_data::label_defs(PERSONAL),
            });
            r.open("p-ai-order");
            let checks = r.checks();
            assert_eq!(checks.len(), 1);
            assert!(checks[0].labels.is_none());
        }

        #[test]
        fn apply_suggestion_is_undoable_and_hides_chip() {
            let (mut r, d) = labelled("p-ai-order");
            r.drain();
            checked_labels(
                &mut r,
                &d,
                (0.03, 0.02, 0.05),
                &[(demo_data::RECEIPTS, 0.88), (demo_data::TRAVEL, 0.08)],
            );
            assert_eq!(names(&r.s, &d.key), ["Receipts"]);
            assert!(r.s.apply_suggestion(1).is_none(), "no second suggestion");
            let op = r.s.apply_suggestion(0).expect("applied");
            assert!(r.s.threads[&d.key].label_ids.contains(demo_data::RECEIPTS));
            assert!(names(&r.s, &d.key).is_empty(), "the chip disappears");
            let notice = r.s.notice.clone().unwrap();
            assert_eq!(
                notice.text,
                "Added \u{201c}Receipts\u{201d} to 1 conversation"
            );
            assert_eq!(notice.action, Some(NoticeAction::Undo));
            let (sync, ai) = r.drain();
            assert!(
                sync.iter()
                    .any(|c| matches!(c, Command::Modify { op: o, .. } if *o == op)),
                "the label change reaches the worker"
            );
            assert!(
                !ai.iter().any(|c| matches!(c, AiCommand::Check(_))),
                "no new check"
            );
            // z: the label goes and the chip comes back, still without a request.
            assert!(r.s.undo());
            assert!(!r.s.threads[&d.key].label_ids.contains(demo_data::RECEIPTS));
            assert_eq!(names(&r.s, &d.key), ["Receipts"]);
            assert!(r.checks().is_empty());
            // ✕ hides the row for the session.
            r.s.dismiss_suggestions();
            assert!(names(&r.s, &d.key).is_empty());
            assert!(r.s.apply_suggestion(0).is_none());
        }

        #[test]
        fn background_answer_without_labels_asks_them_for_the_open_thread() {
            let (mut r, d) = labelled("p-ai-order");
            r.drain();
            // The background check was already in flight when the thread opened.
            let target = r.s.ai_target(&d).unwrap().id.clone();
            r.s.apply_ai(AiEvent::Checking {
                thread: d.key.clone(),
                message_id: target,
            });
            r.checked(&d, (0.03, 0.02, 0.05));
            let checks = r.checks();
            assert_eq!(checks.len(), 1, "the labels go out without a reopen");
            assert!(!checks[0].verdict, "labels only");
            assert!(checks[0].labels.is_some());
            // Once labelled, an answer asks nothing more.
            checked_labels(
                &mut r,
                &d,
                (0.03, 0.02, 0.05),
                &[(demo_data::RECEIPTS, 0.88)],
            );
            assert!(r.checks().is_empty());
        }

        #[test]
        fn label_rename_reasks_labels_only() {
            let (mut r, d) = labelled("p-ai-order");
            let first = r.checks();
            assert!(first[0].verdict);
            checked_labels(
                &mut r,
                &d,
                (0.03, 0.02, 0.05),
                &[(demo_data::RECEIPTS, 0.88)],
            );
            // Reopening asks nothing; counts arriving change nothing.
            r.s.close_thread();
            r.s.open_thread(d.key.clone());
            assert!(r.checks().is_empty());
            let mut counted = demo_data::label_defs(PERSONAL);
            for l in &mut counted {
                l.threads_total = 3;
            }
            r.s.apply(SyncEvent::LabelCounts {
                account: PERSONAL.into(),
                labels: counted,
            });
            r.s.close_thread();
            r.s.open_thread(d.key.clone());
            assert!(r.checks().is_empty());
            // A rename changes the fingerprint: labels only, no triage questions.
            let mut renamed = demo_data::label_defs(PERSONAL);
            renamed
                .iter_mut()
                .find(|l| l.id == demo_data::RECEIPTS)
                .unwrap()
                .name = "Purchases".into();
            r.s.apply(SyncEvent::Labels {
                account: PERSONAL.into(),
                labels: renamed,
            });
            r.s.close_thread();
            r.s.open_thread(d.key.clone());
            let again = r.checks();
            assert_eq!(again.len(), 1);
            assert!(!again[0].verdict, "labels only");
            let ask = again[0].labels.as_ref().unwrap();
            assert_ne!(
                ask.fingerprint,
                first[0].labels.as_ref().unwrap().fingerprint
            );
            assert!(ask.candidates.iter().any(|c| c.name == "Purchases"));
            // Until the answer lands there is no row, and 1 does nothing (SPEC 16.12.3).
            let target = r.s.ai_target(&d).unwrap().id.clone();
            r.s.apply_ai(AiEvent::Checking {
                thread: d.key.clone(),
                message_id: target.clone(),
            });
            assert!(names(&r.s, &d.key).is_empty());
            assert!(r.s.apply_suggestion(0).is_none());
            assert!(!r.s.threads[&d.key].label_ids.contains(demo_data::RECEIPTS));
            // A failed labels-only re-ask shows no failure and asks again on the next open.
            r.s.apply_ai(AiEvent::Failed {
                thread: d.key.clone(),
                message_id: target.clone(),
                error: "busy".into(),
                retryable: false,
            });
            assert!(r.s.ai_failure(&d.key).is_none());
            r.s.close_thread();
            r.s.open_thread(d.key.clone());
            let again = r.checks();
            assert_eq!(again.len(), 1);
            assert!(!again[0].verdict, "labels only, again");
            let ask = again[0].labels.as_ref().unwrap();
            r.s.apply_ai(AiEvent::Checking {
                thread: d.key.clone(),
                message_id: target.clone(),
            });
            // The labels-only answer keeps the stored verdict.
            r.s.apply_ai(AiEvent::Checked {
                thread: d.key.clone(),
                message_id: target.clone(),
                outcome: Outcome {
                    verdict: None,
                    labels: Some((Some(ask.fingerprint), vec![])),
                },
            });
            let res = &r.s.ai.results[&(PERSONAL.to_string(), target)];
            assert!(res.verdict.is_some());
            assert_eq!(res.label_fp, Some(ask.fingerprint));
            assert!(names(&r.s, &d.key).is_empty());
        }

        #[test]
        fn danger_message_has_no_suggestions() {
            let (mut r, d) = labelled("p-ai-phish");
            checked_labels(
                &mut r,
                &d,
                (0.97, 0.02, 0.05),
                &[(demo_data::RECEIPTS, 0.95)],
            );
            assert!(r.s.suggestions(&d.key).is_empty());
            assert!(r.s.apply_suggestion(0).is_none());
            assert!(!r.s.threads[&d.key].label_ids.contains(demo_data::RECEIPTS));
            // The probabilities are still kept.
            let target = r.s.ai_target(&d).unwrap().id.clone();
            assert!(
                r.s.ai.results[&(PERSONAL.to_string(), target)]
                    .labels
                    .is_some()
            );
            // A Warning hides them too, and so does a local caution under a clean verdict.
            let (mut r, d) = labelled("p-ai-order");
            checked_labels(
                &mut r,
                &d,
                (0.6, 0.02, 0.05),
                &[(demo_data::RECEIPTS, 0.95)],
            );
            assert!(r.s.suggestions(&d.key).is_empty());
            let (mut r, d) = labelled("p-ai-notice");
            checked_labels(
                &mut r,
                &d,
                (0.04, 0.02, 0.05),
                &[(demo_data::RECEIPTS, 0.92)],
            );
            assert!(r.s.local_caution(&d.key).is_some());
            assert!(r.s.suggestions(&d.key).is_empty());
            // Spam level is not risky: suggestions show.
            let (mut r, d) = labelled("p-ai-order");
            checked_labels(
                &mut r,
                &d,
                (0.03, 0.02, 0.9),
                &[(demo_data::RECEIPTS, 0.95)],
            );
            assert_eq!(names(&r.s, &d.key), ["Receipts"]);
        }

        #[test]
        fn ai_disclosure_version_bump_turns_off() {
            let mut p = prefs(&[], false);
            p.ai.accounts
                .insert(PERSONAL.into(), DISCLOSURE_VERSION - 1);
            let (atx, arx) = mpsc::channel();
            let (tx, _rx) = mpsc::channel();
            let mut s = Store::for_tests_with_ai(
                SyncHandle::from_sender(tx),
                AiHandle::from_sender(atx),
                Mode::Google,
                p,
            );
            let sent: Vec<AiCommand> = arx.try_iter().collect();
            assert!(matches!(&sent[..], [AiCommand::ForgetAccount(a)] if a == PERSONAL));
            assert!(!s.ai_opted_in(PERSONAL));
            s.apply_ai(AiEvent::Key(KeyState::Saved { hint: "h".into() }));
            s.apply(SyncEvent::Accounts(vec![PERSONAL.into()]));
            let d = demo("p-ai-phish");
            s.open_thread(d.key.clone());
            s.apply(SyncEvent::ThreadDetail(d));
            assert!(!arx.try_iter().any(|c| matches!(c, AiCommand::Check(_))));
        }

        /// Upgrading from M16: a consent given to the version 1 text no longer counts, the
        /// worker forgets the account, and turning it on again stores the current version.
        #[test]
        fn m16_consent_needs_the_label_disclosure() {
            let mut p = prefs(&[], false);
            p.ai.accounts.insert(PERSONAL.into(), 1);
            let mut r = rig(Mode::Google, p);
            r.s.apply_ai(AiEvent::Key(KeyState::Saved { hint: "h".into() }));
            assert!(!r.s.ai_opted_in(PERSONAL));
            assert_eq!(
                r.s.prefs.ai.accounts.get(PERSONAL),
                Some(&1),
                "shown as changed"
            );
            r.open("p-ai-phish");
            assert!(r.checks().is_empty());
            r.s.set_ai_consent(PERSONAL, true);
            assert_eq!(
                r.s.prefs.ai.accounts.get(PERSONAL),
                Some(&DISCLOSURE_VERSION)
            );
            assert!(r.s.ai_opted_in(PERSONAL));
        }

        #[test]
        fn ai_never_checks_self_sent_or_draft() {
            let mut r = ready(&[PERSONAL], false);
            r.open("p-sent");
            r.open("p-draft");
            assert!(r.checks().is_empty());
            // A thread whose last message is the user's checks the last inbound one.
            let d = r.open("p-cafe");
            let checks = r.checks();
            assert_eq!(checks.len(), 1);
            let zoe = d
                .messages
                .iter()
                .rev()
                .find(|m| !m.label_ids.contains(labels::SENT));
            assert_eq!(checks[0].message_id, zoe.unwrap().id);
        }

        #[test]
        fn ai_fast_open_sends_latest_only() {
            let mut r = ready(&[PERSONAL], false);
            let a = r.open("p-ai-phish");
            r.open("p-ai-scam");
            let target = r.s.ai_target(&a).unwrap().id.clone();
            r.s.apply_ai(AiEvent::Checking {
                thread: a.key.clone(),
                message_id: target,
            });
            // Back to A while it is still checking: no second request.
            r.s.open_thread(a.key.clone());
            let checks = r.checks();
            assert_eq!(
                checks
                    .iter()
                    .map(|c| c.thread.id.as_str())
                    .collect::<Vec<_>>(),
                ["p-ai-phish", "p-ai-scam"]
            );
        }

        #[test]
        fn ai_results_dropped_after_opt_out() {
            let mut r = ready(&[PERSONAL], false);
            let d = r.open("p-ai-phish");
            r.checked(&d, (0.97, 0.0, 0.0));
            assert!(r.s.ai_banner(&d.key).is_some());
            r.drain();
            r.s.change_prefs(|p| {
                p.ai.accounts.remove(PERSONAL);
            });
            let (_, ai) = r.drain();
            assert!(
                ai.iter()
                    .any(|c| matches!(c, AiCommand::ForgetAccount(a) if a == PERSONAL))
            );
            assert!(r.s.ai_banner(&d.key).is_none());
            assert!(r.s.ai.results.is_empty());
            // A late Checked for an opted-out account is dropped.
            r.checked(&d, (0.97, 0.0, 0.0));
            assert!(r.s.ai.results.is_empty());
        }

        #[test]
        fn ai_spam_level_hidden_in_spam_view() {
            let mut r = ready(&[PERSONAL], false);
            let d = r.open("p-ai-spam");
            r.checked(&d, (0.0, 0.0, 0.9));
            assert_eq!(r.s.ai_badge(&d.key), Some(Level::Spam));
            assert_eq!(r.s.ai_banner(&d.key).map(|v| v.level), Some(Level::Spam));
            r.s.set_view(ViewKey {
                account: PERSONAL.into(),
                kind: ViewKind::Label(labels::SPAM.into()),
            });
            assert_eq!(r.s.ai_badge(&d.key), None);
            assert!(r.s.ai_banner(&d.key).is_none());
            // Phishing still shows there.
            let p = r.open("p-ai-phish");
            r.checked(&p, (0.97, 0.0, 0.0));
            assert_eq!(r.s.ai_banner(&p.key).map(|v| v.kind), Some(Kind::Phishing));
        }

        #[test]
        fn local_caution_and_guard_without_opt_in() {
            // No consent and no key at all.
            let mut r = rig(Mode::Google, prefs(&[], false));
            let d = r.open("p-ai-notice");
            assert!(r.checks().is_empty());
            let caution = r.s.local_caution(&d.key).unwrap();
            assert_eq!(caution[0], "Sender failed DMARC");
            assert!(caution[1].contains("paypa1-secure.example"));
            let target = d.messages.last().unwrap().id.clone();
            assert!(r.s.link_guard(&d.key, &target));
            assert!(!r.s.link_guard(&d.key, "another-message"));
            let clean = r.open("p-cafe");
            assert!(r.s.local_caution(&clean.key).is_none());
            let id = clean.messages.last().unwrap().id.clone();
            assert!(!r.s.link_guard(&clean.key, &id));
        }

        #[test]
        fn guard_follows_a_danger_verdict() {
            let mut r = ready(&[PERSONAL], false);
            let d = r.open("p-ai-scam");
            let id = d.messages[0].id.clone();
            assert!(!r.s.link_guard(&d.key, &id));
            r.checked(&d, (0.0, 0.91, 0.0));
            assert!(r.s.link_guard(&d.key, &id));
            // A dismissed banner still guards; a warning does not.
            r.s.dismiss_triage();
            assert!(r.s.ai_banner(&d.key).is_none());
            assert!(r.s.link_guard(&d.key, &id));
            let n = r.open("p-news");
            r.checked(&n, (0.62, 0.0, 0.0));
            assert!(!r.s.link_guard(&n.key, &n.messages[0].id));
        }

        #[test]
        fn consent_kept_when_accounts_event_drops_an_account() {
            let mut r = ready(&[PERSONAL, WORK], false);
            let d = r.open("p-ai-phish");
            r.checked(&d, (0.97, 0.0, 0.0));
            r.s.apply(SyncEvent::Accounts(vec![WORK.into()]));
            assert!(r.s.ai_opted_in(PERSONAL), "consent survives");
            assert!(r.s.ai.results.is_empty(), "runtime state goes");
            r.drain();
            r.s.remove_account(WORK);
            assert!(!r.s.ai_opted_in(WORK));
            let (sync, ai) = r.drain();
            assert!(
                sync.iter()
                    .any(|c| matches!(c, Command::RemoveAccount(a) if a == WORK))
            );
            assert!(
                ai.iter()
                    .any(|c| matches!(c, AiCommand::ForgetAccount(a) if a == WORK))
            );
            r.s.clear_client();
            assert!(r.s.prefs.ai.accounts.is_empty());
        }

        #[test]
        fn first_reload_after_auth_leaves_starting() {
            let r = rig(Mode::Google, prefs(&[], false));
            let mut s = r.s;
            s.apply(SyncEvent::Auth(AuthState::Starting));
            assert!(r.ai.try_iter().next().is_none());
            s.apply(SyncEvent::Auth(AuthState::Locked));
            s.apply(SyncEvent::Auth(AuthState::Ready));
            let sent: Vec<AiCommand> = r.ai.try_iter().collect();
            assert_eq!(sent.len(), 1);
            assert!(matches!(sent[0], AiCommand::Reload));
            assert_eq!(s.ai.key, KeyState::Unknown);
        }

        #[test]
        fn refresh_also_reloads_the_key() {
            let r = rig(Mode::Google, prefs(&[], false));
            r.s.refresh();
            let (sync, ai) = r.drain();
            assert!(matches!(&sync[..], [Command::Refresh]));
            assert!(matches!(&ai[..], [AiCommand::Reload]));
        }

        #[test]
        fn spend_limit_change_reaches_the_worker() {
            let mut r = ready(&[], false);
            r.s.change_prefs(|p| p.ai.spend_limit_cents = 250);
            let (_, ai) = r.drain();
            assert!(matches!(&ai[..], [AiCommand::SpendLimit(250)]));
            r.s.change_prefs(|p| p.ai.spend_limit_cents = 1);
            let (_, ai) = r.drain();
            assert!(matches!(&ai[..], [AiCommand::SpendLimit(10)]), "clamped");
        }

        #[test]
        fn dropped_clears_checking_and_reopen_rechecks() {
            let mut r = ready(&[PERSONAL], false);
            let d = r.open("p-ai-phish");
            assert_eq!(r.checks().len(), 1);
            let id = d.messages[0].id.clone();
            r.s.apply_ai(AiEvent::Checking {
                thread: d.key.clone(),
                message_id: id.clone(),
            });
            assert!(r.s.ai_checking(&d.key));
            r.s.apply_ai(AiEvent::Dropped {
                thread: d.key.clone(),
                message_id: id,
            });
            assert!(!r.s.ai_checking(&d.key));
            r.s.open_thread(d.key.clone());
            assert_eq!(r.checks().len(), 1);
        }

        #[test]
        fn failures_retry_only_when_retryable() {
            let mut r = ready(&[PERSONAL], false);
            let d = r.open("p-ai-fail");
            r.checks();
            let id = d.messages[0].id.clone();
            r.s.apply_ai(AiEvent::Failed {
                thread: d.key.clone(),
                message_id: id.clone(),
                error: "Couldn't check this message".into(),
                retryable: false,
            });
            r.s.open_thread(d.key.clone());
            assert!(r.checks().is_empty());
            r.s.apply_ai(AiEvent::Failed {
                thread: d.key.clone(),
                message_id: id,
                error: "Couldn't check this message".into(),
                retryable: true,
            });
            assert_eq!(r.s.ai_failure(&d.key).map(|f| f.1), Some(true));
            r.s.retry_ai();
            assert_eq!(r.checks().len(), 1);
            assert!(r.s.ai_failure(&d.key).is_none());
        }

        #[test]
        fn paused_clears_checking() {
            let mut r = ready(&[PERSONAL], false);
            let d = r.open("p-ai-phish");
            r.s.apply_ai(AiEvent::Checking {
                thread: d.key.clone(),
                message_id: d.messages[0].id.clone(),
            });
            r.s.apply_ai(AiEvent::Paused(Some(
                "Your OpenRouter credit has run out".into(),
            )));
            assert!(r.s.ai.checking.is_empty());
            r.checks();
            // While paused, opening sends nothing.
            r.open("p-ai-scam");
            assert!(r.checks().is_empty());
            r.s.apply_ai(AiEvent::Paused(None));
            r.s.open_thread(d.key.clone());
            assert_eq!(r.checks().len(), 1);
        }

        #[test]
        fn ai_rev_bumps_on_prefs_key_and_pause() {
            let mut r = ready(&[], false);
            let mut last = r.s.ai.ai_rev;
            let mut bumped = |s: &Store| {
                let b = s.ai.ai_rev > last;
                last = s.ai.ai_rev;
                b
            };
            r.s.change_prefs(|p| p.ai.background = true);
            assert!(bumped(&r.s));
            r.s.change_prefs(|p| p.clock_24h = !p.clock_24h);
            assert!(!bumped(&r.s));
            r.s.apply_ai(AiEvent::Key(KeyState::Locked));
            assert!(bumped(&r.s));
            r.s.apply_ai(AiEvent::Paused(Some("x".into())));
            assert!(bumped(&r.s));
        }

        #[test]
        fn background_prefetch_respects_hourly_cap() {
            let mut r = ready(&[PERSONAL], true);
            let now = Instant::now();
            r.s.ai.hourly.insert(
                PERSONAL.into(),
                std::iter::repeat_n(now, BACKGROUND_PER_HOUR - 1).collect(),
            );
            new_mail(&mut r, &["p-ai-phish", "p-ai-scam"]);
            assert_eq!(r.prefetches().len(), 1);
            new_mail(&mut r, &["p-ai-spam"]);
            assert!(r.prefetches().is_empty());
            // An hour later there is room again.
            r.s.ai.hourly.insert(
                PERSONAL.into(),
                std::iter::repeat_n(now - HOUR, BACKGROUND_PER_HOUR).collect(),
            );
            new_mail(&mut r, &["p-ai-spam"]);
            assert_eq!(r.prefetches().len(), 1);
        }

        #[test]
        fn prefetch_detail_checked_but_not_cached() {
            let mut r = ready(&[PERSONAL], true);
            new_mail(&mut r, &["p-ai-phish"]);
            let key = ThreadKey::new(PERSONAL, "p-ai-phish");
            assert_eq!(r.prefetches(), std::slice::from_ref(&key));
            r.s.apply(SyncEvent::ThreadDetail(demo("p-ai-phish")));
            assert!(!r.s.details.contains_key(&key));
            let checks = r.checks();
            assert_eq!(checks.len(), 1);
            assert_eq!(checks[0].priority, Priority::Background);
            assert!(checks[0].labels.is_none());
            assert!(r.s.ai.prefetching.is_empty());
        }

        #[test]
        fn prefetch_survives_the_synced_after_new_mail() {
            let mut r = ready(&[PERSONAL], true);
            new_mail(&mut r, &["p-ai-phish"]);
            r.s.apply(SyncEvent::Synced(Timestamp::now()));
            assert_eq!(r.s.ai.prefetching.len(), 1);
            r.s.apply(SyncEvent::ThreadDetail(demo("p-ai-phish")));
            assert_eq!(r.checks().len(), 1);
            // One older than five minutes goes on the next Synced.
            r.s.ai.prefetching.insert(
                ThreadKey::new(PERSONAL, "old"),
                Instant::now() - PREFETCH_TTL,
            );
            r.s.apply(SyncEvent::Synced(Timestamp::now()));
            assert!(r.s.ai.prefetching.is_empty());
        }

        #[test]
        fn background_off_drops_pending_prefetch() {
            let mut r = ready(&[PERSONAL], true);
            new_mail(&mut r, &["p-ai-phish"]);
            r.drain();
            r.s.change_prefs(|p| p.ai.background = false);
            assert!(r.s.ai.prefetching.is_empty());
            r.s.apply(SyncEvent::ThreadDetail(demo("p-ai-phish")));
            assert!(r.checks().is_empty());
        }

        #[test]
        fn prefetch_rechecks_consent_on_arrival() {
            for change in 0..3 {
                let mut r = ready(&[PERSONAL], true);
                new_mail(&mut r, &["p-ai-phish"]);
                match change {
                    0 => r.s.change_prefs(|p| {
                        p.ai.accounts.clear();
                    }),
                    1 => r.s.apply_ai(AiEvent::Key(KeyState::Missing)),
                    _ => r.s.apply_ai(AiEvent::Paused(Some("x".into()))),
                }
                r.drain();
                r.s.apply(SyncEvent::ThreadDetail(demo("p-ai-phish")));
                assert!(r.checks().is_empty(), "change {change}");
            }
        }

        #[test]
        fn for_tests_sends_no_prefetch() {
            let (mut s, rx) = super::store();
            s.apply(SyncEvent::NewMail {
                account: ME.into(),
                threads: vec![summary(1, &["INBOX", "UNREAD"])],
            });
            assert!(!rx.try_iter().any(|c| matches!(c, Command::Prefetch(_))));
        }

        #[test]
        fn late_open_detail_not_checked_in_background() {
            let mut r = ready(&[PERSONAL], true);
            let a = demo("p-ai-phish");
            r.s.open_thread(a.key.clone());
            r.open("p-ai-scam");
            r.checks();
            // A's detail lands after the user moved on: cached, not checked.
            r.s.apply(SyncEvent::ThreadDetail(a.clone()));
            assert!(r.s.details.contains_key(&a.key));
            assert!(r.checks().is_empty());
        }

        #[test]
        fn no_prefetch_without_key_or_while_paused() {
            let mut r = rig(Mode::Google, prefs(&[PERSONAL], true));
            new_mail(&mut r, &["p-ai-phish"]);
            assert!(r.prefetches().is_empty());
            r.s.apply_ai(AiEvent::Key(KeyState::Saved { hint: "h".into() }));
            r.s.apply_ai(AiEvent::Paused(Some("x".into())));
            new_mail(&mut r, &["p-ai-phish"]);
            assert!(r.prefetches().is_empty());
            r.s.apply_ai(AiEvent::Paused(None));
            new_mail(&mut r, &["p-ai-phish"]);
            assert_eq!(r.prefetches().len(), 1);
        }

        #[test]
        fn key_removed_clears_results() {
            let mut r = ready(&[PERSONAL], false);
            let d = r.open("p-ai-phish");
            r.checked(&d, (0.97, 0.0, 0.0));
            assert!(r.s.ai_badge(&d.key).is_some());
            r.s.apply_ai(AiEvent::Key(KeyState::Missing));
            assert!(r.s.ai.results.is_empty() && r.s.ai.by_thread.is_empty());
            assert!(r.s.ai_badge(&d.key).is_none());
        }

        #[test]
        fn account_reset_forgets_the_account() {
            let mut r = ready(&[PERSONAL], false);
            let d = r.open("p-ai-phish");
            r.checked(&d, (0.97, 0.0, 0.0));
            r.drain();
            r.s.apply(SyncEvent::AccountReset(PERSONAL.into()));
            let (_, ai) = r.drain();
            assert!(
                ai.iter()
                    .any(|c| matches!(c, AiCommand::ForgetAccount(a) if a == PERSONAL))
            );
            assert!(r.s.ai.results.is_empty());
            assert!(r.s.ai_opted_in(PERSONAL));
        }

        #[test]
        fn results_are_bounded() {
            let mut r = ready(&[PERSONAL], false);
            for i in 0..AI_RESULTS + 5 {
                r.s.apply_ai(AiEvent::Checked {
                    thread: ThreadKey::new(PERSONAL, format!("t{i}")),
                    message_id: format!("m{i}"),
                    outcome: Outcome {
                        verdict: None,
                        labels: None,
                    },
                });
            }
            assert_eq!(r.s.ai.results.len(), AI_RESULTS);
            assert!(
                !r.s.ai
                    .results
                    .contains_key(&(PERSONAL.to_string(), "m0".to_string()))
            );
        }

        #[test]
        fn demo_store_opts_in_both_accounts() {
            let (tx, _rx) = mpsc::channel();
            let s = Store::new(
                SyncHandle::from_sender(tx),
                AiHandle::detached(),
                Mode::Demo,
                prefs(&[], false),
            );
            assert!(s.ai_opted_in(PERSONAL) && s.ai_opted_in(WORK));
            assert!(s.prefs.ai.background);
        }
    }

    /// Pumps worker events into the store until `done` or the deadline.
    fn pump(
        s: &mut Store,
        rx: &async_channel::Receiver<SyncEvent>,
        timeout: Duration,
        mut done: impl FnMut(&Store, &SyncEvent) -> bool,
    ) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match rx.try_recv() {
                Ok(ev) => {
                    let ev2 = ev.clone();
                    s.apply(ev);
                    if done(s, &ev2) {
                        return true;
                    }
                }
                Err(_) => std::thread::sleep(Duration::from_millis(5)),
            }
        }
        false
    }

    fn finished(op: OpId) -> impl FnMut(&Store, &SyncEvent) -> bool {
        move |_, ev| matches!(ev, SyncEvent::MutationFinished { op: o, .. } if *o == op)
    }

    #[test]
    fn demo_end_to_end() {
        let prefs = Prefs {
            ephemeral: true,
            ..Prefs::default()
        };
        let (sync, events) = crate::demo::start(&prefs).unwrap();
        let (ai, ai_events) = crate::ai::start(Mode::Demo, None, &prefs).unwrap();
        let mut ai_prefs = prefs.clone();
        ai_prefs
            .ai
            .accounts
            .insert(ME.into(), crate::ai::DISCLOSURE_VERSION);
        let mut s = Store::for_tests_with_ai(sync.clone(), ai.clone(), Mode::Demo, ai_prefs);
        let page_size = s.prefs.page_size as usize;
        let inbox = view("INBOX");
        pump(&mut s, &events, Duration::from_secs(2), |s, _| {
            s.lists.get(&inbox).is_some_and(|l| !l.ids.is_empty())
        });
        assert_eq!(s.auth, AuthState::Demo);
        assert_eq!(s.accounts.len(), 2);
        assert!(s.unread_badge(ME, "INBOX").is_some_and(|n| n > 0));
        assert_eq!(s.view, Some(inbox.clone()));
        assert_eq!(s.visible_rows().len(), page_size);
        assert!(s.lists[&inbox].has_more);

        // Open the cursor row: a detail arrives and the thread is marked read.
        let first = s.cursor.clone().unwrap();
        assert!(s.threads[&first].unread());
        s.open_cursor();
        assert!(pump(&mut s, &events, Duration::from_secs(2), |s, _| {
            matches!(s.details.get(&first), Some(DetailState::Loaded(_)))
        }));
        assert!(!s.threads[&first].unread());
        assert!(pump(&mut s, &events, Duration::from_secs(2), |s, _| s
            .pending
            .is_empty()));
        assert!(!s.threads[&first].unread(), "the worker agrees");

        // Archive removes it from INBOX and the worker confirms.
        let index = s.cursor_index().unwrap();
        let op = s
            .mutate(MutationKind::Labels(LabelDelta::archive()))
            .unwrap();
        assert!(!s.lists[&inbox].ids.contains(&first.id));
        assert_ne!(s.open, Some(first.clone()));
        assert!(pump(&mut s, &events, Duration::from_secs(2), finished(op)));
        assert!(!s.threads[&first].in_label("INBOX"));

        // Undo puts it back where it was.
        assert!(s.undo());
        assert_eq!(
            s.lists[&inbox].ids.iter().position(|id| *id == first.id),
            Some(index)
        );
        assert!(pump(&mut s, &events, Duration::from_secs(2), |s, _| s
            .pending
            .is_empty()));
        assert!(s.threads[&first].in_label("INBOX"));
        assert!(s.lists[&inbox].ids.contains(&first.id));

        // A "[fail]" thread rolls back.
        let fail = s.lists[&inbox]
            .ids
            .iter()
            .map(|id| key(id))
            .find(|k| s.threads[k].subject.contains("[fail]"))
            .expect("a [fail] thread on the first page");
        let fail_index = s.lists[&inbox].ids.iter().position(|id| *id == fail.id);
        s.close_thread();
        s.toggle_select(fail.clone());
        let op = s
            .mutate(MutationKind::Labels(LabelDelta::archive()))
            .unwrap();
        assert!(!s.lists[&inbox].ids.contains(&fail.id));
        assert!(pump(&mut s, &events, Duration::from_secs(2), finished(op)));
        assert_eq!(
            s.lists[&inbox].ids.iter().position(|id| *id == fail.id),
            fail_index
        );
        assert!(s.threads[&fail].in_label("INBOX"));
        assert!(
            s.notice
                .as_ref()
                .unwrap()
                .text
                .starts_with("Couldn't archive")
        );

        // A search returns only matching rows.
        let search = ViewKey {
            account: ME.into(),
            kind: ViewKind::Search("from:zoë".into()),
        };
        s.set_view(search.clone());
        assert!(pump(&mut s, &events, Duration::from_secs(2), |s, _| {
            s.lists.get(&search).is_some_and(|l| !l.loading)
        }));
        let rows = s.visible_rows();
        assert!(!rows.is_empty());
        for row in rows {
            let Row::Thread(t) = row else {
                panic!("skeleton in a demo search");
            };
            assert!(
                t.participants.iter().any(|(a, _)| a
                    .name
                    .as_deref()
                    .unwrap_or("")
                    .to_lowercase()
                    .contains("zoë")),
                "{} does not match",
                t.key
            );
        }

        // Sending a reply appends to the thread, and the open detail is fetched again.
        let detail = match s.details.get(&first) {
            Some(DetailState::Loaded(d)) => d.clone(),
            other => panic!("no detail for the first thread: {other:?}"),
        };
        let before = detail.messages.len();
        let replied_to = crate::mail::reply::target(&detail).and_then(|m| m.message_id.clone());
        let out = crate::mail::reply::prefill(
            &detail,
            crate::model::ComposeKind::Reply,
            &Address::new(None, ME),
            &|_| "today".into(),
        )
        .unwrap();
        let op = s.send(out);
        assert_eq!(s.send_state(op), Some(&SendState::Sending));
        assert!(pump(&mut s, &events, Duration::from_secs(2), |s, _| {
            s.send_state(op) == Some(&SendState::Sent)
        }));
        assert_eq!(s.notice.as_ref().unwrap().text, "Message sent");
        assert!(pump(&mut s, &events, Duration::from_secs(2), |s, _| {
            matches!(s.details.get(&first), Some(DetailState::Loaded(d)) if d.messages.len() == before + 1)
        }));
        assert_eq!(s.threads[&first].message_count as usize, before + 1);
        let Some(DetailState::Loaded(d)) = s.details.get(&first) else {
            unreachable!()
        };
        let sent = d.messages.last().unwrap();
        assert!(sent.label_ids.contains(labels::SENT));
        assert_eq!(sent.in_reply_to, replied_to);
        assert!(sent.body.contains("wrote:"));
        assert!(s.threads[&first].in_label(labels::SENT));

        // AI triage with the offline checker (SPEC 16.15): the phishing demo is Danger and
        // arms the link guard.
        let open_and_check = |s: &mut Store, id: &str| -> (ThreadKey, String) {
            let key = key(id);
            s.open_thread(key.clone());
            assert!(pump(s, &events, Duration::from_secs(2), |s, _| {
                matches!(s.details.get(&key), Some(DetailState::Loaded(_)))
            }));
            let target = match s.details.get(&key) {
                Some(DetailState::Loaded(d)) => s.ai_target(d).unwrap().id.clone(),
                _ => unreachable!(),
            };
            let mk = (ME.to_string(), target.clone());
            assert!(
                pump_ai(s, &ai_events, Duration::from_secs(3), |s| {
                    s.ai.results.contains_key(&mk)
                }),
                "no verdict for {id}"
            );
            (key, target)
        };
        let (phish, target) = open_and_check(&mut s, "p-ai-phish");
        let v = s.ai_banner(&phish).copied().unwrap();
        assert_eq!(
            (v.level, v.kind),
            (Level::Danger, crate::ai::jev::Kind::Phishing)
        );
        assert!(s.link_guard(&phish, &target));
        assert_eq!(s.ai_badge(&phish), Some(Level::Danger));
        // The crafted notice talks the checker down, but the local caution still guards.
        let (notice, target) = open_and_check(&mut s, "p-ai-notice");
        let result = &s.ai.results[&(ME.to_string(), target.clone())];
        assert_eq!(result.verdict.map(|v| v.level), Some(Level::None));
        assert!(s.ai_banner(&notice).is_none());
        assert!(s.local_caution(&notice).is_some());
        assert!(s.link_guard(&notice, &target));
        assert!(s.suggestions(&notice).is_empty(), "risky mail gets none");
        // (M17) The order receipt suggests Receipts; applying it files the thread and the
        // chip goes, and undo brings both back.
        let (order, _) = open_and_check(&mut s, "p-ai-order");
        let names = |s: &Store| -> Vec<String> {
            s.suggestions(&order).into_iter().map(|x| x.name).collect()
        };
        assert_eq!(names(&s), ["Receipts"]);
        let op = s.apply_suggestion(0).expect("a suggestion to apply");
        assert!(pump(&mut s, &events, Duration::from_secs(2), finished(op)));
        assert!(
            s.threads[&order]
                .label_ids
                .contains(crate::demo_data::RECEIPTS)
        );
        assert!(names(&s).is_empty());
        assert!(s.undo());
        assert!(
            !s.threads[&order]
                .label_ids
                .contains(crate::demo_data::RECEIPTS)
        );
        assert_eq!(names(&s), ["Receipts"]);
        sync.send(Command::Shutdown);
        ai.send(AiCommand::Shutdown);
    }

    /// Pumps AI worker events into the store until `done` or the deadline.
    fn pump_ai(
        s: &mut Store,
        rx: &async_channel::Receiver<AiEvent>,
        timeout: Duration,
        done: impl Fn(&Store) -> bool,
    ) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match rx.try_recv() {
                Ok(ev) => {
                    s.apply_ai(ev);
                    if done(s) {
                        return true;
                    }
                }
                Err(_) => std::thread::sleep(Duration::from_millis(5)),
            }
        }
        false
    }
}
