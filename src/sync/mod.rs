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

use crate::cache::{CachedThread, MailCache, Snapshot};
use crate::config::Prefs;
use crate::dismissals::DismissalStore;
use crate::google::api::{self, ApiError, Endpoints};
use crate::google::gmail;
use crate::google::oauth::{self, AuthError, ClientCredentials, TokenResponse};
use crate::mail::{b64, compose, files, parse, signature};
use crate::model::{
    AccountId, Address, Attachment, AttachmentSource, ComposeKind, Label, LabelDelta, LabelKind,
    MAX_OUTGOING_ATTACHMENTS, Outgoing, OutgoingAttachment, SendAs, ThreadKey, ThreadSummary,
    ViewKey, ViewKind, labels,
};
use crate::nudges::Dismissed;
use crate::protocol::{AuthState, Command, OpId, PageMode, SendError, SyncEvent, SyncHandle};
use crate::secrets::{SecretError, Secrets};
use crate::thread_cache::{self, ThreadCache};

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
        MailCache::default_dir(),
    )
}

/// Starts the worker against `endpoints` with `builtin` as the fallback client, saving
/// attachments to `downloads` (the user's download folder when `None`) and the summary cache
/// to `cache_dir` (no cache when `None`).
fn start_with(
    secrets: Secrets,
    endpoints: Endpoints,
    builtin: Option<ClientCredentials>,
    tuning: Tuning,
    downloads: Option<PathBuf>,
    cache_dir: Option<PathBuf>,
) -> std::io::Result<(SyncHandle, async_channel::Receiver<SyncEvent>)> {
    let (tx, rx) = mpsc::channel();
    let (etx, erx) = async_channel::unbounded();
    let self_tx = tx.clone();
    std::thread::Builder::new()
        .name("eloquawk-sync".into())
        .spawn(move || {
            let mut worker = Worker::new(secrets, etx, self_tx, endpoints, builtin, tuning);
            worker.downloads = downloads;
            worker.cache_dir = cache_dir;
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

mod account;
mod attachments;
mod auth;
mod cache;
mod changes;
mod errors;
mod outcomes;
mod queue;
mod read_jobs;
mod tuning;
mod worker;
mod write_jobs;

use self::account::*;
use self::attachments::*;
use self::changes::*;
use self::errors::*;
use self::queue::*;
use self::tuning::*;
use self::worker::*;

#[cfg(test)]
mod tests;
