//! AI triage (SPEC 16): a decision model flags phishing, scams and spam. The checker is Jev,
//! TypeSafe's model reached through OpenRouter's Decisions API, or Cloudflare's Clef or
//! Clef-flash on Workers AI ([`AiBackend`]). Advisory only: nothing here moves or labels mail.
//!
//! The UI builds a small, redacted [`prompt::AiInput`] and sends it as a [`AiCommand::Check`]
//! to the `eloquawk-ai` thread ([`worker`]), which owns the checker's key, the HTTP calls,
//! retries, pauses and the spend limit, and answers with [`AiEvent`]s. Demo mode runs the same
//! worker with an offline checker ([`fake`]) and never touches the network or the keyring.

pub mod cache;
pub mod cloudflare;
pub mod fake;
pub mod jev;
#[cfg(test)]
mod live;
pub mod openrouter;
pub mod prompt;
pub mod worker;

use std::fmt;
use std::sync::mpsc;

use serde::{Deserialize, Serialize};

use crate::config::Prefs;
use crate::model::{AccountId, ThreadKey};
use crate::secrets::Secrets;
use crate::store::Mode;

use jev::Verdict;
use openrouter::{AiEndpoints, ApiKey, Vendor};
use prompt::AiInput;

/// Whether opt-in Jev diagnostics are enabled.
pub fn debug_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("ELOQUAWK_JEV_DEBUG").as_deref() == Ok("1"))
}

/// Diagnostics include label names and scores, but no message contents or credentials.
pub fn debug(args: fmt::Arguments<'_>) {
    if debug_enabled() {
        eprintln!("eloquawk: jev: debug: {args}");
    }
}

pub fn key_state_name(state: &KeyState) -> &'static str {
    match state {
        KeyState::Unknown => "unknown",
        KeyState::Missing => "missing",
        KeyState::Saved { .. } => "saved",
        KeyState::Locked => "locked",
        KeyState::Rejected { .. } => "rejected",
        KeyState::Demo => "demo",
    }
}

/// The pinned model; its thresholds are tuned for this version (SPEC 16.1).
pub const JEV_MODEL: &str = "typesafe/jev-1.13";
/// The disclosure text's version. Any change to what is sent bumps it, which turns every
/// account off until the user accepts again (SPEC 16.2). Version 2 (M17) adds the label
/// names sent with each check of a message you open.
pub const DISCLOSURE_VERSION: u32 = 2;

/// The decision model that checks messages, and so who receives them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiBackend {
    /// TypeSafe's Jev through OpenRouter.
    #[default]
    Jev,
    /// Cloudflare's Clef on Workers AI.
    Clef,
    /// Cloudflare's Clef-flash on Workers AI.
    ClefFlash,
}

impl AiBackend {
    pub const ALL: [AiBackend; 3] = [AiBackend::Jev, AiBackend::Clef, AiBackend::ClefFlash];

    pub fn vendor(self) -> Vendor {
        match self {
            AiBackend::Jev => Vendor::OpenRouter,
            AiBackend::Clef | AiBackend::ClefFlash => Vendor::Cloudflare,
        }
    }

    /// The model's name as the UI shows it.
    pub fn model_name(self) -> &'static str {
        match self {
            AiBackend::Jev => "Jev",
            AiBackend::Clef => "Clef",
            AiBackend::ClefFlash => "Clef-flash",
        }
    }

    /// Who the requests go to.
    pub fn vendor_name(self) -> &'static str {
        match self.vendor() {
            Vendor::OpenRouter => "OpenRouter",
            Vendor::Cloudflare => "Cloudflare",
        }
    }

    /// "Jev (OpenRouter)": the checker choice in Settings.
    pub fn label(self) -> String {
        format!("{} ({})", self.model_name(), self.vendor_name())
    }

    /// Workers AI's model selector, for Clef only.
    pub fn clef_selector(self) -> Option<&'static str> {
        match self {
            AiBackend::Jev => None,
            AiBackend::Clef => Some("clef"),
            AiBackend::ClefFlash => Some("clef-flash"),
        }
    }
}

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
    pub guidance: String,
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
    /// The checker or the Cloudflare account ID changed: drop everything, forget every result
    /// and load the new checker's key.
    Backend {
        backend: AiBackend,
        account_id: String,
    },
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
    worker::start_with(mode, secrets, prefs, AiEndpoints::production())
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
