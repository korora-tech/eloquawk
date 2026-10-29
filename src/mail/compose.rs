//! Outgoing mail: [`Outgoing`] to RFC 5322 bytes with lettre's builder (SPEC 10), plus the
//! small pure helpers the composer and the workers share (address validation, MIME types by
//! extension, the 25 MB attachment budget and bounded reads of local files).
//!
//! [`build`] is pure: the clock and the Message-ID are injected. Bcc is kept in the bytes on
//! purpose (`keep_bcc`), because Gmail reads Bcc from the raw message and strips it on
//! delivery.

use std::fmt;
use std::io::Read as _;
use std::path::Path;
use std::str::FromStr as _;
use std::time::SystemTime;

use jiff::Timestamp;
use lettre::message::header::ContentType;
use lettre::message::{Attachment, Mailbox, MultiPart, SinglePart};

use crate::model::{Address, MAX_OUTGOING_ATTACHMENTS, Outgoing, OutgoingAttachment};

#[derive(Debug, PartialEq, Eq)]
pub enum ComposeError {
    /// An address lettre would not accept; carries the address as typed.
    Address(String),
    /// No To, Cc or Bcc.
    NoRecipients,
    /// lettre refused the message.
    Build(String),
}

impl fmt::Display for ComposeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ComposeError::Address(a) => write!(f, "\u{201c}{a}\u{201d} is not a valid address"),
            ComposeError::NoRecipients => f.write_str("Add at least one recipient"),
            ComposeError::Build(e) => write!(f, "Couldn't build the message: {e}"),
        }
    }
}

/// Whether lettre accepts `email` as an address (what [`build`] will need).
pub fn valid_email(email: &str) -> bool {
    lettre::Address::from_str(email.trim()).is_ok()
}

fn mailbox(a: &Address) -> Result<Mailbox, ComposeError> {
    let email = lettre::Address::from_str(a.email.trim())
        .map_err(|_| ComposeError::Address(a.email.clone()))?;
    let name = a
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string);
    Ok(Mailbox::new(name, email))
}

/// A Message-ID in the form `<16 hex digits@eloquawk.local>`.
pub fn message_id_from(random: [u8; 8]) -> String {
    let hex: String = random.iter().map(|b| format!("{b:02x}")).collect();
    format!("<{hex}@eloquawk.local>")
}

/// A fresh random Message-ID.
pub fn new_message_id() -> String {
    let mut random = [0u8; 8];
    if let Err(e) = getrandom::fill(&mut random) {
        // Uniqueness only matters for threading; the time keeps ids apart well enough.
        eprintln!("eloquawk: no randomness for a Message-ID ({e}); using the clock");
        let nanos = Timestamp::now().as_nanosecond() as u64;
        random = nanos.to_le_bytes();
    }
    message_id_from(random)
}

/// Builds the RFC 5322 bytes of `out` with `attachments` (each with its bytes) at `now`.
pub fn build(
    out: &Outgoing,
    attachments: &[(OutgoingAttachment, Vec<u8>)],
    now: Timestamp,
    message_id: &str,
) -> Result<Vec<u8>, ComposeError> {
    if out.to.is_empty() && out.cc.is_empty() && out.bcc.is_empty() {
        return Err(ComposeError::NoRecipients);
    }
    let mut b = lettre::Message::builder()
        .from(mailbox(&out.from)?)
        .keep_bcc();
    for a in &out.to {
        b = b.to(mailbox(a)?);
    }
    for a in &out.cc {
        b = b.cc(mailbox(a)?);
    }
    for a in &out.bcc {
        b = b.bcc(mailbox(a)?);
    }
    b = b
        .subject(out.subject.clone())
        .message_id(Some(message_id.to_string()))
        .date(SystemTime::from(now))
        .user_agent(format!("Eloquawk/{}", env!("CARGO_PKG_VERSION")));
    if let Some(id) = out.in_reply_to.as_deref().filter(|s| !s.trim().is_empty()) {
        b = b.in_reply_to(id.trim().to_string());
    }
    if !out.references.is_empty() {
        b = b.references(out.references.join(" "));
    }
    let body = out.body.replace("\r\n", "\n");
    let message = if attachments.is_empty() {
        b.singlepart(SinglePart::plain(body))
    } else {
        let mut mixed = MultiPart::mixed().singlepart(SinglePart::plain(body));
        for (a, bytes) in attachments {
            let ct = ContentType::parse(&a.mime_type).unwrap_or_else(|_| octet_stream());
            mixed = mixed.singlepart(Attachment::new(a.filename.clone()).body(bytes.clone(), ct));
        }
        b.multipart(mixed)
    }
    .map_err(|e| ComposeError::Build(e.to_string()))?;
    Ok(message.formatted())
}

fn octet_stream() -> ContentType {
    ContentType::parse("application/octet-stream").unwrap_or(ContentType::TEXT_PLAIN)
}

/// A MIME type guessed from a file name's extension; `application/octet-stream` otherwise.
pub fn mime_for(filename: &str) -> &'static str {
    let ext = filename
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "txt" | "log" => "text/plain",
        "md" => "text/markdown",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "ics" => "text/calendar",
        "json" => "application/json",
        "zip" => "application/zip",
        "gz" => "application/gzip",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "odt" => "application/vnd.oasis.opendocument.text",
        "ods" => "application/vnd.oasis.opendocument.spreadsheet",
        "mp3" => "audio/mpeg",
        "mp4" => "video/mp4",
        _ => "application/octet-stream",
    }
}

/// The composer's inline error when an attachment would break the 25 MB budget.
pub const TOO_LARGE_TO_ATTACH: &str = "Attachments can't exceed 25 MB in total";
/// The worker's error when the bytes it read break the budget.
pub const TOO_LARGE_TO_SEND: &str = "Attachments are larger than 25 MB.";

/// Whether a file of `size` bytes still fits next to `current`.
pub fn fits(current: &[OutgoingAttachment], size: u64) -> bool {
    let used: u64 = current.iter().map(|a| a.size).sum();
    used.saturating_add(size) <= MAX_OUTGOING_ATTACHMENTS
}

/// Reads a local attachment, reading at most `budget + 1` bytes so a file that grew past the
/// budget since it was attached is caught without reading it all. `Err` carries a message for
/// the user.
pub fn read_local(path: &Path, budget: u64) -> Result<Vec<u8>, String> {
    let file = std::fs::File::open(path).map_err(|e| {
        format!(
            "Couldn't read {}: {}",
            path.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            e.kind()
        )
    })?;
    let mut bytes = Vec::new();
    file.take(budget.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|e| format!("Couldn't read an attachment: {}", e.kind()))?;
    if bytes.len() as u64 > budget {
        return Err(TOO_LARGE_TO_SEND.to_string());
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use mail_parser::MessageParser;

    use super::*;
    use crate::model::{AttachmentSource, ComposeKind};

    fn addr(name: Option<&str>, email: &str) -> Address {
        Address::new(name, email)
    }

    fn outgoing() -> Outgoing {
        Outgoing {
            account: "you@example.com".into(),
            kind: ComposeKind::Reply,
            from: addr(Some("You"), "you@example.com"),
            to: vec![addr(Some("Zoë Ångström"), "zoe@example.org")],
            cc: vec![addr(None, "cc@example.org")],
            bcc: vec![addr(Some("Hidden"), "bcc@example.org")],
            subject: "Re: Café ☕ plans".into(),
            body: format!(
                "Ça marche, à midi.\n\n{}\n> quoted line\n",
                "long ".repeat(60).trim_end()
            ),
            thread_id: Some("t1".into()),
            in_reply_to: Some("<m2@mail.example>".into()),
            references: vec!["<m1@mail.example>".into(), "<m2@mail.example>".into()],
            attachments: vec![],
        }
    }

    fn now() -> Timestamp {
        "2025-10-03T09:30:00Z".parse().unwrap()
    }

    #[test]
    fn round_trips_headers_and_body() {
        let id = message_id_from([0xde, 0xad, 0xbe, 0xef, 0, 1, 2, 3]);
        assert_eq!(id, "<deadbeef00010203@eloquawk.local>");
        let raw = build(&outgoing(), &[], now(), &id).unwrap();
        let text = String::from_utf8_lossy(&raw);
        assert!(text.contains("\r\n"), "CRLF line endings");
        let m = MessageParser::default().parse(&raw[..]).unwrap();

        let first = |l: Option<mail_parser::AddressList<'_>>| {
            let mb = l.and_then(|l| l.first()).unwrap();
            (
                mb.name().map(str::to_string),
                mb.address().map(str::to_string),
            )
        };
        assert_eq!(
            first(m.from()),
            (Some("You".into()), Some("you@example.com".into()))
        );
        assert_eq!(
            first(m.to()),
            (Some("Zoë Ångström".into()), Some("zoe@example.org".into()))
        );
        assert_eq!(first(m.cc()).1.as_deref(), Some("cc@example.org"));
        // Bcc stays in the bytes for Gmail.
        assert_eq!(
            first(m.bcc()),
            (Some("Hidden".into()), Some("bcc@example.org".into()))
        );
        assert_eq!(m.subject(), Some("Re: Café ☕ plans"));
        assert_eq!(m.message_id(), Some("deadbeef00010203@eloquawk.local"));
        assert_eq!(
            m.in_reply_to().and_then(|l| l.first()),
            Some("m2@mail.example")
        );
        let refs: Vec<&str> = m
            .references()
            .map(|l| l.iter().collect())
            .unwrap_or_default();
        assert_eq!(refs, ["m1@mail.example", "m2@mail.example"]);
        assert_eq!(m.date().map(|d| d.to_timestamp()), Some(now().as_second()));
        // Long lines and non-ASCII survive; the transport adds a final line break.
        let body = m.body_text(0).unwrap().replace("\r\n", "\n");
        assert_eq!(body.trim_end(), outgoing().body.trim_end());
        assert!(!m.has_attachments());
        // Deterministic apart from nothing: same inputs, same bytes.
        assert_eq!(raw, build(&outgoing(), &[], now(), &id).unwrap());
    }

    #[test]
    fn attachments_make_a_mixed_multipart() {
        let mut out = outgoing();
        let att = OutgoingAttachment {
            filename: "plan.pdf".into(),
            mime_type: "application/pdf".into(),
            size: 4,
            source: AttachmentSource::Local("/tmp/plan.pdf".into()),
        };
        let odd = OutgoingAttachment {
            filename: "notes.weird".into(),
            mime_type: "not a type".into(),
            size: 3,
            source: AttachmentSource::Local("/tmp/notes.weird".into()),
        };
        out.attachments = vec![att.clone(), odd.clone()];
        let bytes = vec![0x25, 0x50, 0x44, 0x46];
        let raw = build(
            &out,
            &[(att, bytes.clone()), (odd, b"abc".to_vec())],
            now(),
            "<x@eloquawk.local>",
        )
        .unwrap();
        let m = MessageParser::default().parse(&raw[..]).unwrap();
        assert!(
            m.content_type()
                .is_some_and(|c| c.subtype() == Some("mixed"))
        );
        assert_eq!(
            m.body_text(0).unwrap().replace("\r\n", "\n").trim_end(),
            outgoing().body.trim_end()
        );
        let parts: Vec<_> = m.attachments().collect();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].attachment_name(), Some("plan.pdf"));
        assert!(parts[0].is_content_type("application", "pdf"));
        assert_eq!(&parts[0].decoded()[..], &bytes[..]);
        assert_eq!(parts[1].attachment_name(), Some("notes.weird"));
        assert!(parts[1].is_content_type("application", "octet-stream"));
        assert_eq!(&parts[1].decoded()[..], b"abc");
    }

    #[test]
    fn bad_addresses_and_no_recipients_are_errors() {
        let mut out = outgoing();
        out.to = vec![addr(None, "not an address")];
        assert_eq!(
            build(&out, &[], now(), "<x@y>"),
            Err(ComposeError::Address("not an address".into()))
        );
        out.to.clear();
        out.cc.clear();
        out.bcc.clear();
        assert_eq!(
            build(&out, &[], now(), "<x@y>"),
            Err(ComposeError::NoRecipients)
        );
        assert!(valid_email("a@b.example"));
        assert!(valid_email(" a@b.example "));
        assert!(!valid_email("a@"));
        assert!(!valid_email("plain text"));
    }

    #[test]
    fn new_message_ids_are_random() {
        let (a, b) = (new_message_id(), new_message_id());
        assert_ne!(a, b);
        assert!(a.starts_with('<') && a.ends_with("@eloquawk.local>"));
        assert_eq!(a.len(), "<@eloquawk.local>".len() + 16);
    }

    #[test]
    fn mime_types_and_budget() {
        assert_eq!(mime_for("Report.PDF"), "application/pdf");
        assert_eq!(mime_for("photo.jpeg"), "image/jpeg");
        assert_eq!(mime_for("README"), "application/octet-stream");
        let a = |size| OutgoingAttachment {
            filename: "a".into(),
            mime_type: "text/plain".into(),
            size,
            source: AttachmentSource::Local("/a".into()),
        };
        assert!(fits(&[], MAX_OUTGOING_ATTACHMENTS));
        assert!(!fits(&[], MAX_OUTGOING_ATTACHMENTS + 1));
        assert!(fits(&[a(20 * 1024 * 1024)], 5 * 1024 * 1024));
        assert!(!fits(&[a(20 * 1024 * 1024)], 5 * 1024 * 1024 + 1));
        assert!(!fits(&[a(u64::MAX)], 1));
    }

    #[test]
    fn local_reads_stop_past_the_budget() {
        let dir = std::env::temp_dir().join(format!("eloquawk-compose-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ten.bin");
        std::fs::write(&path, [7u8; 10]).unwrap();
        assert_eq!(read_local(&path, 10).unwrap(), vec![7u8; 10]);
        assert_eq!(read_local(&path, 9), Err(TOO_LARGE_TO_SEND.to_string()));
        assert!(read_local(&dir.join("missing"), 10).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
