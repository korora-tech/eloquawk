//! Gmail DTOs to domain types: messages (headers, body choice, charsets, attachments),
//! thread summaries and details, and labels.
//!
//! Everything here is defensive: Gmail payloads come from arbitrary senders, so a missing
//! header, payload or body never fails a conversion; it yields an empty field instead.

use std::collections::BTreeSet;

use encoding_rs::{Encoding, WINDOWS_1252};
use jiff::Timestamp;

use crate::google::gmail::{Header, LabelDto, MessageDto, PartDto, ThreadDto};
use crate::model::{
    AccountId, Address, Attachment, BodySource, Label, LabelKind, Message, ThreadDetail, ThreadKey,
    ThreadSummary, labels,
};

use super::address::{parse_address, parse_list, participants};
use super::html::{decode_entities, html_to_text};

/// The value of the first header called `name`, compared case-insensitively.
pub fn header<'a>(headers: &'a [Header], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str())
}

/// Decodes RFC 2047 encoded-words that Gmail left in a header. Text without encoded-words,
/// or that fails to decode, is returned as it is.
pub fn decode_header(value: &str) -> String {
    if value.contains("=?") {
        match rfc2047_decoder::decode(value) {
            Ok(s) => return s,
            Err(_) => return value.to_string(),
        }
    }
    value.to_string()
}

/// Parses `internalDate` (milliseconds since the epoch, as a string).
pub fn internal_date(value: Option<&str>) -> Option<Timestamp> {
    let ms: i64 = value?.trim().parse().ok()?;
    Timestamp::from_millisecond(ms).ok()
}

/// A Gmail numeric string (id, `historyId`); anything unparseable is 0.
pub fn parse_u64(s: &str) -> u64 {
    s.trim().parse().unwrap_or(0)
}

/// A parameter of a `Content-Type`-like header value, unquoted, e.g. `charset`.
fn header_param<'a>(value: &'a str, name: &str) -> Option<&'a str> {
    value.split(';').skip(1).find_map(|p| {
        let (k, v) = p.split_once('=')?;
        k.trim()
            .eq_ignore_ascii_case(name)
            .then(|| v.trim().trim_matches('"'))
    })
}

/// Decodes body bytes. Bytes that are valid UTF-8 are taken as UTF-8 whatever the declared
/// charset (Gmail may already have converted them; SPEC 15 item 1). Otherwise the declared
/// charset is used, falling back to windows-1252, which never fails.
pub fn decode_text(bytes: &[u8], charset: Option<&str>) -> String {
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    let enc = charset
        .and_then(|c| Encoding::for_label(c.trim().as_bytes()))
        .unwrap_or(WINDOWS_1252);
    enc.decode(bytes).0.into_owned()
}

fn part_text(part: &PartDto) -> Option<String> {
    let data = part.body.as_ref()?.data.as_deref()?;
    let bytes = super::b64::decode(data).ok()?;
    let charset = header(&part.headers, "Content-Type").and_then(|v| header_param(v, "charset"));
    Some(decode_text(&bytes, charset).replace("\r\n", "\n"))
}

fn is_attachment(part: &PartDto) -> bool {
    !part.filename.is_empty()
        || header(&part.headers, "Content-Disposition").is_some_and(|d| {
            d.trim_start()
                .get(..10)
                .is_some_and(|s| s.eq_ignore_ascii_case("attachment"))
        })
        || part
            .body
            .as_ref()
            .is_some_and(|b| b.attachment_id.is_some())
            && !part.mime_type.starts_with("text/")
}

#[derive(Default)]
struct Walk<'a> {
    /// Every inline text/plain part, in order: a multipart/mixed body may be split around an
    /// inline image or similar, and all of its text belongs in the body.
    plain: Vec<&'a PartDto>,
    html: Option<&'a PartDto>,
    attachments: Vec<Attachment>,
    /// A text part Gmail returned by `attachmentId` only (a large body); its text is not in
    /// the payload, so the body is incomplete.
    unfetched_text: bool,
}

fn is_unfetched(part: &PartDto) -> bool {
    part.body
        .as_ref()
        .is_some_and(|b| b.attachment_id.is_some() && b.data.is_none())
}

fn walk<'a>(part: &'a PartDto, out: &mut Walk<'a>) {
    if is_attachment(part) {
        let body = part.body.clone().unwrap_or_default();
        out.attachments.push(Attachment {
            part_id: part.part_id.clone(),
            attachment_id: body.attachment_id,
            filename: if part.filename.is_empty() {
                "attachment".to_string()
            } else {
                decode_header(&part.filename)
            },
            mime_type: if part.mime_type.is_empty() {
                "application/octet-stream".to_string()
            } else {
                part.mime_type.to_ascii_lowercase()
            },
            size: body.size,
        });
        return;
    }
    let mime = part.mime_type.to_ascii_lowercase();
    if !part.parts.is_empty() {
        for p in &part.parts {
            walk(p, out);
        }
    } else if mime == "text/plain" || (mime.is_empty() && part.body.is_some()) {
        out.unfetched_text |= is_unfetched(part);
        out.plain.push(part);
    } else if mime == "text/html" {
        out.unfetched_text |= is_unfetched(part);
        out.html.get_or_insert(part);
    }
}

/// Joins inline text parts with a blank line between them, trimming the trailing whitespace
/// of each part.
fn join_text_parts(texts: &[String]) -> String {
    texts
        .iter()
        .map(|t| t.trim_end())
        .filter(|t| !t.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Converts a Gmail message (`format=full` or `metadata`) into a [`Message`]. Plain text is
/// preferred over HTML; HTML goes through `html_to_text`, falling back to the snippet when the
/// conversion fails.
pub fn parse_message(dto: &MessageDto) -> Message {
    let empty = PartDto::default();
    let payload = dto.payload.as_ref().unwrap_or(&empty);
    let h = |name: &str| header(&payload.headers, name);
    let snippet = decode_entities(&dto.snippet);

    let mut found = Walk::default();
    walk(payload, &mut found);
    let texts: Vec<String> = found.plain.iter().filter_map(|p| part_text(p)).collect();
    let plain = (!texts.is_empty())
        .then(|| join_text_parts(&texts))
        .filter(|t| !t.trim().is_empty() || found.html.is_none());
    let (body, body_source, body_truncated) = match (plain, found.html) {
        (Some(text), _) => (text, BodySource::Plain, found.unfetched_text),
        (None, Some(part)) => match part_text(part).map(|t| html_to_text(&t)) {
            Some(Some(c)) => (
                c.text,
                BodySource::Html,
                c.truncated || found.unfetched_text,
            ),
            Some(None) => (snippet.clone(), BodySource::Html, true),
            None if found.unfetched_text => (snippet.clone(), BodySource::Empty, true),
            None => (String::new(), BodySource::Empty, false),
        },
        // Only an unfetched text part: show the snippet and flag the body as incomplete.
        (None, None) if found.unfetched_text => (snippet.clone(), BodySource::Empty, true),
        (None, None) => (String::new(), BodySource::Empty, false),
    };

    let date = internal_date(dto.internal_date.as_deref())
        .or_else(|| {
            let zoned = jiff::fmt::rfc2822::parse(h("Date")?.trim()).ok()?;
            Some(zoned.timestamp())
        })
        .unwrap_or(Timestamp::UNIX_EPOCH);
    let list = |name: &str| h(name).map(parse_list).unwrap_or_default();

    Message {
        id: dto.id.clone(),
        thread_id: dto.thread_id.clone(),
        label_ids: dto.label_ids.iter().cloned().collect(),
        from: h("From")
            .and_then(parse_address)
            .unwrap_or_else(|| Address::new(None, "")),
        to: list("To"),
        cc: list("Cc"),
        bcc: list("Bcc"),
        reply_to: list("Reply-To"),
        date,
        subject: h("Subject").map(decode_header).unwrap_or_default(),
        message_id: h("Message-ID").map(|v| v.trim().to_string()),
        in_reply_to: h("In-Reply-To").map(|v| v.trim().to_string()),
        references: h("References")
            .map(|v| v.split_whitespace().map(str::to_string).collect())
            .unwrap_or_default(),
        snippet,
        body,
        body_source,
        body_truncated,
        attachments: found.attachments,
    }
}

/// The thread's subject: the first message's non-empty subject.
fn thread_subject(dto: &ThreadDto) -> String {
    dto.messages
        .iter()
        .filter_map(|m| m.payload.as_ref())
        .filter_map(|p| header(&p.headers, "Subject"))
        .map(decode_header)
        .find(|s| !s.trim().is_empty())
        .unwrap_or_default()
}

fn label_union(dto: &ThreadDto) -> BTreeSet<String> {
    dto.messages
        .iter()
        .flat_map(|m| m.label_ids.iter().cloned())
        .collect()
}

/// Builds the list-row summary of a thread from `format=metadata` (or `full`). `None` when the
/// thread has no messages.
///
/// `has_attachments` is true when any message has an attachment part (full format) or its
/// payload is `multipart/mixed` (the metadata heuristic, SPEC 15 item 2).
pub fn thread_summary(account: &AccountId, dto: &ThreadDto) -> Option<ThreadSummary> {
    let last = dto.messages.last()?;
    let dates: Vec<Timestamp> = dto
        .messages
        .iter()
        .filter_map(|m| internal_date(m.internal_date.as_deref()))
        .collect();
    let senders = dto.messages.iter().filter_map(|m| {
        let from = m
            .payload
            .as_ref()
            .and_then(|p| header(&p.headers, "From"))?;
        let unread = m.label_ids.iter().any(|l| l == labels::UNREAD);
        parse_address(from).map(|a| (a, unread))
    });
    let has_attachments = dto
        .messages
        .iter()
        .filter_map(|m| m.payload.as_ref())
        .any(|p| {
            p.mime_type.eq_ignore_ascii_case("multipart/mixed") || {
                let mut w = Walk::default();
                walk(p, &mut w);
                !w.attachments.is_empty()
            }
        });
    let history_id = match parse_u64(&dto.history_id) {
        0 => dto
            .messages
            .iter()
            .map(|m| parse_u64(&m.history_id))
            .max()
            .unwrap_or(0),
        n => n,
    };
    Some(ThreadSummary {
        key: ThreadKey::new(account.clone(), dto.id.clone()),
        history_id,
        subject: thread_subject(dto),
        snippet: decode_entities(&last.snippet),
        participants: participants(senders),
        last_date: dates.into_iter().max().unwrap_or(Timestamp::UNIX_EPOCH),
        message_count: u32::try_from(dto.messages.len()).unwrap_or(u32::MAX),
        label_ids: label_union(dto),
        has_attachments,
    })
}

/// Converts a `format=full` thread into the reader's detail.
pub fn thread_detail(account: &AccountId, dto: &ThreadDto) -> ThreadDetail {
    ThreadDetail {
        key: ThreadKey::new(account.clone(), dto.id.clone()),
        subject: thread_subject(dto),
        label_ids: label_union(dto),
        messages: dto.messages.iter().map(parse_message).collect(),
    }
}

/// Parses a "#rrggbb" colour.
fn parse_color(s: Option<&str>) -> Option<u32> {
    let hex = s?.trim().strip_prefix('#')?;
    (hex.len() == 6)
        .then(|| u32::from_str_radix(hex, 16).ok())
        .flatten()
}

/// Converts a Gmail label. Counts absent from the DTO (as in `labels.list`) are 0.
pub fn label(account: &AccountId, dto: &LabelDto) -> Label {
    let color = dto.color.as_ref().and_then(|c| {
        Some((
            parse_color(c.text_color.as_deref())?,
            parse_color(c.background_color.as_deref())?,
        ))
    });
    Label {
        account: account.clone(),
        id: dto.id.clone(),
        name: dto.name.clone(),
        kind: if dto.kind.as_deref() == Some("user") {
            LabelKind::User
        } else {
            LabelKind::System
        },
        color,
        threads_unread: dto.threads_unread.unwrap_or(0),
        threads_total: dto.threads_total.unwrap_or(0),
        messages_total: dto.messages_total.unwrap_or(0),
        hidden: dto.label_list_visibility.as_deref() == Some("labelHide"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::google::gmail::fixtures;
    use crate::google::gmail::{BodyDto, LabelList};
    use base64::Engine;

    fn hdr(name: &str, value: &str) -> Header {
        Header {
            name: name.into(),
            value: value.into(),
        }
    }

    fn data(bytes: &[u8]) -> Option<BodyDto> {
        Some(BodyDto {
            attachment_id: None,
            size: bytes.len() as u64,
            data: Some(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)),
        })
    }

    fn leaf(id: &str, mime: &str, headers: Vec<Header>, bytes: &[u8]) -> PartDto {
        PartDto {
            part_id: id.into(),
            mime_type: mime.into(),
            headers,
            body: data(bytes),
            ..Default::default()
        }
    }

    fn multipart(mime: &str, parts: Vec<PartDto>) -> PartDto {
        PartDto {
            mime_type: mime.into(),
            parts,
            ..Default::default()
        }
    }

    fn msg(payload: Option<PartDto>) -> MessageDto {
        MessageDto {
            id: "m".into(),
            thread_id: "t".into(),
            label_ids: vec!["INBOX".into()],
            snippet: "the &amp; snippet".into(),
            history_id: "5".into(),
            internal_date: Some("1700000000000".into()),
            payload,
        }
    }

    fn with_headers(mut p: PartDto, headers: Vec<Header>) -> PartDto {
        p.headers = headers;
        p
    }

    #[test]
    fn alternative_prefers_plain() {
        let p = multipart(
            "multipart/alternative",
            vec![
                leaf("1", "text/html", vec![], b"<p>html <b>body</b></p>"),
                leaf("0", "text/plain", vec![], b"plain body\r\nline two\r\n"),
            ],
        );
        let m = parse_message(&msg(Some(p)));
        assert_eq!(m.body, "plain body\nline two");
        assert_eq!(m.body_source, BodySource::Plain);
        assert!(!m.body_truncated);
    }

    #[test]
    fn html_only_goes_through_html_to_text() {
        let p = leaf(
            "",
            "text/html",
            vec![hdr("Content-Type", "text/html; charset=utf-8")],
            b"<p>Hello <a href=\"https://x.example/\">there</a></p><img src=\"https://t.example/p\">",
        );
        let m = parse_message(&msg(Some(p)));
        assert_eq!(m.body, "Hello there <https://x.example/>");
        assert_eq!(m.body_source, BodySource::Html);
    }

    #[test]
    fn empty_plain_alternative_falls_back_to_html() {
        let p = multipart(
            "multipart/alternative",
            vec![
                leaf("0", "text/plain", vec![], b"  \r\n"),
                leaf("1", "text/html", vec![], b"<p>real</p>"),
            ],
        );
        assert_eq!(parse_message(&msg(Some(p))).body, "real");
    }

    #[test]
    fn mixed_body_split_around_an_inline_part_is_joined() {
        let mut img = leaf("1", "image/png", vec![], b"\x89PNG");
        img.headers = vec![hdr("Content-Disposition", "inline")];
        img.body = Some(BodyDto {
            attachment_id: Some("img".into()),
            size: 4,
            data: None,
        });
        let p = multipart(
            "multipart/mixed",
            vec![
                leaf("0", "text/plain", vec![], b"before\r\n"),
                img,
                leaf("2", "text/plain", vec![], b"after\r\n"),
            ],
        );
        let m = parse_message(&msg(Some(p)));
        assert_eq!(m.body, "before\n\nafter");
        assert_eq!(m.body_source, BodySource::Plain);
        assert!(!m.body_truncated);
        assert_eq!(m.attachments.len(), 1);
    }

    #[test]
    fn text_body_by_attachment_id_only_is_flagged_truncated() {
        let mut big = leaf("0", "text/plain", vec![], b"");
        big.body = Some(BodyDto {
            attachment_id: Some("big".into()),
            size: 900_000,
            data: None,
        });
        let m = parse_message(&msg(Some(big.clone())));
        assert!(m.body_truncated);
        assert_eq!(m.body, "the & snippet");
        assert!(m.attachments.is_empty());

        let p = multipart(
            "multipart/mixed",
            vec![leaf("1", "text/plain", vec![], b"intro\r\n"), big],
        );
        let m = parse_message(&msg(Some(p)));
        assert_eq!(m.body, "intro");
        assert_eq!(m.body_source, BodySource::Plain);
        assert!(m.body_truncated);
    }

    #[test]
    fn nested_mixed_with_attachment_part() {
        let mut att = leaf(
            "1",
            "application/pdf",
            vec![hdr("Content-Disposition", "attachment; filename=\"a.pdf\"")],
            b"",
        );
        att.filename = "a.pdf".into();
        att.body = Some(BodyDto {
            attachment_id: Some("ATT1".into()),
            size: 1234,
            data: None,
        });
        let p = multipart(
            "multipart/mixed",
            vec![
                multipart(
                    "multipart/alternative",
                    vec![
                        leaf("0.0", "text/plain", vec![], b"see attached"),
                        leaf("0.1", "text/html", vec![], b"<p>see attached</p>"),
                    ],
                ),
                att,
            ],
        );
        let m = parse_message(&msg(Some(p)));
        assert_eq!(m.body, "see attached");
        assert_eq!(
            m.attachments,
            vec![Attachment {
                part_id: "1".into(),
                attachment_id: Some("ATT1".into()),
                filename: "a.pdf".into(),
                mime_type: "application/pdf".into(),
                size: 1234,
            }]
        );
    }

    #[test]
    fn attachment_with_inline_data_and_no_attachment_id() {
        let mut att = leaf("1", "text/plain", vec![], b"notes in a file");
        att.filename = "notes.txt".into();
        let p = multipart(
            "multipart/mixed",
            vec![leaf("0", "text/plain", vec![], b"body"), att],
        );
        let m = parse_message(&msg(Some(p)));
        assert_eq!(m.body, "body");
        assert_eq!(m.attachments.len(), 1);
        assert_eq!(m.attachments[0].filename, "notes.txt");
        assert_eq!(m.attachments[0].attachment_id, None);
        assert_eq!(m.attachments[0].size, 15);
    }

    #[test]
    fn iso_8859_1_and_windows_1252_charsets() {
        let latin1 = leaf(
            "",
            "text/plain",
            vec![hdr("Content-Type", "text/plain; charset=\"ISO-8859-1\"")],
            b"caf\xe9 cr\xe8me",
        );
        assert_eq!(parse_message(&msg(Some(latin1))).body, "café crème");
        let cp1252 = leaf(
            "",
            "text/plain",
            vec![hdr(
                "Content-Type",
                "text/plain; format=flowed; charset=windows-1252",
            )],
            b"\x93quoted\x94 \x80 5",
        );
        assert_eq!(parse_message(&msg(Some(cp1252))).body, "“quoted” € 5");
        // Already-UTF-8 bytes win over a stale charset label.
        let utf8 = leaf(
            "",
            "text/plain",
            vec![hdr("Content-Type", "text/plain; charset=iso-8859-1")],
            "café".as_bytes(),
        );
        assert_eq!(parse_message(&msg(Some(utf8))).body, "café");
        // Unknown charset falls back to windows-1252.
        assert_eq!(decode_text(b"\xe9", Some("x-unknown")), "é");
    }

    #[test]
    fn header_names_are_case_insensitive_and_encoded_words_decode() {
        let p = with_headers(
            leaf("", "text/plain", vec![], b"hi"),
            vec![
                hdr("FROM", "=?utf-8?B?Wm/DqyDDhW5nc3Ryw7Zt?= <zoe@example.org>"),
                hdr("subject", "=?utf-8?B?Q2Fmw6kg4piVIHBsYW5z?="),
                hdr("tO", "a@example.com, \"B, C\" <bc@example.com>"),
                hdr("Message-Id", "<x@y>"),
                hdr("references", "<a@y>\r\n <b@y>"),
            ],
        );
        let m = parse_message(&msg(Some(p)));
        assert_eq!(
            m.from,
            Address::new(Some("Zoë Ångström"), "zoe@example.org")
        );
        assert_eq!(m.subject, "Café ☕ plans");
        assert_eq!(m.to.len(), 2);
        assert_eq!(m.to[1].name.as_deref(), Some("B, C"));
        assert_eq!(m.message_id.as_deref(), Some("<x@y>"));
        assert_eq!(m.references, vec!["<a@y>", "<b@y>"]);
        assert_eq!(decode_header("plain =? text"), "plain =? text");
    }

    #[test]
    fn missing_payload_yields_empty_message() {
        let m = parse_message(&msg(None));
        assert_eq!(m.body, "");
        assert_eq!(m.body_source, BodySource::Empty);
        assert_eq!(m.from.email, "");
        assert!(m.attachments.is_empty());
        assert_eq!(m.snippet, "the & snippet");
        assert_eq!(m.date.as_millisecond(), 1_700_000_000_000);
    }

    #[test]
    fn date_falls_back_to_the_date_header() {
        let mut d = msg(Some(with_headers(
            PartDto::default(),
            vec![hdr("Date", "Tue, 14 Nov 2023 22:13:20 +0000")],
        )));
        d.internal_date = None;
        assert_eq!(parse_message(&d).date.as_second(), 1_700_000_000);
        d.payload = None;
        assert_eq!(parse_message(&d).date, Timestamp::UNIX_EPOCH);
    }

    #[test]
    fn internal_date_parsing() {
        assert_eq!(
            internal_date(Some("1700003600000")).unwrap().as_second(),
            1_700_003_600
        );
        assert_eq!(internal_date(Some("nope")), None);
        assert_eq!(internal_date(None), None);
    }

    #[test]
    fn invalid_body_data_is_empty_not_a_panic() {
        let mut p = leaf("", "text/plain", vec![], b"");
        p.body = Some(BodyDto {
            attachment_id: None,
            size: 3,
            data: Some("!!!".into()),
        });
        let m = parse_message(&msg(Some(p)));
        assert_eq!(m.body_source, BodySource::Empty);
    }

    #[test]
    fn summary_from_metadata_fixture() {
        let dto: ThreadDto = serde_json::from_str(fixtures::THREAD_META).unwrap();
        let s = thread_summary(&"you@example.com".to_string(), &dto).unwrap();
        assert_eq!(s.key.to_string(), "you@example.com/18c0a1b2c3d4e5f6");
        assert_eq!(s.history_id, 123450);
        assert_eq!(s.subject, "Café ☕ plans");
        assert_eq!(s.snippet, "Don't forget the plans & tickets");
        assert_eq!(s.message_count, 2);
        assert_eq!(s.last_date.as_millisecond(), 1_700_003_600_000);
        assert!(s.unread() && s.starred() && s.in_label("INBOX") && s.in_label("IMPORTANT"));
        assert_eq!(
            s.participants,
            vec![
                (Address::new(Some("Zoë Ångström"), "zoe@example.org"), false),
                (Address::new(Some("Bob Jones"), "bob@example.net"), true),
            ]
        );
        // multipart/mixed payload in metadata format.
        assert!(s.has_attachments);
    }

    #[test]
    fn summary_without_attachments_and_empty_thread() {
        let mut dto: ThreadDto = serde_json::from_str(fixtures::THREAD_META).unwrap();
        dto.messages.truncate(1);
        let s = thread_summary(&"you@example.com".to_string(), &dto).unwrap();
        assert!(!s.has_attachments && !s.unread() && !s.starred());
        assert_eq!(s.message_count, 1);
        dto.messages.clear();
        assert!(thread_summary(&"you@example.com".to_string(), &dto).is_none());
    }

    #[test]
    fn detail_from_full_fixture() {
        let dto: ThreadDto = serde_json::from_str(fixtures::THREAD_FULL).unwrap();
        let d = thread_detail(&"you@example.com".to_string(), &dto);
        assert_eq!(d.messages.len(), 2);
        assert_eq!(d.subject, "Café ☕ plans");
        let first = &d.messages[0];
        assert_eq!(first.body, "Shall we meet at the café?");
        assert_eq!(first.body_source, BodySource::Plain);
        assert_eq!(first.snippet, "Shall we meet at the café?");
        let second = &d.messages[1];
        assert_eq!(
            second.body,
            "Don't forget the plans & tickets.\n\n> Shall we meet at the café?"
        );
        assert_eq!(second.from.email, "bob@example.net");
        assert_eq!(
            second.reply_to,
            vec![Address::new(Some("Bob Work"), "bob@work.example")]
        );
        assert_eq!(
            second.cc,
            vec![Address::new(Some("Carol"), "carol@example.com")]
        );
        assert_eq!(second.in_reply_to.as_deref(), Some("<abc123@example.org>"));
        assert_eq!(second.references, vec!["<abc123@example.org>"]);
        assert_eq!(second.attachments.len(), 1);
        assert_eq!(second.attachments[0].filename, "tickets.pdf");
        assert_eq!(
            second.attachments[0].attachment_id.as_deref(),
            Some("ANGjdJ8abcdef")
        );
        let s = thread_summary(&"you@example.com".to_string(), &dto).unwrap();
        assert!(s.has_attachments);
    }

    #[test]
    fn html_fixture_body_converts() {
        let dto: ThreadDto = serde_json::from_str(fixtures::THREAD_FULL).unwrap();
        let html = &dto.messages[0].payload.as_ref().unwrap().parts[1];
        let text = part_text(html).unwrap();
        let c = html_to_text(&text).unwrap();
        assert_eq!(
            c.text,
            "Shall we meet at the café?\n\nMap <https://maps.example/cafe>"
        );
    }

    #[test]
    fn labels_from_fixtures() {
        let list: LabelList = serde_json::from_str(fixtures::LABELS).unwrap();
        let acct = "you@example.com".to_string();
        let all: Vec<Label> = list.labels.iter().map(|l| label(&acct, l)).collect();
        let receipts = all.iter().find(|l| l.id == "Label_1").unwrap();
        assert_eq!(receipts.kind, LabelKind::User);
        assert_eq!(receipts.color, Some((0xffffff, 0x16a766)));
        assert!(!receipts.hidden);
        assert!(all.iter().find(|l| l.id == "Label_3").unwrap().hidden);
        let inbox = all.iter().find(|l| l.id == "INBOX").unwrap();
        assert_eq!(inbox.kind, LabelKind::System);
        assert_eq!(inbox.threads_unread, 0);
        let counted: LabelDto = serde_json::from_str(fixtures::LABEL_INBOX).unwrap();
        let inbox = label(&acct, &counted);
        assert_eq!(
            (
                inbox.threads_unread,
                inbox.threads_total,
                inbox.messages_total
            ),
            (3, 42, 57)
        );
        assert_eq!(parse_color(Some("#12345")), None);
        assert_eq!(parse_color(Some("123456")), None);
    }
}
