//! Dismissed nudges (openspec/specs/nudges): per account, thread id → the date (ms) of the newest message
//! when it was dismissed. Sealed like the summary cache (`cache::seal`, the keyring's cache
//! key, the account as associated data) in a file of its own, so a cache rebuild keeps it.
//! A file that is missing, from another key, or tampered with reads as no dismissals.

use std::os::unix::fs::DirBuilderExt as _;
use std::path::{Path, PathBuf};

use chacha20poly1305::{KeyInit as _, XChaCha20Poly1305};

use crate::cache::{hashed_name, open, seal, write_private};
use crate::nudges::{Dismissed, MAX_AGE};

const MAGIC: &[u8; 8] = b"ELQNUDG1";

pub struct DismissalStore {
    dir: PathBuf,
    cipher: XChaCha20Poly1305,
}

fn path_in(dir: &Path, account: &str) -> PathBuf {
    dir.join(format!("{}.nudges", hashed_name(account)))
}

impl DismissalStore {
    pub fn new(dir: PathBuf, key: &[u8; 32]) -> Self {
        Self {
            dir,
            cipher: XChaCha20Poly1305::new(key.into()),
        }
    }

    pub fn load(&self, account: &str) -> Dismissed {
        let read = || {
            let raw = std::fs::read(path_in(&self.dir, account)).ok()?;
            let plain = open(&self.cipher, MAGIC, account.as_bytes(), &raw)?;
            serde_json::from_slice(&plain).ok()
        };
        read().unwrap_or_default()
    }

    /// Replaces the account's dismissals, leaving out those too old to matter any more: a
    /// thread whose newest message is past the nudge window cannot be a nudge.
    pub fn save(&self, account: &str, dismissed: &Dismissed, now_ms: i64) -> std::io::Result<()> {
        let horizon = now_ms.saturating_sub(MAX_AGE.as_millis() as i64 * 2);
        let kept: Dismissed = dismissed
            .iter()
            .filter(|(_, date)| **date >= horizon)
            .map(|(id, date)| (id.clone(), *date))
            .collect();
        let sealed = seal(
            &self.cipher,
            MAGIC,
            account.as_bytes(),
            &serde_json::to_vec(&kept)?,
        )?;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)?;
        let path = path_in(&self.dir, account);
        let tmp = path.with_extension("tmp");
        write_private(&tmp, &sealed)?;
        std::fs::rename(&tmp, &path)
    }
}

/// Deletes the account's dismissals; succeeds when there are none. Needs no key.
pub fn remove(dir: &Path, account: &str) -> std::io::Result<()> {
    match std::fs::remove_file(path_in(dir, account)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: &str = "me@example.com";

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("eloquawk-nudges-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn round_trips_sealed_and_private() {
        let d = dir("roundtrip");
        let store = DismissalStore::new(d.clone(), &[7; 32]);
        let now = 1_800_000_000_000;
        let mut m = Dismissed::new();
        m.insert("thread-secret-id".into(), now - 1000);
        m.insert("old".into(), 5);
        store.save(ME, &m, now).unwrap();
        let loaded = store.load(ME);
        assert_eq!(loaded.len(), 1, "stale entries are pruned");
        assert_eq!(loaded["thread-secret-id"], now - 1000);
        let path = path_in(&d, ME);
        let raw = std::fs::read(&path).unwrap();
        assert!(
            !raw.windows(6).any(|w| w == b"secret"),
            "no plaintext on disk"
        );
        assert!(!path.to_string_lossy().contains("example"));
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        // Another key or account reads nothing.
        assert!(DismissalStore::new(d.clone(), &[8; 32]).load(ME).is_empty());
        assert!(store.load("you@example.com").is_empty());
        remove(&d, ME).unwrap();
        remove(&d, ME).unwrap();
        assert!(store.load(ME).is_empty());
        let _ = std::fs::remove_dir_all(d);
    }
}
