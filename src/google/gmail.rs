//! Gmail API DTOs (serde) and the pure query builders for its list endpoints (SPEC 6.2).
//!
//! [`profile`] is the first endpoint function (M9); the rest arrive with the sync worker
//! (M10, M11). Every endpoint takes the [`Endpoints`] so tests can redirect it. Ids,
//! `historyId` and `internalDate` are JSON strings; `mail::parse` turns them into numbers and
//! `model` types. Every array is `#[serde(default)]` because Gmail omits empty ones.
//!
//! The DTOs that carry mail content (snippets, headers, bodies, attachment data) have
//! hand-written `Debug` impls that print ids, counts and sizes only, so an accidental `{:?}`
//! in a log never leaks mail (the same rule as the `model` types, SPEC 1).

use std::fmt;

use serde::{Deserialize, Serialize};

use super::api::{self, ApiError, Endpoints, Limits};
use crate::model::ViewKind;

/// `GET /profile`.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Profile {
    pub email_address: String,
    #[allow(dead_code)] // consumed in M10
    #[serde(default)]
    pub history_id: String,
    #[allow(dead_code)] // consumed in M10
    #[serde(default)]
    pub messages_total: Option<u64>,
    #[allow(dead_code)] // consumed in M10
    #[serde(default)]
    pub threads_total: Option<u64>,
}

/// `GET /labels`.
#[allow(dead_code)] // consumed in M10
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LabelList {
    #[serde(default)]
    pub labels: Vec<LabelDto>,
}

/// One label, from `GET /labels` (no counts) or `GET /labels/{id}` (with counts).
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LabelDto {
    pub id: String,
    pub name: String,
    /// "system" or "user".
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub label_list_visibility: Option<String>,
    #[serde(default)]
    pub messages_total: Option<u32>,
    #[serde(default)]
    pub threads_total: Option<u32>,
    #[serde(default)]
    pub threads_unread: Option<u32>,
    #[serde(default)]
    pub color: Option<LabelColor>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LabelColor {
    #[serde(default)]
    pub text_color: Option<String>,
    #[serde(default)]
    pub background_color: Option<String>,
}

/// `GET /threads`.
#[allow(dead_code)] // consumed in M10
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadList {
    #[serde(default)]
    pub threads: Vec<ThreadRef>,
    #[serde(default)]
    pub next_page_token: Option<String>,
    #[serde(default)]
    pub result_size_estimate: Option<u64>,
}

#[allow(dead_code)] // consumed in M10
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadRef {
    pub id: String,
    #[serde(default)]
    pub snippet: String,
    #[serde(default)]
    pub history_id: String,
}

/// `GET /threads/{id}` in `metadata` or `full` format.
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadDto {
    pub id: String,
    #[serde(default)]
    pub history_id: String,
    #[serde(default)]
    pub messages: Vec<MessageDto>,
}

/// A message in a thread, or `GET /messages/{id}`.
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageDto {
    pub id: String,
    #[serde(default)]
    pub thread_id: String,
    #[serde(default)]
    pub label_ids: Vec<String>,
    #[serde(default)]
    pub snippet: String,
    #[serde(default)]
    pub history_id: String,
    /// Milliseconds since the epoch, as a string.
    #[serde(default)]
    pub internal_date: Option<String>,
    #[serde(default)]
    pub payload: Option<PartDto>,
}

/// A MIME part. The message payload is the root part.
#[derive(Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PartDto {
    #[serde(default)]
    pub part_id: String,
    #[serde(default)]
    pub mime_type: String,
    #[serde(default)]
    pub filename: String,
    #[serde(default)]
    pub headers: Vec<Header>,
    #[serde(default)]
    pub body: Option<BodyDto>,
    #[serde(default)]
    pub parts: Vec<PartDto>,
}

#[derive(Clone, Deserialize)]
pub struct Header {
    pub name: String,
    pub value: String,
}

#[derive(Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BodyDto {
    #[serde(default)]
    pub attachment_id: Option<String>,
    #[serde(default)]
    pub size: u64,
    /// URL-safe base64.
    #[serde(default)]
    pub data: Option<String>,
}

/// `GET /history`.
#[allow(dead_code)] // consumed in M11
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryList {
    #[serde(default)]
    pub history: Vec<HistoryRecord>,
    #[serde(default)]
    pub next_page_token: Option<String>,
    #[serde(default)]
    pub history_id: String,
}

#[allow(dead_code)] // consumed in M11
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryRecord {
    pub id: String,
    #[serde(default)]
    pub messages: Vec<MessageRef>,
    #[serde(default)]
    pub messages_added: Vec<HistoryMessage>,
    #[serde(default)]
    pub messages_deleted: Vec<HistoryMessage>,
    #[serde(default)]
    pub labels_added: Vec<HistoryLabels>,
    #[serde(default)]
    pub labels_removed: Vec<HistoryLabels>,
}

#[allow(dead_code)] // consumed in M11
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageRef {
    pub id: String,
    #[serde(default)]
    pub thread_id: String,
    #[serde(default)]
    pub label_ids: Vec<String>,
}

#[allow(dead_code)] // consumed in M11
#[derive(Clone, Debug, Deserialize)]
pub struct HistoryMessage {
    pub message: MessageRef,
}

#[allow(dead_code)] // consumed in M11
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryLabels {
    pub message: MessageRef,
    #[serde(default)]
    pub label_ids: Vec<String>,
}

/// `GET /messages/{m}/attachments/{a}`.
#[allow(dead_code)] // consumed in M12
#[derive(Clone, Deserialize)]
pub struct AttachmentBody {
    #[serde(default)]
    pub size: u64,
    /// URL-safe base64.
    #[serde(default)]
    pub data: String,
}

/// Body of `threads/{id}/modify` and `messages/{id}/modify`.
#[allow(dead_code)] // consumed in M11
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModifyRequest {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub add_label_ids: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub remove_label_ids: Vec<String>,
}

/// JSON part of the multipart `messages/send` upload.
#[allow(dead_code)] // consumed in M11
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SendMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
}

/// Response of `messages/send` (a `Message` resource with only ids and labels).
#[allow(dead_code)] // consumed in M11
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendResponse {
    pub id: String,
    #[serde(default)]
    pub thread_id: String,
    #[serde(default)]
    pub label_ids: Vec<String>,
}

impl fmt::Debug for ThreadRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ThreadRef")
            .field("id", &self.id)
            .field("history_id", &self.history_id)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for ThreadDto {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ThreadDto")
            .field("id", &self.id)
            .field("history_id", &self.history_id)
            .field("messages", &self.messages)
            .finish()
    }
}

impl fmt::Debug for MessageDto {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MessageDto")
            .field("id", &self.id)
            .field("thread_id", &self.thread_id)
            .field("label_ids", &self.label_ids)
            .field("history_id", &self.history_id)
            .field("payload", &self.payload)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for PartDto {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PartDto")
            .field("part_id", &self.part_id)
            .field("mime_type", &self.mime_type)
            .field("headers", &self.headers.len())
            .field("body", &self.body)
            .field("parts", &self.parts)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for Header {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Header")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for BodyDto {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BodyDto")
            .field("has_attachment_id", &self.attachment_id.is_some())
            .field("size", &self.size)
            .field("data_len", &self.data.as_ref().map(String::len))
            .finish()
    }
}

impl fmt::Debug for AttachmentBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AttachmentBody")
            .field("size", &self.size)
            .field("data_len", &self.data.len())
            .finish()
    }
}

/// Query parameters for `GET /threads`. `Label(id)` sets `labelIds` (plus
/// `includeSpamTrash=true` for TRASH and SPAM), `AllMail` sets neither, `Search(q)` sets `q`.
/// `GET /profile`: the account's address and current history id (1 unit).
pub fn profile(agent: &ureq::Agent, ep: &Endpoints, token: &str) -> Result<Profile, ApiError> {
    api::get(
        agent,
        token,
        &format!("{}/profile", ep.base),
        &[],
        Limits::default(),
    )
}

#[allow(dead_code)] // consumed in M10
pub fn threads_query(
    view: &ViewKind,
    page_token: Option<&str>,
    max: u32,
) -> Vec<(&'static str, String)> {
    let mut q = Vec::new();
    match view {
        ViewKind::Label(id) => {
            q.push(("labelIds", id.clone()));
            if id == "TRASH" || id == "SPAM" {
                q.push(("includeSpamTrash", "true".to_string()));
            }
        }
        ViewKind::AllMail => {}
        ViewKind::Search(text) => q.push(("q", text.clone())),
    }
    q.push(("maxResults", max.to_string()));
    if let Some(t) = page_token {
        q.push(("pageToken", t.to_string()));
    }
    q
}

/// Query parameters for `GET /threads/{id}?format=metadata`.
#[allow(dead_code)] // consumed in M10
pub fn thread_meta_query() -> Vec<(&'static str, String)> {
    let mut q = vec![("format", "metadata".to_string())];
    for h in ["From", "To", "Cc", "Subject", "Date"] {
        q.push(("metadataHeaders", h.to_string()));
    }
    q
}

/// Query parameters for `GET /history`, with the repeated `historyTypes`.
#[allow(dead_code)] // consumed in M11
pub fn history_query(start: u64, page_token: Option<&str>) -> Vec<(&'static str, String)> {
    let mut q = vec![("startHistoryId", start.to_string())];
    for t in [
        "messageAdded",
        "messageDeleted",
        "labelAdded",
        "labelRemoved",
    ] {
        q.push(("historyTypes", t.to_string()));
    }
    q.push(("maxResults", "500".to_string()));
    if let Some(t) = page_token {
        q.push(("pageToken", t.to_string()));
    }
    q
}

#[cfg(test)]
pub(crate) mod fixtures {
    pub const PROFILE: &str = include_str!("fixtures/profile.json");
    pub const LABELS: &str = include_str!("fixtures/labels.json");
    pub const LABEL_INBOX: &str = include_str!("fixtures/label_inbox.json");
    pub const THREADS: &str = include_str!("fixtures/threads.json");
    pub const THREADS_EMPTY: &str = include_str!("fixtures/threads_empty.json");
    pub const THREAD_META: &str = include_str!("fixtures/thread_meta.json");
    pub const THREAD_FULL: &str = include_str!("fixtures/thread_full.json");
    pub const HISTORY: &str = include_str!("fixtures/history.json");
    pub const HISTORY_EMPTY: &str = include_str!("fixtures/history_empty.json");
    pub const ATTACHMENT: &str = include_str!("fixtures/attachment.json");
    pub const SEND: &str = include_str!("fixtures/send.json");
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    #[test]
    fn content_dtos_debug_without_mail_content() {
        let t: ThreadDto = serde_json::from_str(THREAD_FULL).unwrap();
        let printed = format!("{t:?}");
        assert!(printed.contains(&t.id));
        let m = &t.messages[0];
        assert!(!printed.contains(&m.snippet));
        let payload = m.payload.as_ref().unwrap();
        for h in &payload.headers {
            assert!(!printed.contains(&h.value), "{} leaked", h.name);
        }
        let a: AttachmentBody = serde_json::from_str(ATTACHMENT).unwrap();
        assert!(!format!("{a:?}").contains(&a.data));
    }

    #[test]
    fn profile_deserialises() {
        let p: Profile = serde_json::from_str(PROFILE).unwrap();
        assert_eq!(p.email_address, "you@example.com");
        assert_eq!(p.history_id, "123456");
    }

    #[test]
    fn labels_deserialise() {
        let l: LabelList = serde_json::from_str(LABELS).unwrap();
        assert!(l.labels.len() >= 8);
        let user = l.labels.iter().find(|l| l.id == "Label_1").unwrap();
        assert_eq!(user.kind.as_deref(), Some("user"));
        let color = user.color.as_ref().unwrap();
        assert_eq!(color.background_color.as_deref(), Some("#16a766"));
        let hidden = l.labels.iter().find(|l| l.id == "Label_3").unwrap();
        assert_eq!(hidden.label_list_visibility.as_deref(), Some("labelHide"));
        assert!(l.labels.iter().all(|l| l.threads_total.is_none()));
        let empty: LabelList = serde_json::from_str("{}").unwrap();
        assert!(empty.labels.is_empty());
    }

    #[test]
    fn label_counts_deserialise() {
        let l: LabelDto = serde_json::from_str(LABEL_INBOX).unwrap();
        assert_eq!(l.id, "INBOX");
        assert_eq!(l.threads_unread, Some(3));
        assert_eq!(l.threads_total, Some(42));
        assert_eq!(l.messages_total, Some(57));
    }

    #[test]
    fn thread_lists_deserialise_including_empty() {
        let t: ThreadList = serde_json::from_str(THREADS).unwrap();
        assert_eq!(t.threads.len(), 2);
        assert_eq!(t.threads[0].id, "18c0a1b2c3d4e5f6");
        assert_eq!(t.next_page_token.as_deref(), Some("09876543210"));
        let e: ThreadList = serde_json::from_str(THREADS_EMPTY).unwrap();
        assert!(e.threads.is_empty());
        assert!(e.next_page_token.is_none());
    }

    #[test]
    fn threads_deserialise() {
        let m: ThreadDto = serde_json::from_str(THREAD_META).unwrap();
        assert_eq!(m.messages.len(), 2);
        let p = m.messages[0].payload.as_ref().unwrap();
        assert!(p.headers.iter().any(|h| h.name == "Subject"));
        assert!(p.body.as_ref().unwrap().data.is_none());
        let f: ThreadDto = serde_json::from_str(THREAD_FULL).unwrap();
        assert_eq!(f.messages.len(), 2);
        assert!(!f.messages[1].payload.as_ref().unwrap().parts.is_empty());
    }

    #[test]
    fn history_deserialises_including_empty() {
        let h: HistoryList = serde_json::from_str(HISTORY).unwrap();
        assert_eq!(h.history_id, "123500");
        assert_eq!(h.history.len(), 3);
        assert_eq!(h.history[0].messages_added.len(), 1);
        assert_eq!(h.history[1].labels_removed[0].label_ids, vec!["UNREAD"]);
        assert_eq!(h.history[2].messages_deleted[0].message.thread_id, "t-old");
        let e: HistoryList = serde_json::from_str(HISTORY_EMPTY).unwrap();
        assert!(e.history.is_empty());
        assert_eq!(e.history_id, "123456");
    }

    #[test]
    fn attachment_and_send_deserialise() {
        let a: AttachmentBody = serde_json::from_str(ATTACHMENT).unwrap();
        assert_eq!(a.size, 11);
        assert_eq!(crate::mail::b64::decode(&a.data).unwrap(), b"hello world");
        let s: SendResponse = serde_json::from_str(SEND).unwrap();
        assert_eq!(s.thread_id, "18c0a1b2c3d4e5f6");
        assert_eq!(s.label_ids, vec!["SENT"]);
    }

    #[test]
    fn requests_serialise() {
        let m = ModifyRequest {
            add_label_ids: vec![],
            remove_label_ids: vec!["INBOX".into()],
        };
        assert_eq!(
            serde_json::to_string(&m).unwrap(),
            r#"{"removeLabelIds":["INBOX"]}"#
        );
        assert_eq!(
            serde_json::to_string(&SendMetadata::default()).unwrap(),
            "{}"
        );
        let s = SendMetadata {
            thread_id: Some("t1".into()),
        };
        assert_eq!(serde_json::to_string(&s).unwrap(), r#"{"threadId":"t1"}"#);
    }

    fn has(q: &[(&str, String)], k: &str, v: &str) -> bool {
        q.iter().any(|(a, b)| *a == k && b == v)
    }

    #[test]
    fn threads_query_per_view() {
        let inbox = threads_query(&ViewKind::Label("INBOX".into()), None, 50);
        assert!(has(&inbox, "labelIds", "INBOX"));
        assert!(has(&inbox, "maxResults", "50"));
        assert!(
            !inbox
                .iter()
                .any(|(k, _)| *k == "includeSpamTrash" || *k == "pageToken")
        );
        for id in ["TRASH", "SPAM"] {
            let q = threads_query(&ViewKind::Label(id.into()), Some("tok"), 20);
            assert!(has(&q, "labelIds", id));
            assert!(has(&q, "includeSpamTrash", "true"));
            assert!(has(&q, "pageToken", "tok"));
        }
        let all = threads_query(&ViewKind::AllMail, None, 50);
        assert!(!all.iter().any(|(k, _)| *k == "labelIds" || *k == "q"));
        let search = threads_query(&ViewKind::Search("from:bob".into()), None, 50);
        assert!(has(&search, "q", "from:bob"));
        assert!(!search.iter().any(|(k, _)| *k == "labelIds"));
    }

    #[test]
    fn history_query_repeats_history_types() {
        let q = history_query(123, Some("p2"));
        assert!(has(&q, "startHistoryId", "123"));
        let types: Vec<&str> = q
            .iter()
            .filter(|(k, _)| *k == "historyTypes")
            .map(|(_, v)| v.as_str())
            .collect();
        assert_eq!(
            types,
            [
                "messageAdded",
                "messageDeleted",
                "labelAdded",
                "labelRemoved"
            ]
        );
        assert!(has(&q, "maxResults", "500"));
        assert!(has(&q, "pageToken", "p2"));
        let meta = thread_meta_query();
        assert!(has(&meta, "format", "metadata"));
        assert_eq!(
            meta.iter().filter(|(k, _)| *k == "metadataHeaders").count(),
            5
        );
    }
}
