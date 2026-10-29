//! Non-secret preferences in `$XDG_CONFIG_HOME/eloquawk/settings.json` (SPEC 4.3).
//!
//! Credentials are never written here (see `secrets.rs`), and no mail data either, apart from
//! the last selected label id. Saving is an atomic replace, and an `ephemeral` Prefs (demo
//! mode, tests) never touches the disk.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::AccountId;

/// The colour scheme the user picked.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemePref {
    /// Follow the desktop's light or dark appearance.
    #[default]
    System,
    #[allow(dead_code)] // consumed in M4
    Light,
    #[allow(dead_code)] // consumed in M4
    Dark,
}

pub const REFRESH_SECS_MIN: u64 = 30;
pub const REFRESH_SECS_MAX: u64 = 900;
pub const PAGE_SIZE_MIN: u32 = 10;
pub const PAGE_SIZE_MAX: u32 = 100;

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
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        std::fs::write(&path, r#"{"refresh_secs":5,"page_size":1000}"#).unwrap();
        let p = Prefs::load_from(&path);
        assert_eq!((p.refresh_secs, p.page_size), (30, 100));
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
