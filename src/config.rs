//! Non-secret preferences in `$XDG_CONFIG_HOME/eloquawk/settings.json` (SPEC 4.3).
//!
//! Credentials are never written here (see `secrets.rs`), and no mail data either, apart from
//! the last selected label id. Saving is an atomic replace, and an `ephemeral` Prefs (demo
//! mode, tests) never touches the disk.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::ai::AiBackend;
use crate::model::AccountId;

/// The colour scheme the user picked.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemePref {
    /// Follow the desktop's light or dark appearance.
    #[default]
    System,
    Light,
    Dark,
}

pub const REFRESH_SECS_MIN: u64 = 30;
pub const REFRESH_SECS_MAX: u64 = 900;
pub const PAGE_SIZE_MIN: u32 = 10;
pub const PAGE_SIZE_MAX: u32 = 100;

pub const LABEL_GUIDANCE_MAX_CHARS: usize = 500;

pub const SPEND_LIMIT_CENTS_MIN: u32 = 10;
pub const SPEND_LIMIT_CENTS_MAX: u32 = 10_000;

/// The AI message checks (SPEC 16.9). Holds no key and no mail.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AiPrefs {
    /// The disclosure version each account accepted; an absent account is off.
    pub accounts: BTreeMap<AccountId, u32>,
    /// Suggest labels when a message is opened (M17; on by default).
    pub suggest_labels: bool,
    /// User-authored label meanings, stored locally and sent with label questions.
    pub label_guidance: String,
    /// Optional parent label whose best-scoring child is always offered.
    pub required_label_group: String,
    /// Check new inbox mail as it arrives (decision 2: off by default).
    pub background: bool,
    /// Checks stop for the session at this much spend, in cents, clamped to 10..=10_000.
    pub spend_limit_cents: u32,
    /// Which model checks messages, and so who receives them.
    pub backend: AiBackend,
    /// The Cloudflare account Clef requests name; not a secret. Only a valid ID is used.
    pub cloudflare_account_id: String,
    /// The checker and Cloudflare account `accounts` consented to. Consent never carries over
    /// to another checker or account, however the prefs changed.
    pub consent_backend: AiBackend,
    pub consent_account_id: String,
}

impl Default for AiPrefs {
    fn default() -> Self {
        Self {
            accounts: BTreeMap::new(),
            suggest_labels: true,
            label_guidance: String::new(),
            required_label_group: String::new(),
            background: false,
            spend_limit_cents: 100,
            backend: AiBackend::Jev,
            cloudflare_account_id: String::new(),
            consent_backend: AiBackend::Jev,
            consent_account_id: String::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    pub theme: ThemePref,
    pub clock_24h: bool,
    pub mark_read_on_open: bool,
    pub notifications: bool,
    /// Seconds between history polls, clamped to 30..=900.
    pub refresh_secs: u64,
    /// Threads per list page, clamped to 10..=100.
    pub page_size: u32,
    /// The account and label id shown last; restored at start.
    pub last_view: Option<(AccountId, String)>,
    pub collapsed_accounts: BTreeSet<AccountId>,
    /// Collapsed user label trees, keyed by account and Gmail label id.
    pub collapsed_labels: BTreeSet<(AccountId, String)>,
    /// The sidebar is folded to a narrow rail of icons.
    pub sidebar_collapsed: bool,
    /// The reader shows a conversation's latest message at the top.
    pub newest_first: bool,
    /// The AI message checks (M16).
    pub ai: AiPrefs,
    /// Demo mode and tests: never written to disk.
    #[serde(skip)]
    pub ephemeral: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            theme: ThemePref::System,
            clock_24h: true,
            mark_read_on_open: true,
            notifications: true,
            refresh_secs: 60,
            page_size: 50,
            last_view: None,
            collapsed_accounts: BTreeSet::new(),
            collapsed_labels: BTreeSet::new(),
            sidebar_collapsed: false,
            newest_first: true,
            ai: AiPrefs::default(),
            ephemeral: false,
        }
    }
}

/// `$XDG_CONFIG_HOME/eloquawk`, or `~/.config/eloquawk` when that is unset or relative.
pub fn config_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .map(|p| p.join("eloquawk"))
}

fn settings_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("settings.json"))
}

impl Prefs {
    /// Loads from the XDG path; anything missing or unreadable falls back to defaults.
    pub fn load() -> Self {
        settings_path()
            .map(|p| Self::load_from(&p))
            .unwrap_or_default()
    }

    /// Writes to the XDG path unless ephemeral.
    pub fn save(&self) {
        if let Some(path) = settings_path() {
            self.save_to(&path);
        }
    }

    pub fn load_from(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(raw) => serde_json::from_str::<Prefs>(&raw)
                .map(Prefs::clamped)
                .unwrap_or_else(|e| {
                    eprintln!("eloquawk: ignoring unreadable {}: {e}", path.display());
                    Self::default()
                }),
            Err(_) => Self::default(),
        }
    }

    /// Atomic replace: writes a temp file next to `path`, then renames it. Does nothing when
    /// ephemeral.
    pub fn save_to(&self, path: &Path) {
        if self.ephemeral {
            return;
        }
        let result = (|| -> std::io::Result<()> {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let json = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, json)?;
            std::fs::rename(&tmp, path)
        })();
        if let Err(e) = result {
            eprintln!("eloquawk: could not save settings: {e}");
        }
    }

    /// Pulls out-of-range numbers back into their allowed ranges.
    pub fn clamped(mut self) -> Self {
        self.refresh_secs = self.refresh_secs.clamp(REFRESH_SECS_MIN, REFRESH_SECS_MAX);
        self.page_size = self.page_size.clamp(PAGE_SIZE_MIN, PAGE_SIZE_MAX);
        self.ai.spend_limit_cents = self
            .ai
            .spend_limit_cents
            .clamp(SPEND_LIMIT_CENTS_MIN, SPEND_LIMIT_CENTS_MAX);
        let ai = &mut self.ai;
        let clef = ai.backend.clef_selector().is_some();
        if ai.consent_backend != ai.backend
            || clef && ai.consent_account_id != ai.cloudflare_account_id
        {
            ai.accounts.clear();
        }
        ai.consent_backend = ai.backend;
        ai.consent_account_id = if clef {
            ai.cloudflare_account_id.clone()
        } else {
            String::new()
        };
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consent_is_dropped_with_another_checker_or_account() {
        let consented = |backend, account: &str| {
            let mut p = Prefs::default();
            p.ai.accounts.insert("you@example.com".into(), 2);
            p.ai.backend = backend;
            p.ai.cloudflare_account_id = account.into();
            p.clamped()
        };
        // Existing Jev consent survives; a hand-edited checker does not inherit it.
        let jev = consented(AiBackend::Jev, "");
        assert_eq!(jev.ai.accounts.len(), 1);
        let mut edited = jev.clone();
        edited.ai.backend = AiBackend::Clef;
        assert!(edited.clamped().ai.accounts.is_empty());
        // Within Clef, a new account ID also drops it; Jev ignores the account ID.
        let mut clef = jev.clone();
        clef.ai.backend = AiBackend::Clef;
        clef.ai.cloudflare_account_id = "a".repeat(32);
        let mut clef = clef.clamped();
        clef.ai.accounts.insert("you@example.com".into(), 2);
        let clef = clef.clamped();
        assert_eq!(clef.ai.accounts.len(), 1);
        let mut moved = clef.clone();
        moved.ai.cloudflare_account_id = "b".repeat(32);
        assert!(moved.clamped().ai.accounts.is_empty());
        let mut jev_acct = jev;
        jev_acct.ai.cloudflare_account_id = "b".repeat(32);
        assert_eq!(jev_acct.clamped().ai.accounts.len(), 1);
    }

    /// A fresh directory under the system temp dir, removed when dropped.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let dir = std::env::temp_dir().join(format!(
                "eloquawk-test-{tag}-{}-{nanos}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn unknown_and_missing_fields_fall_back_to_defaults() {
        let p: Prefs = serde_json::from_str(r#"{"theme":"dark","future":1}"#).unwrap();
        assert_eq!(p.theme, ThemePref::Dark);
        assert!(p.clock_24h && p.mark_read_on_open && p.notifications);
        assert_eq!((p.refresh_secs, p.page_size), (60, 50));
        assert!(!p.ephemeral);
        // A settings file from before M16 has no `ai`: checks are off, with the defaults.
        assert_eq!(p.ai, AiPrefs::default());
        assert!(p.ai.accounts.is_empty() && !p.ai.background && p.ai.suggest_labels);
        assert_eq!(p.ai.spend_limit_cents, 100);
    }

    #[test]
    fn prefs_round_trip_through_save_and_load() {
        let dir = TempDir::new("roundtrip");
        let path = dir.0.join("nested").join("settings.json");
        let p = Prefs {
            theme: ThemePref::Light,
            clock_24h: false,
            mark_read_on_open: false,
            notifications: false,
            refresh_secs: 300,
            page_size: 25,
            last_view: Some(("you@example.com".into(), "Label_7".into())),
            collapsed_accounts: ["you@work.example".to_string()].into(),
            collapsed_labels: [("you@example.com".into(), "Label_7".into())].into(),
            sidebar_collapsed: true,
            newest_first: false,
            ai: AiPrefs {
                accounts: [("you@example.com".to_string(), 1)].into(),
                suggest_labels: false,
                label_guidance: "TTL/M3 means retain for three months".into(),
                required_label_group: "TTL".into(),
                background: true,
                spend_limit_cents: 250,
                backend: AiBackend::ClefFlash,
                cloudflare_account_id: "0123456789abcdef0123456789abcdef".into(),
                consent_backend: AiBackend::ClefFlash,
                consent_account_id: "0123456789abcdef0123456789abcdef".into(),
            },
            ephemeral: false,
        };
        p.save_to(&path);
        assert_eq!(Prefs::load_from(&path), p);
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn ephemeral_never_saves() {
        let dir = TempDir::new("ephemeral");
        let path = dir.0.join("settings.json");
        let p = Prefs {
            ephemeral: true,
            ..Prefs::default()
        };
        p.save_to(&path);
        assert!(!path.exists());
    }

    #[test]
    fn out_of_range_numbers_are_clamped_on_load() {
        let dir = TempDir::new("clamp");
        let path = dir.0.join("settings.json");
        std::fs::write(
            &path,
            r#"{"refresh_secs":5,"page_size":1000,"ai":{"spend_limit_cents":1}}"#,
        )
        .unwrap();
        let p = Prefs::load_from(&path);
        assert_eq!((p.refresh_secs, p.page_size), (30, 100));
        assert_eq!(p.ai.spend_limit_cents, SPEND_LIMIT_CENTS_MIN);
        std::fs::write(&path, r#"{"ai":{"spend_limit_cents":99999}}"#).unwrap();
        assert_eq!(
            Prefs::load_from(&path).ai.spend_limit_cents,
            SPEND_LIMIT_CENTS_MAX
        );
    }

    #[test]
    fn unreadable_or_missing_files_give_defaults() {
        let dir = TempDir::new("bad");
        let path = dir.0.join("settings.json");
        assert_eq!(Prefs::load_from(&path), Prefs::default());
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(Prefs::load_from(&path), Prefs::default());
    }
}
