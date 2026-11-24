//! The on-disk summary cache (SPEC 7.7): per account, the labels with their counts, page 1 of
//! each label view and the thread summaries those pages show, so a launch draws the lists at
//! once and only fetches what changed. Bodies are never cached.
//!
//! Each file is `MAGIC ‖ nonce ‖ XChaCha20-Poly1305(JSON)` with the account as associated
//! data, written atomically with mode 0600 under `$XDG_CACHE_HOME/eloquawk/`. The key lives
//! only in the Secret Service (`Secrets::cache_key`). A file that is missing, from another
//! key or format, or tampered with reads as no cache; it is then simply rewritten.
//!
//! The snapshot also carries the History cursor it was taken at, so a relaunch can follow
//! the changes since then instead of listing everything again. Message bodies have their own
//! store, `thread_cache.rs`, sealed with the same key.

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
    /// The Gmail history id this snapshot is consistent with: every change up to it is
    /// already reflected. `None` when it was taken mid-round, which forces a full sync.
    pub history_id: Option<u64>,
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

    fn path(&self, account: &str) -> PathBuf {
        path_in(&self.dir, account)
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

    fn seal(&self, account: &str, plain: &[u8]) -> std::io::Result<Vec<u8>> {
        seal(&self.cipher, MAGIC, account.as_bytes(), plain)
    }

    fn open(&self, account: &str, raw: &[u8]) -> Option<Vec<u8>> {
        open(&self.cipher, MAGIC, account.as_bytes(), raw)
    }
}

/// `magic ‖ nonce ‖ XChaCha20-Poly1305(plain)` with `aad` authenticated but not stored. A
/// fresh random nonce each time, so one key can seal any number of files.
pub(crate) fn seal(
    cipher: &XChaCha20Poly1305,
    magic: &[u8; 8],
    aad: &[u8],
    plain: &[u8],
) -> std::io::Result<Vec<u8>> {
    let mut nonce = [0u8; NONCE_LEN];
    getrandom::fill(&mut nonce).map_err(std::io::Error::other)?;
    let sealed = cipher
        .encrypt(&XNonce::from(nonce), Payload { msg: plain, aad })
        .map_err(|_| std::io::Error::other("encryption failed"))?;
    let mut out = Vec::with_capacity(magic.len() + NONCE_LEN + sealed.len());
    out.extend_from_slice(magic);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&sealed);
    Ok(out)
}

/// The inverse of [`seal`]; `None` for another format, key or `aad`, or a changed byte.
pub(crate) fn open(
    cipher: &XChaCha20Poly1305,
    magic: &[u8; 8],
    aad: &[u8],
    raw: &[u8],
) -> Option<Vec<u8>> {
    let rest = raw.strip_prefix(magic.as_slice())?;
    if rest.len() < NONCE_LEN {
        return None;
    }
    let (nonce, sealed) = rest.split_at(NONCE_LEN);
    let nonce = XNonce::try_from(nonce).ok()?;
    cipher.decrypt(&nonce, Payload { msg: sealed, aad }).ok()
}

/// First 12 bytes of the SHA-256 of `text`, in hex: a file name that does not leak `text`.
pub(crate) fn hashed_name(text: &str) -> String {
    let hash = sha2::Sha256::digest(text.as_bytes());
    hash[..12].iter().map(|b| format!("{b:02x}")).collect()
}

/// The account's file in `dir`; named by a hash so the address is not in the file name.
fn path_in(dir: &Path, account: &str) -> PathBuf {
    dir.join(format!("{}.cache", hashed_name(account)))
}

/// Deletes the account's snapshot and cached threads in `dir`; succeeds when there are none.
/// Needs no key, so signing out clears them even when the keyring never gave one.
pub fn remove(dir: &Path, account: &str) -> std::io::Result<()> {
    let snapshot = match std::fs::remove_file(path_in(dir, account)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    };
    let threads = crate::thread_cache::remove_account(dir, account);
    snapshot.and(threads)
}

pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
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
    /// Absent in files written before the cursor was kept, which then resume nothing.
    #[serde(default)]
    history_id: Option<u64>,
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
            history_id: s.history_id,
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
            history_id: self.history_id,
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
            history_id: Some(4242),
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

    /// A file sealed by the chacha20poly1305 0.10 build must keep opening: the format on disk
    /// is the same XChaCha20-Poly1305 whatever the crate version, and this pins it.
    #[test]
    fn opens_a_file_sealed_by_an_earlier_release() {
        let hex = "454c514341434831201ec73e0ba3157793cfd8dde67110e932cb6da563074949f7a9197883a54516a4baf62a362d616f5bbb4ff4f27d43679fc9d3fb3c8731e880";
        let raw: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect();
        let cache = MailCache::new(PathBuf::from("/nonexistent"), &[7u8; 32]);
        assert_eq!(
            cache.open("me@example.com", &raw).as_deref(),
            Some(br#"{"hello":"cache"}"#.as_slice())
        );
        // The account is bound in as associated data.
        assert!(cache.open("other@example.com", &raw).is_none());
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
        remove(&d, ME).unwrap();
        remove(&d, ME).unwrap();
        assert!(!cache.path(ME).exists());
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
