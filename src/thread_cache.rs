//! The on-disk thread store (SPEC 7.8): opened threads, bodies included, so the reader shows
//! a thread again without a request and works offline. Mail at rest stays encrypted.
//!
//! One file per thread under `<cache dir>/threads/<hash of account>/<hash of thread id>.thread`
//! (mode 0600 in 0700 folders). A file is `MAGIC ‖ nonce ‖ XChaCha20-Poly1305(JSON)` sealed
//! with the same keyring key as the summary cache, with the account and thread id as
//! associated data, so a file moved to another name or account fails to open. Neither the
//! address nor the thread id is in a file name.
//!
//! An entry carries the history id its thread had when fetched. The worker serves it only
//! while that still equals the history id the summary cache knows (or, with no connection,
//! whatever there is). The store is bounded: [`MAX_THREADS`] files and [`MAX_BYTES`] per
//! account, least recently written first out. A missing, foreign-key, old-format or
//! tampered file reads as not cached.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use chacha20poly1305::{KeyInit as _, XChaCha20Poly1305};
use serde::{Deserialize, Serialize};

use crate::cache::{hashed_name, open, seal, write_private};
use crate::mail::links::LinkTarget;
use crate::mail::rich::{ImageKind, Marks, RichDoc, RichImage, RichText, Segment};
use crate::model::{
    Address, Attachment, AuthResult, AuthResults, BodySource, LinkSignal, Message, MessageSignals,
    ThreadDetail, ThreadKey,
};

/// Bumped whenever the plaintext shape changes; older files then read as not cached.
const MAGIC: &[u8; 8] = b"ELQTHR01";
const SUBDIR: &str = "threads";
const EXT: &str = "thread";

/// Per account, the most threads kept and the most bytes they may take on disk.
pub const MAX_THREADS: usize = 1500;
pub const MAX_BYTES: u64 = 192 * 1024 * 1024;

pub struct ThreadCache {
    dir: PathBuf,
    cipher: XChaCha20Poly1305,
}

/// A cached thread and the history id it was fetched at.
#[derive(Clone, Debug, PartialEq)]
pub struct Cached {
    pub history_id: u64,
    pub detail: ThreadDetail,
}

impl ThreadCache {
    /// `dir` is the summary cache's folder; threads live in a subfolder of it.
    pub fn new(dir: PathBuf, key: &[u8; 32]) -> Self {
        Self {
            dir,
            cipher: XChaCha20Poly1305::new(key.into()),
        }
    }

    fn account_dir(&self, account: &str) -> PathBuf {
        account_dir(&self.dir, account)
    }

    fn path(&self, key: &ThreadKey) -> PathBuf {
        self.account_dir(&key.account)
            .join(format!("{}.{EXT}", hashed_name(&key.id)))
    }

    fn aad(key: &ThreadKey) -> Vec<u8> {
        let mut aad = key.account.as_bytes().to_vec();
        aad.push(0);
        aad.extend_from_slice(key.id.as_bytes());
        aad
    }

    pub fn load(&self, key: &ThreadKey) -> Option<Cached> {
        let raw = std::fs::read(self.path(key)).ok()?;
        let plain = open(&self.cipher, MAGIC, &Self::aad(key), &raw)?;
        let file: File = serde_json::from_slice(&plain).ok()?;
        file.into_cached(key)
    }

    pub fn save(&self, history_id: u64, detail: &ThreadDetail) -> std::io::Result<()> {
        let plain = serde_json::to_vec(&File::from_detail(history_id, detail))?;
        let sealed = seal(&self.cipher, MAGIC, &Self::aad(&detail.key), &plain)?;
        let dir = self.account_dir(&detail.key.account);
        make_private_dirs(&dir)?;
        let path = self.path(&detail.key);
        let tmp = path.with_extension("tmp");
        write_private(&tmp, &sealed)?;
        std::fs::rename(&tmp, &path)
    }

    /// Forgets one thread; succeeds when it was not cached.
    pub fn remove(&self, key: &ThreadKey) {
        let _ = std::fs::remove_file(self.path(key));
    }

    /// Drops the oldest entries of `account` beyond the bounds, and leftovers of an
    /// interrupted write. Returns how many entries remain.
    pub fn prune(&self, account: &str, max_threads: usize, max_bytes: u64) -> usize {
        let Ok(entries) = std::fs::read_dir(self.account_dir(account)) else {
            return 0;
        };
        let mut files = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else { continue };
            if path.extension().is_some_and(|e| e == "tmp") {
                let _ = std::fs::remove_file(&path);
                continue;
            }
            let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            files.push((modified, meta.len(), path));
        }
        // Newest first: what survives is a prefix.
        files.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.2.cmp(&b.2)));
        let (mut count, mut bytes) = (0usize, 0u64);
        for (_, len, path) in files {
            if count < max_threads && bytes.saturating_add(len) <= max_bytes {
                count += 1;
                bytes += len;
            } else {
                let _ = std::fs::remove_file(path);
            }
        }
        count
    }

    /// Forgets every cached thread of `account` (a reset, when none can be trusted).
    pub fn clear(&self, account: &str) {
        let _ = remove_account(&self.dir, account);
    }
}

fn account_dir(dir: &Path, account: &str) -> PathBuf {
    dir.join(SUBDIR).join(hashed_name(account))
}

/// Deletes every cached thread of `account`; succeeds when there are none. Needs no key.
pub fn remove_account(dir: &Path, account: &str) -> std::io::Result<()> {
    let removed = match std::fs::remove_dir_all(account_dir(dir, account)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    };
    // The shared folder goes with the last account's; it stays while another has threads.
    let _ = std::fs::remove_dir(dir.join(SUBDIR));
    removed
}

fn make_private_dirs(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

// ----- the plaintext format: explicit, so model changes cannot silently change it ----------

#[derive(Serialize, Deserialize)]
struct File {
    history_id: u64,
    subject: String,
    label_ids: BTreeSet<String>,
    messages: Vec<FileMessage>,
}

type FileAddress = (Option<String>, String);

#[derive(Serialize, Deserialize)]
struct FileMessage {
    id: String,
    thread_id: String,
    label_ids: BTreeSet<String>,
    from: FileAddress,
    to: Vec<FileAddress>,
    cc: Vec<FileAddress>,
    bcc: Vec<FileAddress>,
    reply_to: Vec<FileAddress>,
    date_ms: i64,
    subject: String,
    message_id: Option<String>,
    in_reply_to: Option<String>,
    references: Vec<String>,
    snippet: String,
    body: String,
    /// 0 plain, 1 HTML, 2 empty.
    body_source: u8,
    body_truncated: bool,
    /// The sanitised HTML body, which cannot be rebuilt: the HTML itself is not kept.
    #[serde(default)]
    rich: Option<FileRich>,
    attachments: Vec<FileAttachment>,
    signals: FileSignals,
}

#[derive(Serialize, Deserialize)]
struct FileRich {
    main: Vec<FileSegment>,
    quote: Option<Vec<FileSegment>>,
    truncated: bool,
}

#[derive(Serialize, Deserialize)]
enum FileSegment {
    Text {
        text: String,
        /// (start, end, flags)
        marks: Vec<(usize, usize, u8)>,
        /// (start, end, mail, url)
        links: Vec<(usize, usize, bool, String)>,
    },
    Image {
        url: String,
        insecure: bool,
        alt: String,
        width: Option<u32>,
        height: Option<u32>,
        /// (mail, url)
        link: Option<(bool, String)>,
    },
}

fn link_target(mail: bool, url: String) -> LinkTarget {
    if mail {
        LinkTarget::Mail(url)
    } else {
        LinkTarget::Web(url)
    }
}

fn file_link(t: &LinkTarget) -> (bool, String) {
    (matches!(t, LinkTarget::Mail(_)), t.url().to_string())
}

impl FileRich {
    fn from_doc(d: &RichDoc) -> Self {
        let segs = |v: &[Segment]| v.iter().map(FileSegment::from_segment).collect();
        Self {
            main: segs(&d.main),
            quote: d.quote.as_deref().map(segs),
            truncated: d.truncated,
        }
    }

    /// `None` when any range would not index its text, so a file that does not hold what the
    /// reader slices by is never shown.
    fn into_doc(self) -> Option<RichDoc> {
        let segs = |v: Vec<FileSegment>| {
            v.into_iter()
                .map(FileSegment::into_segment)
                .collect::<Option<Vec<_>>>()
        };
        Some(RichDoc {
            main: segs(self.main)?,
            quote: match self.quote {
                Some(q) => Some(segs(q)?),
                None => None,
            },
            truncated: self.truncated,
        })
    }
}

impl FileSegment {
    fn from_segment(s: &Segment) -> Self {
        match s {
            Segment::Text(t) => Self::Text {
                text: t.text.clone(),
                marks: t
                    .marks
                    .iter()
                    .map(|(r, m)| (r.start, r.end, m.bits()))
                    .collect(),
                links: t
                    .links
                    .iter()
                    .map(|(r, l)| {
                        let (mail, url) = file_link(l);
                        (r.start, r.end, mail, url)
                    })
                    .collect(),
            },
            Segment::Image(i) => Self::Image {
                url: i.url.clone(),
                insecure: i.kind == ImageKind::Insecure,
                alt: i.alt.clone(),
                width: i.width,
                height: i.height,
                link: i.link.as_ref().map(file_link),
            },
        }
    }

    fn into_segment(self) -> Option<Segment> {
        Some(match self {
            Self::Text { text, marks, links } => {
                let fits = |s: usize, e: usize| {
                    s <= e && text.is_char_boundary(s) && text.is_char_boundary(e)
                };
                if !marks.iter().all(|&(s, e, _)| fits(s, e))
                    || !links.iter().all(|&(s, e, ..)| fits(s, e))
                {
                    return None;
                }
                Segment::Text(RichText {
                    marks: marks
                        .into_iter()
                        .map(|(s, e, m)| (s..e, Marks::from_bits(m)))
                        .collect(),
                    links: links
                        .into_iter()
                        .map(|(s, e, mail, url)| (s..e, link_target(mail, url)))
                        .collect(),
                    text,
                })
            }
            Self::Image {
                url,
                insecure,
                alt,
                width,
                height,
                link,
            } => Segment::Image(RichImage {
                url,
                kind: if insecure {
                    ImageKind::Insecure
                } else {
                    ImageKind::Remote
                },
                alt,
                width,
                height,
                link: link.map(|(mail, url)| link_target(mail, url)),
            }),
        })
    }
}

#[derive(Serialize, Deserialize)]
struct FileAttachment {
    part_id: String,
    attachment_id: Option<String>,
    filename: String,
    mime_type: String,
    size: u64,
}

#[derive(Serialize, Deserialize)]
struct FileSignals {
    spf: String,
    dkim: String,
    dmarc: String,
    dkim_domains: Vec<String>,
    return_path_domain: Option<String>,
    list_unsubscribe: bool,
    links: Vec<(String, String, Option<String>)>,
}

fn addr(a: &Address) -> FileAddress {
    (a.name.clone(), a.email.clone())
}

fn unaddr((name, email): FileAddress) -> Address {
    Address { name, email }
}

impl File {
    fn from_detail(history_id: u64, d: &ThreadDetail) -> Self {
        Self {
            history_id,
            subject: d.subject.clone(),
            label_ids: d.label_ids.clone(),
            messages: d.messages.iter().map(FileMessage::from_message).collect(),
        }
    }

    fn into_cached(self, key: &ThreadKey) -> Option<Cached> {
        let messages = self
            .messages
            .into_iter()
            .map(FileMessage::into_message)
            .collect::<Option<Vec<_>>>()?;
        Some(Cached {
            history_id: self.history_id,
            detail: ThreadDetail {
                key: key.clone(),
                subject: self.subject,
                label_ids: self.label_ids,
                messages,
            },
        })
    }
}

impl FileMessage {
    fn from_message(m: &Message) -> Self {
        let addrs = |v: &[Address]| v.iter().map(addr).collect();
        Self {
            id: m.id.clone(),
            thread_id: m.thread_id.clone(),
            label_ids: m.label_ids.clone(),
            from: addr(&m.from),
            to: addrs(&m.to),
            cc: addrs(&m.cc),
            bcc: addrs(&m.bcc),
            reply_to: addrs(&m.reply_to),
            date_ms: m.date.as_millisecond(),
            subject: m.subject.clone(),
            message_id: m.message_id.clone(),
            in_reply_to: m.in_reply_to.clone(),
            references: m.references.clone(),
            snippet: m.snippet.clone(),
            body: m.body.clone(),
            body_source: match m.body_source {
                BodySource::Plain => 0,
                BodySource::Html => 1,
                BodySource::Empty => 2,
            },
            body_truncated: m.body_truncated,
            rich: m.rich.as_deref().map(FileRich::from_doc),
            attachments: m
                .attachments
                .iter()
                .map(|a| FileAttachment {
                    part_id: a.part_id.clone(),
                    attachment_id: a.attachment_id.clone(),
                    filename: a.filename.clone(),
                    mime_type: a.mime_type.clone(),
                    size: a.size,
                })
                .collect(),
            signals: FileSignals {
                spf: m.signals.auth.spf.as_str().into(),
                dkim: m.signals.auth.dkim.as_str().into(),
                dmarc: m.signals.auth.dmarc.as_str().into(),
                dkim_domains: m.signals.auth.dkim_domains.clone(),
                return_path_domain: m.signals.return_path_domain.clone(),
                list_unsubscribe: m.signals.list_unsubscribe,
                links: m
                    .signals
                    .links
                    .iter()
                    .map(|l| (l.text.clone(), l.host.clone(), l.text_host.clone()))
                    .collect(),
            },
        }
    }

    fn into_message(self) -> Option<Message> {
        let addrs = |v: Vec<FileAddress>| v.into_iter().map(unaddr).collect();
        Some(Message {
            id: self.id,
            thread_id: self.thread_id,
            label_ids: self.label_ids,
            from: unaddr(self.from),
            to: addrs(self.to),
            cc: addrs(self.cc),
            bcc: addrs(self.bcc),
            reply_to: addrs(self.reply_to),
            date: jiff::Timestamp::from_millisecond(self.date_ms).ok()?,
            subject: self.subject,
            message_id: self.message_id,
            in_reply_to: self.in_reply_to,
            references: self.references,
            snippet: self.snippet,
            body: self.body,
            body_source: match self.body_source {
                0 => BodySource::Plain,
                1 => BodySource::Html,
                2 => BodySource::Empty,
                _ => return None,
            },
            body_truncated: self.body_truncated,
            rich: match self.rich {
                Some(r) => Some(Arc::new(r.into_doc()?)),
                None => None,
            },
            attachments: self
                .attachments
                .into_iter()
                .map(|a| Attachment {
                    part_id: a.part_id,
                    attachment_id: a.attachment_id,
                    filename: a.filename,
                    mime_type: a.mime_type,
                    size: a.size,
                })
                .collect(),
            signals: MessageSignals {
                auth: AuthResults {
                    spf: AuthResult::parse(&self.signals.spf),
                    dkim: AuthResult::parse(&self.signals.dkim),
                    dmarc: AuthResult::parse(&self.signals.dmarc),
                    dkim_domains: self.signals.dkim_domains,
                },
                return_path_domain: self.signals.return_path_domain,
                list_unsubscribe: self.signals.list_unsubscribe,
                links: self
                    .signals
                    .links
                    .into_iter()
                    .map(|(text, host, text_host)| LinkSignal {
                        text,
                        host,
                        text_host,
                    })
                    .collect(),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: &str = "me@example.com";

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "eloquawk-threads-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn detail(id: &str, body: &str) -> ThreadDetail {
        let alice = Address::new(Some("Alice"), "alice@example.com");
        ThreadDetail {
            key: ThreadKey::new(ME, id),
            subject: "Quarterly figures".into(),
            label_ids: BTreeSet::from(["INBOX".to_string()]),
            messages: vec![Message {
                id: format!("m-{id}"),
                thread_id: id.into(),
                label_ids: BTreeSet::from(["INBOX".to_string(), "UNREAD".to_string()]),
                from: alice.clone(),
                to: vec![Address::new(None, ME)],
                cc: vec![alice],
                bcc: vec![],
                reply_to: vec![],
                date: jiff::Timestamp::from_second(1_800_000_000).unwrap(),
                subject: "Quarterly figures".into(),
                message_id: Some("<a@example.com>".into()),
                in_reply_to: None,
                references: vec!["<r@example.com>".into()],
                snippet: "See attached".into(),
                body: body.into(),
                body_source: BodySource::Html,
                body_truncated: true,
                rich: Some(Arc::new(RichDoc {
                    main: vec![
                        Segment::Text(RichText {
                            text: "Hello café".into(),
                            marks: vec![(0..5, Marks::BOLD.union(Marks::ITALIC))],
                            links: vec![(6..11, LinkTarget::Web("https://example.com/c".into()))],
                        }),
                        Segment::Image(RichImage {
                            url: "https://img.example.com/a.png".into(),
                            kind: ImageKind::Remote,
                            alt: "A picture".into(),
                            width: Some(10),
                            height: None,
                            link: Some(LinkTarget::Mail("mailto:a@example.com".into())),
                        }),
                    ],
                    quote: Some(vec![Segment::Text(RichText {
                        text: "quoted".into(),
                        marks: vec![],
                        links: vec![],
                    })]),
                    truncated: true,
                })),
                attachments: vec![Attachment {
                    part_id: "2".into(),
                    attachment_id: Some("ATT".into()),
                    filename: "report.pdf".into(),
                    mime_type: "application/pdf".into(),
                    size: 1234,
                }],
                signals: MessageSignals {
                    auth: AuthResults {
                        spf: AuthResult::Pass,
                        dkim: AuthResult::Fail,
                        dmarc: AuthResult::SoftFail,
                        dkim_domains: vec!["example.com".into()],
                    },
                    return_path_domain: Some("example.com".into()),
                    list_unsubscribe: true,
                    links: vec![LinkSignal {
                        text: "bank.com".into(),
                        host: "evil.test".into(),
                        text_host: Some("bank.com".into()),
                    }],
                },
            }],
        }
    }

    #[test]
    fn round_trips_encrypted_and_private() {
        let d = dir("roundtrip");
        let cache = ThreadCache::new(d.clone(), &[7; 32]);
        let t = detail("t1", "Secret body text");
        cache.save(77, &t).unwrap();
        assert_eq!(
            cache.load(&t.key),
            Some(Cached {
                history_id: 77,
                detail: t.clone()
            })
        );
        let path = cache.path(&t.key);
        let raw = std::fs::read(&path).unwrap();
        assert!(raw.starts_with(MAGIC));
        for needle in [&b"Secret body"[..], b"Quarterly", b"alice@", b"report.pdf"] {
            assert!(!raw.windows(needle.len()).any(|w| w == needle), "plaintext");
        }
        let shown = path.to_string_lossy();
        assert!(!shown.contains("example") && !shown.contains("t1"));
        use std::os::unix::fs::PermissionsExt as _;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn wrong_key_name_or_tampering_reads_as_not_cached() {
        let d = dir("tamper");
        let cache = ThreadCache::new(d.clone(), &[7; 32]);
        let t = detail("t1", "body");
        cache.save(1, &t).unwrap();
        assert_eq!(ThreadCache::new(d.clone(), &[8; 32]).load(&t.key), None);
        // One thread's file renamed to another's, or to another account's, fails on the
        // associated data.
        let other = ThreadKey::new(ME, "t2");
        std::fs::copy(cache.path(&t.key), cache.path(&other)).unwrap();
        assert_eq!(cache.load(&other), None);
        let foreign = ThreadKey::new("you@example.com", "t1");
        std::fs::create_dir_all(cache.account_dir("you@example.com")).unwrap();
        std::fs::copy(cache.path(&t.key), cache.path(&foreign)).unwrap();
        assert_eq!(cache.load(&foreign), None);
        let mut raw = std::fs::read(cache.path(&t.key)).unwrap();
        let last = raw.len() - 1;
        raw[last] ^= 1;
        std::fs::write(cache.path(&t.key), raw).unwrap();
        assert_eq!(cache.load(&t.key), None);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn remove_forgets_one_thread_and_an_account() {
        let d = dir("remove");
        let cache = ThreadCache::new(d.clone(), &[7; 32]);
        let (a, b) = (detail("a", "x"), detail("b", "y"));
        cache.save(1, &a).unwrap();
        cache.save(1, &b).unwrap();
        cache.remove(&a.key);
        cache.remove(&a.key);
        assert!(cache.load(&a.key).is_none() && cache.load(&b.key).is_some());
        remove_account(&d, ME).unwrap();
        remove_account(&d, ME).unwrap();
        assert!(cache.load(&b.key).is_none());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn prune_keeps_the_newest_within_both_bounds() {
        let d = dir("prune");
        let cache = ThreadCache::new(d.clone(), &[7; 32]);
        for i in 0..5 {
            cache
                .save(1, &detail(&format!("t{i}"), &"x".repeat(100)))
                .unwrap();
            // File times differ by whole milliseconds on every filesystem we run on.
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        std::fs::write(cache.account_dir(ME).join("stale.tmp"), b"junk").unwrap();
        assert_eq!(cache.prune(ME, 3, u64::MAX), 3);
        let kept: Vec<bool> = (0..5)
            .map(|i| cache.load(&ThreadKey::new(ME, format!("t{i}"))).is_some())
            .collect();
        assert_eq!(kept, [false, false, true, true, true], "oldest go first");
        assert!(!cache.account_dir(ME).join("stale.tmp").exists());
        let size = std::fs::metadata(cache.path(&ThreadKey::new(ME, "t4")))
            .unwrap()
            .len();
        assert_eq!(
            cache.prune(ME, 100, size * 2),
            2,
            "bytes bound the count too"
        );
        assert_eq!(cache.prune("nobody@example.com", 3, 1), 0);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_rich_body_whose_ranges_do_not_fit_reads_as_not_cached() {
        let seg = |marks| FileSegment::Text {
            text: "café".into(),
            marks,
            links: vec![],
        };
        assert!(seg(vec![(0, 3, 1)]).into_segment().is_some());
        // Past the end, inverted, and inside the two bytes of "é".
        for bad in [(0, 99, 1), (3, 1, 1), (0, 4, 1)] {
            assert!(seg(vec![bad]).into_segment().is_none(), "{bad:?}");
        }
    }
}
