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

use crate::config::Prefs;
use crate::mail::address::{self, ParticipantsLabel};
use crate::mail::query::Query;
use crate::model::{
    AccountId, Label, LabelDelta, LabelKind, Outgoing, ThreadDetail, ThreadKey, ThreadSummary,
    ViewKey, ViewKind, labels,
};
use crate::protocol::{AuthState, Command, OpId, PageMode, SendError, SyncEvent, SyncHandle};

/// How many thread details are kept in memory.
const DETAIL_CACHE: usize = 30;
/// Moving the cursor into this many last rows asks for the next page.
const MORE_THRESHOLD: usize = 10;
/// The label id `last_view` stores for All Mail, which is not a Gmail label.
const ALL_MAIL_ID: &str = "ALL_MAIL";

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
}

/// A transient message for the toast.
#[derive(Clone, Debug, PartialEq)]
pub struct Notice {
    pub text: String,
    pub action: Option<NoticeAction>,
    pub posted_at: Instant,
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
    pub fn new(sync: SyncHandle, mode: Mode, prefs: Prefs) -> Self {
        Self {
            mode,
            sync,
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
    #[cfg(test)]
    pub fn for_tests(sync: SyncHandle) -> Self {
        let prefs = Prefs {
            ephemeral: true,
            ..Prefs::default()
        };
        Self::new(sync, Mode::Demo, prefs)
    }

    pub fn send_command(&self, cmd: Command) {
        self.sync.send(cmd);
    }

    /// Changes preferences from Settings: applies `f`, pulls numbers back into range and
    /// saves, except when the prefs are ephemeral (demo mode, tests).
    /// A new refresh interval or page size reaches the worker straight away.
    pub fn change_prefs(&mut self, f: impl FnOnce(&mut Prefs)) {
        let before = (self.prefs.refresh_secs, self.prefs.page_size);
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
    /// when that preference is on (not undoable, no toast).
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
            SyncEvent::Auth(a) => self.auth = a,
            SyncEvent::Client(c) => self.client_id = c,
            SyncEvent::Accounts(a) => self.on_accounts(a),
            SyncEvent::SigningIn(s) => self.signing_in = s,
            SyncEvent::Busy(b) => self.busy = b,
            SyncEvent::Synced(t) => {
                self.last_sync = Some(t);
                self.syncs += 1;
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
            SyncEvent::ThreadDetail(detail) => {
                let key = detail.key.clone();
                if let Some(s) = self.threads.get(&key) {
                    self.detail_history.insert(key.clone(), s.history_id);
                }
                self.details
                    .insert(key.clone(), DetailState::Loaded(detail));
                self.touch_detail(&key);
            }
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
                if self.prefs.notifications {
                    self.new_mail
                        .extend(threads.into_iter().filter(|t| t.key.account == account));
                }
            }
            SyncEvent::AccountReset(account) => {
                self.lists.retain(|v, _| v.account != account);
                self.drop_threads(|k| k.account == account);
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
        let mut s = Store::for_tests(sync.clone());
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
        sync.send(Command::Shutdown);
    }
}
