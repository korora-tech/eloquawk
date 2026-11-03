//! The on-disk summary cache (SPEC 7.7): per account, the labels with their counts, page 1 of
//! each label view and the thread summaries those pages show, so a launch draws the lists at
//! once and only fetches what changed. Bodies are never cached.
//!
//! Each file is `MAGIC ‖ nonce ‖ XChaCha20-Poly1305(JSON)` with the account as associated
//! data, written atomically with mode 0600 under `$XDG_CACHE_HOME/eloquawk/`. The key lives
//! only in the Secret Service (`Secrets::cache_key`). A file that is missing, from another
//! key or format, or tampered with reads as no cache; it is then simply rewritten.

use std::collections::BTreeSet;
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

use chacha20poly1305::aead::{Aead as _, KeyInit as _, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};
use sha2::Digest as _;

use crate::model::{AccountId, Address, Label, LabelKind, ThreadKey, ThreadSummary, ViewKind};

/// Bumped whenever the plaintext shape changes; older files then read as no cache.
const MAGIC: &[u8; 8] = b"ELQCACH1";
const NONCE_LEN: usize = 24;

/// What a cache file holds for one account.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    pub labels: Vec<Label>,
    /// Page 1 of each cached view, in list order. Searches are never cached.
    pub views: Vec<(ViewKind, Vec<String>)>,
    pub threads: Vec<CachedThread>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CachedThread {
    pub star_target: Option<String>,
    pub summary: ThreadSummary,
}

/// Reads and writes the encrypted cache files.
pub struct MailCache {
    dir: PathBuf,
    cipher: XChaCha20Poly1305,
}

impl MailCache {
    pub fn new(dir: PathBuf, key: &[u8; 32]) -> Self {
        Self {
            dir,
            cipher: XChaCha20Poly1305::new(key.into()),
        }
    }

    /// `$XDG_CACHE_HOME/eloquawk`, or `~/.cache/eloquawk` when that is unset or relative.
    pub fn default_dir() -> Option<PathBuf> {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
            .map(|p| p.join("eloquawk"))
    }

    /// The account's file; named by a hash so the address is not in the file name.
    fn path(&self, account: &str) -> PathBuf {
        let hash = sha2::Sha256::digest(account.as_bytes());
        let name: String = hash[..12].iter().map(|b| format!("{b:02x}")).collect();
        self.dir.join(format!("{name}.cache"))
    }

    /// The account's snapshot, or `None` when there is none it can read.
    pub fn load(&self, account: &str) -> Option<Snapshot> {
        let raw = std::fs::read(self.path(account)).ok()?;
        let plain = self.open(account, &raw)?;
        let file: File = serde_json::from_slice(&plain).ok()?;
        Some(file.into_snapshot(account))
    }

    /// Replaces the account's snapshot.
    pub fn save(&self, account: &str, snapshot: &Snapshot) -> std::io::Result<()> {
        let plain = serde_json::to_vec(&File::from_snapshot(snapshot))?;
        let sealed = self.seal(account, &plain)?;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)?;
        let path = self.path(account);
        let tmp = path.with_extension("tmp");
        write_private(&tmp, &sealed)?;
        std::fs::rename(&tmp, &path)
    }

    /// Deletes the account's snapshot; succeeds when there is none.
    pub fn remove(&self, account: &str) -> std::io::Result<()> {
        match std::fs::remove_file(self.path(account)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    fn seal(&self, account: &str, plain: &[u8]) -> std::io::Result<Vec<u8>> {
        let mut nonce = [0u8; NONCE_LEN];
        getrandom::fill(&mut nonce).map_err(std::io::Error::other)?;
        let sealed = self
            .cipher
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: plain,
                    aad: account.as_bytes(),
                },
            )
            .map_err(|_| std::io::Error::other("encryption failed"))?;
        let mut out = Vec::with_capacity(MAGIC.len() + NONCE_LEN + sealed.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&sealed);
        Ok(out)
    }

    fn open(&self, account: &str, raw: &[u8]) -> Option<Vec<u8>> {
        let rest = raw.strip_prefix(MAGIC.as_slice())?;
        if rest.len() < NONCE_LEN {
            return None;
        }
        let (nonce, sealed) = rest.split_at(NONCE_LEN);
        self.cipher
            .decrypt(
                XNonce::from_slice(nonce),
                Payload {
                    msg: sealed,
                    aad: account.as_bytes(),
                },
            )
            .ok()
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

// ----- the plaintext format: explicit, so model changes cannot silently change it ----------

#[derive(Serialize, Deserialize)]
struct File {
    labels: Vec<FileLabel>,
    views: Vec<(FileView, Vec<String>)>,
    threads: Vec<FileThread>,
}

#[derive(Serialize, Deserialize)]
enum FileView {
    Label(String),
    AllMail,
}

#[derive(Serialize, Deserialize)]
struct FileLabel {
    id: String,
    name: String,
    user: bool,
    color: Option<(u32, u32)>,
    threads_unread: u32,
    threads_total: u32,
    messages_total: u32,
    hidden: bool,
}

#[derive(Serialize, Deserialize)]
struct FileThread {
    id: String,
    history_id: u64,
    star_target: Option<String>,
    subject: String,
    snippet: String,
    participants: Vec<(Option<String>, String, bool)>,
    last_date_ms: i64,
    message_count: u32,
    label_ids: BTreeSet<String>,
    has_attachments: bool,
}

impl File {
    fn from_snapshot(s: &Snapshot) -> Self {
        Self {
            labels: s
                .labels
                .iter()
                .map(|l| FileLabel {
                    id: l.id.clone(),
                    name: l.name.clone(),
                    user: l.kind == LabelKind::User,
                    color: l.color,
                    threads_unread: l.threads_unread,
                    threads_total: l.threads_total,
                    messages_total: l.messages_total,
                    hidden: l.hidden,
                })
                .collect(),
            views: s
                .views
                .iter()
                .filter_map(|(kind, ids)| {
                    let view = match kind {
                        ViewKind::Label(id) => FileView::Label(id.clone()),
                        ViewKind::AllMail => FileView::AllMail,
                        ViewKind::Search(_) => return None,
                    };
                    Some((view, ids.clone()))
                })
                .collect(),
            threads: s
                .threads
                .iter()
                .map(|t| {
                    let s = &t.summary;
                    FileThread {
                        id: s.key.id.clone(),
                        history_id: s.history_id,
                        star_target: t.star_target.clone(),
                        subject: s.subject.clone(),
                        snippet: s.snippet.clone(),
                        participants: s
                            .participants
                            .iter()
                            .map(|(a, unread)| (a.name.clone(), a.email.clone(), *unread))
                            .collect(),
                        last_date_ms: s.last_date.as_millisecond(),
                        message_count: s.message_count,
                        label_ids: s.label_ids.clone(),
                        has_attachments: s.has_attachments,
                    }
                })
                .collect(),
        }
    }

    fn into_snapshot(self, account: &str) -> Snapshot {
        let account: AccountId = account.to_string();
        Snapshot {
            labels: self
                .labels
                .into_iter()
                .map(|l| Label {
                    account: account.clone(),
                    id: l.id,
                    name: l.name,
                    kind: if l.user {
                        LabelKind::User
                    } else {
                        LabelKind::System
                    },
                    color: l.color,
                    threads_unread: l.threads_unread,
                    threads_total: l.threads_total,
                    messages_total: l.messages_total,
                    hidden: l.hidden,
                })
                .collect(),
            views: self
                .views
                .into_iter()
                .map(|(v, ids)| {
                    let kind = match v {
                        FileView::Label(id) => ViewKind::Label(id),
                        FileView::AllMail => ViewKind::AllMail,
                    };
                    (kind, ids)
                })
                .collect(),
            threads: self
                .threads
                .into_iter()
                .filter_map(|t| {
                    Some(CachedThread {
                        star_target: t.star_target,
                        summary: ThreadSummary {
                            key: ThreadKey::new(account.as_str(), t.id.as_str()),
                            history_id: t.history_id,
                            subject: t.subject,
                            snippet: t.snippet,
                            participants: t
                                .participants
                                .into_iter()
                                .map(|(name, email, unread)| (Address { name, email }, unread))
                                .collect(),
                            last_date: jiff::Timestamp::from_millisecond(t.last_date_ms).ok()?,
                            message_count: t.message_count,
                            label_ids: t.label_ids,
                            has_attachments: t.has_attachments,
                        },
                    })
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: &str = "me@example.com";

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "eloquawk-cache-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn snapshot() -> Snapshot {
        Snapshot {
            labels: vec![Label {
                account: ME.into(),
                id: "INBOX".into(),
                name: "INBOX".into(),
                kind: LabelKind::System,
                color: None,
                threads_unread: 3,
                threads_total: 9,
                messages_total: 12,
                hidden: false,
            }],
            views: vec![(ViewKind::Label("INBOX".into()), vec!["t1".into()])],
            threads: vec![CachedThread {
                star_target: Some("m2".into()),
                summary: ThreadSummary {
                    key: ThreadKey::new(ME, "t1"),
                    history_id: 77,
                    subject: "Quarterly figures".into(),
                    snippet: "See attached".into(),
                    participants: vec![(Address::new(Some("Alice"), "alice@example.com"), true)],
                    last_date: jiff::Timestamp::from_second(1_800_000_000).unwrap(),
                    message_count: 2,
                    label_ids: BTreeSet::from(["INBOX".to_string(), "UNREAD".to_string()]),
                    has_attachments: true,
                },
            }],
        }
    }

    #[test]
    fn round_trips_encrypted_and_private() {
        let d = dir("roundtrip");
        let cache = MailCache::new(d.clone(), &[7; 32]);
        cache.save(ME, &snapshot()).unwrap();
        assert_eq!(cache.load(ME), Some(snapshot()));
        let path = cache.path(ME);
        let raw = std::fs::read(&path).unwrap();
        assert!(raw.starts_with(MAGIC));
        assert!(
            !raw.windows(9).any(|w| w == b"Quarterly"),
            "no plaintext on disk"
        );
        assert!(!path.to_string_lossy().contains("example"));
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn wrong_key_account_or_tampering_reads_as_none() {
        let d = dir("tamper");
        let cache = MailCache::new(d.clone(), &[7; 32]);
        cache.save(ME, &snapshot()).unwrap();
        assert_eq!(MailCache::new(d.clone(), &[8; 32]).load(ME), None);
        // Another account's file renamed into place fails on the associated data.
        std::fs::copy(cache.path(ME), cache.path("you@example.com")).unwrap();
        assert_eq!(cache.load("you@example.com"), None);
        let mut raw = std::fs::read(cache.path(ME)).unwrap();
        let last = raw.len() - 1;
        raw[last] ^= 1;
        std::fs::write(cache.path(ME), raw).unwrap();
        assert_eq!(cache.load(ME), None);
        cache.remove(ME).unwrap();
        cache.remove(ME).unwrap();
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn searches_are_never_written() {
        let d = dir("search");
        let cache = MailCache::new(d.clone(), &[7; 32]);
        let mut s = snapshot();
        s.views
            .push((ViewKind::Search("from:bank".into()), vec!["t1".into()]));
        cache.save(ME, &s).unwrap();
        assert_eq!(cache.load(ME).unwrap().views.len(), 1);
        let _ = std::fs::remove_dir_all(d);
    }
}
