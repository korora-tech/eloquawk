//! Domain types shared by the worker, the store and the UI (SPEC 4.1).
//!
//! These are plain data. Conversion from Gmail's JSON lives in `mail::parse`. `Message`,
//! `ThreadDetail` and `Outgoing` carry mail content, so their `Debug` impls are hand-written
//! and print ids and counts only: an accidental `{:?}` can never leak a subject or a body.

use std::collections::BTreeSet;
use std::fmt;
use std::path::PathBuf;

use jiff::Timestamp;

/// The account's email address. Never contains '/'.
pub type AccountId = String;

/// Gmail's system label ids used by the client.
pub mod labels {
    pub const INBOX: &str = "INBOX";
    pub const UNREAD: &str = "UNREAD";
    pub const STARRED: &str = "STARRED";
    pub const IMPORTANT: &str = "IMPORTANT";
    pub const SENT: &str = "SENT";
    pub const DRAFT: &str = "DRAFT";
    pub const SPAM: &str = "SPAM";
    pub const TRASH: &str = "TRASH";
}

/// A thread, identified by its account and Gmail thread id.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ThreadKey {
    pub account: AccountId,
    pub id: String,
}

impl ThreadKey {
    pub fn new(account: impl Into<AccountId>, id: impl Into<String>) -> Self {
        Self {
            account: account.into(),
            id: id.into(),
        }
    }
}

impl fmt::Display for ThreadKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.account, self.id)
    }
}

/// What a thread list shows.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ViewKind {
    Label(String),
    AllMail,
    Search(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ViewKey {
    pub account: AccountId,
    pub kind: ViewKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LabelKind {
    System,
    User,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Label {
    pub account: AccountId,
    pub id: String,
    pub name: String,
    pub kind: LabelKind,
    /// (text, background) parsed from "#rrggbb".
    pub color: Option<(u32, u32)>,
    pub threads_unread: u32,
    pub threads_total: u32,
    pub messages_total: u32,
    /// `labelListVisibility == "labelHide"`.
    pub hidden: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Address {
    pub name: Option<String>,
    pub email: String,
}

impl Address {
    pub fn new(name: Option<&str>, email: &str) -> Self {
        Self {
            name: name.map(str::to_string),
            email: email.to_string(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ThreadSummary {
    pub key: ThreadKey,
    pub history_id: u64,
    pub subject: String,
    /// Entity-decoded.
    pub snippet: String,
    /// In message order, deduped by email; the flag is true when any of their messages is
    /// unread.
    pub participants: Vec<(Address, bool)>,
    /// Latest `internalDate`.
    pub last_date: Timestamp,
    pub message_count: u32,
    pub label_ids: BTreeSet<String>,
    pub has_attachments: bool,
}

impl ThreadSummary {
    pub fn unread(&self) -> bool {
        self.in_label(labels::UNREAD)
    }

    pub fn starred(&self) -> bool {
        self.in_label(labels::STARRED)
    }

    pub fn in_label(&self, id: &str) -> bool {
        self.label_ids.contains(id)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Attachment {
    pub part_id: String,
    pub attachment_id: Option<String>,
    pub filename: String,
    pub mime_type: String,
    pub size: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BodySource {
    Plain,
    Html,
    Empty,
}

#[derive(Clone, PartialEq)]
pub struct Message {
    pub id: String,
    pub thread_id: String,
    pub label_ids: BTreeSet<String>,
    pub from: Address,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub bcc: Vec<Address>,
    pub reply_to: Vec<Address>,
    pub date: Timestamp,
    pub subject: String,
    pub message_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub snippet: String,
    pub body: String,
    pub body_source: BodySource,
    /// The HTML input cap was hit or the conversion failed (SPEC 8.2).
    pub body_truncated: bool,
    pub attachments: Vec<Attachment>,
}

impl fmt::Debug for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Message")
            .field("id", &self.id)
            .field("thread_id", &self.thread_id)
            .field("labels", &self.label_ids.len())
            .field("body_len", &self.body.len())
            .field("attachments", &self.attachments.len())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, PartialEq)]
pub struct ThreadDetail {
    pub key: ThreadKey,
    pub subject: String,
    pub label_ids: BTreeSet<String>,
    pub messages: Vec<Message>,
}

impl fmt::Debug for ThreadDetail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ThreadDetail")
            .field("key", &self.key)
            .field("labels", &self.label_ids.len())
            .field("messages", &self.messages.len())
            .finish_non_exhaustive()
    }
}

/// Labels to add and remove in one mutation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LabelDelta {
    pub add: Vec<String>,
    pub remove: Vec<String>,
}

impl LabelDelta {
    fn of(add: &[&str], remove: &[&str]) -> Self {
        Self {
            add: add.iter().map(|s| s.to_string()).collect(),
            remove: remove.iter().map(|s| s.to_string()).collect(),
        }
    }

    pub fn archive() -> Self {
        Self::of(&[], &[labels::INBOX])
    }

    pub fn read() -> Self {
        Self::of(&[], &[labels::UNREAD])
    }

    pub fn unread() -> Self {
        Self::of(&[labels::UNREAD], &[])
    }

    pub fn star() -> Self {
        Self::of(&[labels::STARRED], &[])
    }

    pub fn unstar() -> Self {
        Self::of(&[], &[labels::STARRED])
    }

    pub fn spam() -> Self {
        Self::of(&[labels::SPAM], &[labels::INBOX])
    }

    pub fn not_spam() -> Self {
        Self::of(&[labels::INBOX], &[labels::SPAM])
    }

    /// The delta that undoes this one.
    pub fn inverse(&self) -> Self {
        Self {
            add: self.remove.clone(),
            remove: self.add.clone(),
        }
    }

    #[allow(dead_code)] // consumed in M11
    pub fn is_empty(&self) -> bool {
        self.add.is_empty() && self.remove.is_empty()
    }

    /// Applies the delta to a label set: removals first, then additions.
    pub fn apply_to(&self, set: &mut BTreeSet<String>) {
        for id in &self.remove {
            set.remove(id);
        }
        for id in &self.add {
            set.insert(id.clone());
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComposeKind {
    New,
    Reply,
    ReplyAll,
    Forward,
}

#[derive(Clone)]
pub struct Outgoing {
    pub account: AccountId,
    pub kind: ComposeKind,
    pub from: Address,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub bcc: Vec<Address>,
    pub subject: String,
    pub body: String,
    pub thread_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub attachments: Vec<OutgoingAttachment>,
}

impl fmt::Debug for Outgoing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Outgoing")
            .field("kind", &self.kind)
            .field("thread_id", &self.thread_id)
            .field(
                "recipients",
                &(self.to.len() + self.cc.len() + self.bcc.len()),
            )
            .field("body_len", &self.body.len())
            .field("attachments", &self.attachments.len())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug)]
pub enum AttachmentSource {
    Local(PathBuf),
    Gmail { message_id: String, part_id: String },
}

#[derive(Clone, Debug)]
pub struct OutgoingAttachment {
    pub filename: String,
    pub mime_type: String,
    pub size: u64,
    pub source: AttachmentSource,
}

/// Summed `size` limit of an outgoing message's attachments (SPEC 8.5).
pub const MAX_OUTGOING_ATTACHMENTS: u64 = 25 * 1024 * 1024;

#[cfg(test)]
mod tests {
    use super::*;

    fn message() -> Message {
        Message {
            id: "m1".into(),
            thread_id: "t1".into(),
            label_ids: BTreeSet::new(),
            from: Address::new(Some("Secret Sender"), "secret@example.com"),
            to: vec![],
            cc: vec![],
            bcc: vec![],
            reply_to: vec![],
            date: Timestamp::UNIX_EPOCH,
            subject: "Secret subject".into(),
            message_id: None,
            in_reply_to: None,
            references: vec![],
            snippet: "secret snippet".into(),
            body: "secret body".into(),
            body_source: BodySource::Plain,
            body_truncated: false,
            attachments: vec![],
        }
    }

    #[test]
    fn thread_key_displays_account_slash_id() {
        assert_eq!(
            ThreadKey::new("me@example.com", "abc").to_string(),
            "me@example.com/abc"
        );
    }

    #[test]
    fn debug_impls_redact_mail_content() {
        let m = message();
        let detail = ThreadDetail {
            key: ThreadKey::new("me@example.com", "t1"),
            subject: "Secret subject".into(),
            label_ids: BTreeSet::new(),
            messages: vec![m.clone()],
        };
        let out = Outgoing {
            account: "me@example.com".into(),
            kind: ComposeKind::New,
            from: Address::new(None, "me@example.com"),
            to: vec![Address::new(Some("Secret"), "secret@example.com")],
            cc: vec![],
            bcc: vec![],
            subject: "Secret subject".into(),
            body: "secret body".into(),
            thread_id: None,
            in_reply_to: None,
            references: vec![],
            attachments: vec![],
        };
        for s in [format!("{m:?}"), format!("{detail:?}"), format!("{out:?}")] {
            assert!(!s.to_lowercase().contains("secret"), "leaked: {s}");
        }
    }

    #[test]
    fn label_delta_helpers_and_inverse() {
        let mut set: BTreeSet<String> = ["INBOX", "UNREAD"].map(String::from).into();
        LabelDelta::archive().apply_to(&mut set);
        assert!(!set.contains("INBOX"));
        LabelDelta::archive().inverse().apply_to(&mut set);
        assert!(set.contains("INBOX"));
        assert_eq!(LabelDelta::spam().inverse(), LabelDelta::not_spam());
        assert_eq!(LabelDelta::star().inverse(), LabelDelta::unstar());
        assert_eq!(LabelDelta::read().inverse(), LabelDelta::unread());
        assert!(LabelDelta::default().is_empty());
    }

    #[test]
    fn summary_flags_follow_labels() {
        let s = ThreadSummary {
            key: ThreadKey::new("a@b.c", "t"),
            history_id: 1,
            subject: String::new(),
            snippet: String::new(),
            participants: vec![],
            last_date: Timestamp::UNIX_EPOCH,
            message_count: 1,
            label_ids: ["UNREAD", "STARRED"].map(String::from).into(),
            has_attachments: false,
        };
        assert!(s.unread() && s.starred() && !s.in_label("INBOX"));
    }
}
