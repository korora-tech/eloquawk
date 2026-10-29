//! The worker protocol (SPEC 5): the commands the UI sends to a worker and the events a worker
//! sends back. The demo worker (`demo.rs`) and the Google worker (`sync.rs`) share it.
//!
//! UI to worker is a `std::sync::mpsc` channel wrapped by [`SyncHandle`]; worker to UI is an
//! unbounded `async_channel`. `Command` is deliberately not `Debug` (it carries client secrets
//! and message bodies) and `SyncEvent` has a hand-written `Debug` that prints variant names,
//! ids and counts only, never mail content.

use std::fmt;
use std::sync::mpsc;

use jiff::Timestamp;

use crate::google::oauth::{AuthError, TokenResponse};
use crate::model::{
    AccountId, Attachment, Label, LabelDelta, Outgoing, ThreadDetail, ThreadKey, ThreadSummary,
    ViewKey,
};

/// Identifies one optimistic mutation or send. Allocated by the Store, monotonically
/// increasing.
pub type OpId = u64;

/// UI to worker. Deliberately not `Debug`: `SaveClient` carries a client secret and `Send` a
/// message body.
pub enum Command {
    /// `client_secret: None` keeps the stored secret (only valid for the same client id).
    SaveClient {
        client_id: String,
        client_secret: Option<String>,
    },
    /// Removes the saved client and every account.
    ClearClient,
    /// Starts a browser sign-in; the account chosen there is added (or re-authorised).
    AddAccount,
    CancelSignIn,
    /// Signs one account out: revokes its token and forgets it.
    RemoveAccount(AccountId),
    /// Re-lists what is visible; also retries a locked keyring.
    Refresh,
    /// The user dismissed the banner, including sticky problems.
    DismissProblems,
    /// The view the UI is showing; `more` asks for the next page.
    Want {
        view: ViewKey,
        more: bool,
    },
    OpenThread(ThreadKey),
    Modify {
        op: OpId,
        account: AccountId,
        thread_ids: Vec<String>,
        delta: LabelDelta,
    },
    Trash {
        op: OpId,
        account: AccountId,
        thread_ids: Vec<String>,
    },
    /// `restore` is applied with threads.modify after a successful untrash (undo, Restore).
    Untrash {
        op: OpId,
        account: AccountId,
        thread_ids: Vec<String>,
        restore: LabelDelta,
    },
    Send {
        op: OpId,
        outgoing: Outgoing,
    },
    #[allow(dead_code)] // consumed in M12
    SaveAttachment {
        account: AccountId,
        message_id: String,
        attachment: Attachment,
    },
    Shutdown,
    /// Posted by the sign-in thread to its own worker: the sign-in's sequence number and the
    /// token exchange's result. The demo ignores it.
    #[doc(hidden)]
    SignInFinished(u64, Result<TokenResponse, AuthError>),
}

/// What the worker knows about credentials.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthState {
    /// The worker has not read the keyring yet.
    Starting,
    /// The keyring could not be read (locked, prompt dismissed, no Secret Service).
    Locked,
    /// No saved client and none compiled in.
    NeedsClient,
    /// The client is known; see [`SyncEvent::Accounts`] for who is signed in.
    Ready,
    Demo,
}

/// How a [`SyncEvent::ThreadPage`] combines with the list the store already holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageMode {
    /// Page 1 of a newly requested view: replaces the list.
    Replace,
    /// The next page: appended, deduplicated by id.
    Append,
    /// Page 1 re-listed by a sync round: merged, keeping the loaded tail (SPEC 8.1).
    Refresh,
}

/// Why a send failed. `maybe_sent` means the outcome is unknown (network failure or 5xx), so
/// the composer must not offer a blind retry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SendError {
    pub message: String,
    pub maybe_sent: bool,
}

/// Worker to UI. `Debug` is hand-written and redacting.
#[derive(Clone)]
pub enum SyncEvent {
    Auth(AuthState),
    /// The saved client id (never the secret).
    Client(Option<String>),
    /// Signed-in accounts, in display order.
    Accounts(Vec<AccountId>),
    /// A browser sign-in is waiting (`Some(url)`) or not.
    SigningIn(Option<String>),
    #[allow(dead_code)] // consumed in M10
    Busy(bool),
    Synced(Timestamp),
    /// A persistent problem for the banner; `None` clears it.
    Problem(Option<String>),
    /// A one-off message for the toast.
    Notice(String),
    /// The full list: replaces the account's labels.
    Labels {
        account: AccountId,
        labels: Vec<Label>,
    },
    /// A count refresh: upserts by label id.
    LabelCounts {
        account: AccountId,
        labels: Vec<Label>,
    },
    /// One page of a view. `ids` is the page in list order; `threads` holds the summaries the
    /// worker already has, and ids without one show as skeleton rows until a
    /// `ThreadsUpdated` fills them in.
    ThreadPage {
        view: ViewKey,
        ids: Vec<String>,
        threads: Vec<ThreadSummary>,
        mode: PageMode,
        has_more: bool,
    },
    /// Fresh summaries for threads anywhere (history sync, post-mutation). Upsert only.
    ThreadsUpdated(Vec<ThreadSummary>),
    #[allow(dead_code)] // consumed in M11
    ThreadsGone {
        account: AccountId,
        ids: Vec<String>,
    },
    ThreadDetail(ThreadDetail),
    ThreadFailed {
        key: ThreadKey,
        error: String,
    },
    MutationFinished {
        op: OpId,
        error: Option<String>,
    },
    SendFinished {
        op: OpId,
        error: Option<SendError>,
    },
    /// New unread INBOX threads found by an incremental sync (never on the first sync).
    NewMail {
        #[allow(dead_code)] // consumed in M13
        account: AccountId,
        threads: Vec<ThreadSummary>,
    },
    /// History expired, so the account's caches are dropped.
    #[allow(dead_code)] // consumed in M11
    AccountReset(AccountId),
}

impl fmt::Debug for SyncEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SyncEvent::Auth(a) => write!(f, "Auth({a:?})"),
            SyncEvent::Client(c) => write!(f, "Client(set: {})", c.is_some()),
            SyncEvent::Accounts(a) => write!(f, "Accounts({})", a.len()),
            SyncEvent::SigningIn(u) => write!(f, "SigningIn({})", u.is_some()),
            SyncEvent::Busy(b) => write!(f, "Busy({b})"),
            SyncEvent::Synced(_) => f.write_str("Synced"),
            SyncEvent::Problem(p) => write!(f, "Problem({})", p.is_some()),
            SyncEvent::Notice(_) => f.write_str("Notice"),
            SyncEvent::Labels { labels, .. } => write!(f, "Labels({})", labels.len()),
            SyncEvent::LabelCounts { labels, .. } => write!(f, "LabelCounts({})", labels.len()),
            SyncEvent::ThreadPage {
                ids,
                mode,
                has_more,
                ..
            } => write!(
                f,
                "ThreadPage({mode:?}, {} ids, has_more: {has_more})",
                ids.len()
            ),
            SyncEvent::ThreadsUpdated(t) => write!(f, "ThreadsUpdated({})", t.len()),
            SyncEvent::ThreadsGone { ids, .. } => write!(f, "ThreadsGone({})", ids.len()),
            SyncEvent::ThreadDetail(d) => write!(f, "ThreadDetail({} messages)", d.messages.len()),
            SyncEvent::ThreadFailed { .. } => f.write_str("ThreadFailed"),
            SyncEvent::MutationFinished { op, error } => {
                write!(f, "MutationFinished({op}, failed: {})", error.is_some())
            }
            SyncEvent::SendFinished { op, error } => {
                write!(f, "SendFinished({op}, failed: {})", error.is_some())
            }
            SyncEvent::NewMail { threads, .. } => write!(f, "NewMail({})", threads.len()),
            SyncEvent::AccountReset(_) => f.write_str("AccountReset"),
        }
    }
}

/// The UI's end of the command channel. Sending never blocks; a stopped worker is ignored.
#[derive(Clone)]
pub struct SyncHandle {
    tx: mpsc::Sender<Command>,
}

impl SyncHandle {
    pub fn send(&self, cmd: Command) {
        let _ = self.tx.send(cmd);
    }

    pub fn from_sender(tx: mpsc::Sender<Command>) -> Self {
        Self { tx }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::model::Address;

    #[test]
    fn sync_event_debug_never_prints_mail() {
        let summary = ThreadSummary {
            key: ThreadKey::new("me@example.com", "t1"),
            history_id: 1,
            subject: "Secret subject".into(),
            snippet: "secret snippet".into(),
            participants: vec![(Address::new(Some("Secret"), "secret@example.org"), true)],
            last_date: Timestamp::UNIX_EPOCH,
            message_count: 1,
            label_ids: BTreeSet::new(),
            has_attachments: false,
        };
        let events = [
            SyncEvent::ThreadsUpdated(vec![summary.clone()]),
            SyncEvent::NewMail {
                account: "me@example.com".into(),
                threads: vec![summary.clone()],
            },
            SyncEvent::Notice("secret notice".into()),
            SyncEvent::Problem(Some("secret problem".into())),
            SyncEvent::ThreadFailed {
                key: summary.key.clone(),
                error: "secret".into(),
            },
        ];
        for ev in events {
            let s = format!("{ev:?}");
            assert!(!s.to_lowercase().contains("secret"), "leaked: {s}");
        }
    }
}
