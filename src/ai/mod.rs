//! AI triage (SPEC 16): Jev, TypeSafe's decision model, reached through OpenRouter's Decisions
//! API, flags phishing, scams and spam. Advisory only: nothing here moves or labels mail.
//!
//! The UI builds a small, redacted [`prompt::AiInput`] and sends it as a [`AiCommand::Check`]
//! to the `eloquawk-ai` thread ([`worker`]), which owns the OpenRouter key, the HTTP calls,
//! retries, pauses and the spend limit, and answers with [`AiEvent`]s. Demo mode runs the same
//! worker with an offline checker ([`fake`]) and never touches the network or the keyring.

pub mod cache;
pub mod fake;
pub mod jev;
pub mod openrouter;
pub mod prompt;
pub mod worker;

use std::fmt;
use std::sync::mpsc;

use crate::config::Prefs;
use crate::model::{AccountId, ThreadKey};
use crate::secrets::Secrets;
use crate::store::Mode;

use jev::Verdict;
use openrouter::{AiEndpoints, ApiKey};
use prompt::AiInput;

/// The pinned model; its thresholds are tuned for this version (SPEC 16.1).
pub const JEV_MODEL: &str = "typesafe/jev-1.13";
/// The disclosure text's version. Any change to what is sent bumps it, which turns every
/// account off until the user accepts again (SPEC 16.2).
pub const DISCLOSURE_VERSION: u32 = 1;

/// Which lane a check runs in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Priority {
    /// The user opened the message: a single slot, the newest wins.
    Interactive,
    /// New mail checked as it arrives: a FIFO capped at 50.
    Background,
}

/// One label offered to Jev (M17).
#[derive(Clone, PartialEq)]
pub struct LabelCandidate {
    pub id: String,
    pub name: String,
}

/// The label questions of a check (M17).
#[derive(Clone, PartialEq)]
pub struct LabelAsk {
    pub fingerprint: u64,
    pub candidates: Vec<LabelCandidate>,
}

/// One message to check. Not `Debug`: its input is mail.
pub struct CheckRequest {
    pub account: AccountId,
    pub thread: ThreadKey,
    pub message_id: String,
    pub priority: Priority,
    /// Ask the triage questions.
    pub verdict: bool,
    /// Ask label questions (M17; always `None` before it).
    pub labels: Option<LabelAsk>,
    pub input: AiInput,
}

/// UI to worker. Not `Debug`: it carries the key and mail.
#[allow(dead_code)] // SaveKey, ClearKey, TestKey and ResumePaused: consumed in M16 by Settings
pub enum AiCommand {
    SaveKey(ApiKey),
    ClearKey,
    /// Loads (or reloads) the key; the first one waits for the sync worker's keyring access.
    Reload,
    /// Tests a typed key, or the saved one with `None`.
    TestKey(Option<ApiKey>),
    Check(Box<CheckRequest>),
    /// Opt-out, account removal or reset: drop everything queued for the account.
    ForgetAccount(AccountId),
    /// Settings' "Try again" after a pause.
    ResumePaused,
    /// The session spend limit changed, in cents.
    SpendLimit(u32),
    Shutdown,
}

/// What the worker knows about the key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyState {
    /// The worker has not read the keyring yet.
    Unknown,
    Missing,
    Saved {
        hint: String,
    },
    Locked,
    Rejected {
        hint: String,
    },
    Demo,
}

/// Label answers (M17): the fingerprint they were asked under (`None` when partial) and the
/// probabilities by label id.
pub type LabelAnswers = (Option<u64>, Vec<(String, f32)>);

/// The answers of one check.
#[derive(Clone, PartialEq)]
pub struct Outcome {
    pub verdict: Option<Verdict>,
    pub labels: Option<LabelAnswers>,
}

/// Worker to UI. `Debug` prints variant names only.
#[derive(Clone)]
pub enum AiEvent {
    Key(KeyState),
    /// A check was accepted; exactly one `Checked`, `Failed` or `Dropped` follows.
    Checking {
        thread: ThreadKey,
        message_id: String,
    },
    Checked {
        thread: ThreadKey,
        message_id: String,
        outcome: Outcome,
    },
    Failed {
        thread: ThreadKey,
        message_id: String,
        error: String,
        retryable: bool,
    },
    /// Discarded without a result.
    Dropped {
        thread: ThreadKey,
        message_id: String,
    },
    /// `Some(reason)`: no requests until resumed.
    Paused(Option<String>),
    Usage {
        checks: u32,
        cost_usd: f64,
        cost_unknown: u32,
    },
    TestResult(Result<String, String>),
}

impl fmt::Debug for AiEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            AiEvent::Key(_) => "Key",
            AiEvent::Checking { .. } => "Checking",
            AiEvent::Checked { .. } => "Checked",
            AiEvent::Failed { .. } => "Failed",
            AiEvent::Dropped { .. } => "Dropped",
            AiEvent::Paused(_) => "Paused",
            AiEvent::Usage { .. } => "Usage",
            AiEvent::TestResult(_) => "TestResult",
        })
    }
}

/// The UI's end of the AI command channel. Sending never blocks; a stopped worker is ignored.
#[derive(Clone)]
pub struct AiHandle {
    tx: mpsc::Sender<AiCommand>,
}

impl AiHandle {
    pub fn send(&self, cmd: AiCommand) {
        let _ = self.tx.send(cmd);
    }

    pub fn from_sender(tx: mpsc::Sender<AiCommand>) -> Self {
        Self { tx }
    }

    /// A handle nobody listens to (tests).
    #[cfg(test)]
    pub fn detached() -> Self {
        let (tx, _) = mpsc::channel();
        Self { tx }
    }
}

const fn assert_send<T: Send>() {}
// The keyring handle moves into the AI thread (SPEC 16.4).
const _: () = assert_send::<Secrets>();

/// Starts the `eloquawk-ai` worker. Demo mode passes no secrets and uses the offline checker.
pub fn start(
    mode: Mode,
    secrets: Option<Secrets>,
    prefs: &Prefs,
) -> std::io::Result<(AiHandle, async_channel::Receiver<AiEvent>)> {
    worker::start_with(mode, secrets, prefs, AiEndpoints::openrouter())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ai_event_debug_prints_variant_names_only() {
        let ev = AiEvent::Failed {
            thread: ThreadKey::new("secret@example.com", "secret"),
            message_id: "secret".into(),
            error: "secret".into(),
            retryable: true,
        };
        assert_eq!(format!("{ev:?}"), "Failed");
        assert_eq!(
            format!("{:?}", AiEvent::TestResult(Err("secret".into()))),
            "TestResult"
        );
    }
}
