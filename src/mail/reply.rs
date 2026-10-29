//! Pure builders for the composer's prefill of a reply, reply-all or forward (SPEC 10):
//! recipients, subject prefix, quoting and the threading headers.
//!
//! The target is the thread's last message that is not a draft. A reply goes to its
//! `Reply-To` (else `From`), or back to its recipients when I sent it, as Gmail does. Reply-all
//! adds the target's To and Cc, drops my own address and never lists anyone twice. The date in
//! the attribution line comes from a caller-supplied formatter, so this module stays free of
//! time zones and preferences.

use jiff::Timestamp;

use crate::mail::address::{format_address, format_list};
use crate::model::{
    Address, AttachmentSource, ComposeKind, Message, Outgoing, OutgoingAttachment, ThreadDetail,
    labels,
};

/// How many References a reply keeps (the most recent ones).
const MAX_REFERENCES: usize = 20;

/// The message a reply or forward answers: the last one that is not a draft, else the last.
pub fn target(detail: &ThreadDetail) -> Option<&Message> {
    detail
        .messages
        .iter()
        .rev()
        .find(|m| !m.label_ids.contains(labels::DRAFT))
        .or_else(|| detail.messages.last())
}

fn same_email(a: &Address, b: &Address) -> bool {
    a.email.trim().eq_ignore_ascii_case(b.email.trim())
}

/// Drops `me` and repeats (case-insensitive by email), keeping the first of each.
fn dedupe(
    list: impl IntoIterator<Item = Address>,
    me: &Address,
    seen: &mut Vec<Address>,
) -> Vec<Address> {
    let mut out = Vec::new();
    for a in list {
        if a.email.trim().is_empty() || same_email(&a, me) || seen.iter().any(|s| same_email(s, &a))
        {
            continue;
        }
        seen.push(a.clone());
        out.push(a);
    }
    out
}

/// `Re: ` unless the subject already starts with `re:` (any case, after trimming).
pub fn reply_subject(subject: &str) -> String {
    let s = super::one_line(subject);
    let s = s.trim();
    if s.get(..3).is_some_and(|p| p.eq_ignore_ascii_case("re:")) {
        s.to_string()
    } else {
        format!("Re: {s}")
    }
}

/// `Fwd: ` unless the subject already starts with `fwd:` or `fw:`.
pub fn forward_subject(subject: &str) -> String {
    let s = super::one_line(subject);
    let s = s.trim();
    let starts = |p: &str| s.get(..p.len()).is_some_and(|h| h.eq_ignore_ascii_case(p));
    if starts("fwd:") || starts("fw:") {
        s.to_string()
    } else {
        format!("Fwd: {s}")
    }
}

/// The target's References plus its Message-ID, without repeats, keeping the last 20.
pub fn references(target: &Message) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for id in target.references.iter().chain(target.message_id.iter()) {
        let id = id.trim();
        if !id.is_empty() && !out.iter().any(|o| o == id) {
            out.push(id.to_string());
        }
    }
    let skip = out.len().saturating_sub(MAX_REFERENCES);
    out.split_off(skip)
}

/// The text a reply quotes or a forward carries: the body, else the snippet.
fn body_of(m: &Message) -> &str {
    if m.body.trim().is_empty() {
        &m.snippet
    } else {
        &m.body
    }
}

/// `Name <a@b>`, or `<a@b>` without a name, for the attribution line.
fn who(a: &Address) -> String {
    match a.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        Some(n) => format!("{n} <{}>", a.email),
        None => format!("<{}>", a.email),
    }
}

/// Each line prefixed with `> `, or `>` when empty.
pub fn quote(body: &str) -> String {
    body.replace("\r\n", "\n")
        .split('\n')
        .map(|l| {
            if l.is_empty() {
                ">".to_string()
            } else {
                format!("> {l}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The prefill for a reply, reply-all or forward of `detail`, sent as `me` from the thread's
/// account. `date` formats a message date for the attribution and forward header. `None` for
/// `ComposeKind::New` or a thread without messages.
pub fn prefill(
    detail: &ThreadDetail,
    kind: ComposeKind,
    me: &Address,
    date: &dyn Fn(Timestamp) -> String,
) -> Option<Outgoing> {
    let t = target(detail)?;
    let subject_src = if t.subject.trim().is_empty() {
        &detail.subject
    } else {
        &t.subject
    };
    let from_me = same_email(&t.from, me);
    let reply_to = if from_me {
        t.to.clone()
    } else if !t.reply_to.is_empty() {
        t.reply_to.clone()
    } else {
        vec![t.from.clone()]
    };
    let (to, cc, subject, body, attachments) = match kind {
        ComposeKind::New => return None,
        ComposeKind::Reply => (
            reply_to,
            Vec::new(),
            reply_subject(subject_src),
            reply_body(t, date),
            Vec::new(),
        ),
        ComposeKind::ReplyAll => {
            let mut seen = Vec::new();
            let to = dedupe(
                reply_to.into_iter().chain(t.to.iter().cloned()),
                me,
                &mut seen,
            );
            let cc = dedupe(t.cc.iter().cloned(), me, &mut seen);
            (
                to,
                cc,
                reply_subject(subject_src),
                reply_body(t, date),
                Vec::new(),
            )
        }
        ComposeKind::Forward => (
            Vec::new(),
            Vec::new(),
            forward_subject(subject_src),
            forward_body(t, subject_src, date),
            t.attachments
                .iter()
                .map(|a| OutgoingAttachment {
                    filename: a.filename.clone(),
                    mime_type: a.mime_type.clone(),
                    size: a.size,
                    source: AttachmentSource::Gmail {
                        message_id: t.id.clone(),
                        part_id: a.part_id.clone(),
                    },
                })
                .collect(),
        ),
    };
    Some(Outgoing {
        account: detail.key.account.clone(),
        kind,
        from: me.clone(),
        to,
        cc,
        bcc: Vec::new(),
        subject,
        body,
        thread_id: Some(detail.key.id.clone()),
        in_reply_to: t.message_id.clone(),
        references: references(t),
        attachments,
    })
}

fn reply_body(t: &Message, date: &dyn Fn(Timestamp) -> String) -> String {
    format!(
        "\n\nOn {}, {} wrote:\n{}",
        date(t.date),
        who(&t.from),
        quote(body_of(t))
    )
}

fn forward_body(t: &Message, subject: &str, date: &dyn Fn(Timestamp) -> String) -> String {
    let mut s = String::from("\n\n---------- Forwarded message ---------\n");
    s.push_str(&format!("From: {}\n", format_address(&t.from)));
    s.push_str(&format!("Date: {}\n", date(t.date)));
    s.push_str(&format!("Subject: {subject}\n"));
    s.push_str(&format!("To: {}\n", format_list(&t.to)));
    if !t.cc.is_empty() {
        s.push_str(&format!("Cc: {}\n", format_list(&t.cc)));
    }
    s.push('\n');
    s.push_str(body_of(t));
    s
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::model::{Attachment, BodySource, ThreadKey};

    const ME: &str = "you@example.com";

    fn addr(name: &str, email: &str) -> Address {
        Address::new((!name.is_empty()).then_some(name), email)
    }

    fn me() -> Address {
        addr("", ME)
    }

    fn msg(id: &str, from: Address, to: Vec<Address>, cc: Vec<Address>) -> Message {
        Message {
            id: id.into(),
            thread_id: "t1".into(),
            label_ids: BTreeSet::new(),
            from,
            to,
            cc,
            bcc: vec![],
            reply_to: vec![],
            date: Timestamp::UNIX_EPOCH,
            subject: "Lunch".into(),
            message_id: Some(format!("<{id}@mail.example>")),
            in_reply_to: None,
            references: vec![],
            snippet: "snippet".into(),
            body: "Shall we?\n\nNoon works.".into(),
            body_source: BodySource::Plain,
            body_truncated: false,
            attachments: vec![],
        }
    }

    fn thread(messages: Vec<Message>) -> ThreadDetail {
        ThreadDetail {
            key: ThreadKey::new(ME, "t1"),
            subject: "Lunch".into(),
            label_ids: BTreeSet::new(),
            messages,
        }
    }

    fn date(_: Timestamp) -> String {
        "Thu 1 Jan 1970 00:00".into()
    }

    fn emails(list: &[Address]) -> Vec<&str> {
        list.iter().map(|a| a.email.as_str()).collect()
    }

    #[test]
    fn reply_to_someone_else() {
        let zoe = addr("Zoë Ångström", "zoe@example.org");
        let d = thread(vec![msg("m1", zoe.clone(), vec![me()], vec![])]);
        let out = prefill(&d, ComposeKind::Reply, &me(), &date).unwrap();
        assert_eq!(out.to, vec![zoe]);
        assert!(out.cc.is_empty() && out.bcc.is_empty());
        assert_eq!(out.subject, "Re: Lunch");
        assert_eq!(out.account, ME);
        assert_eq!(out.thread_id.as_deref(), Some("t1"));
        assert_eq!(out.in_reply_to.as_deref(), Some("<m1@mail.example>"));
        assert_eq!(out.references, vec!["<m1@mail.example>".to_string()]);
        assert!(out.attachments.is_empty());

        // Reply-To wins over From.
        let mut m = msg("m1", addr("List", "list@example.org"), vec![me()], vec![]);
        m.reply_to = vec![addr("", "replies@example.org")];
        let out = prefill(&thread(vec![m]), ComposeKind::Reply, &me(), &date).unwrap();
        assert_eq!(emails(&out.to), ["replies@example.org"]);
    }

    #[test]
    fn reply_to_my_own_message_goes_to_its_recipients() {
        let bob = addr("Bob", "bob@example.org");
        let d = thread(vec![
            msg("m1", bob.clone(), vec![me()], vec![]),
            msg(
                "m2",
                addr("Me", "YOU@example.com"),
                vec![bob.clone()],
                vec![],
            ),
        ]);
        let out = prefill(&d, ComposeKind::Reply, &me(), &date).unwrap();
        assert_eq!(out.to, vec![bob]);
        assert_eq!(out.in_reply_to.as_deref(), Some("<m2@mail.example>"));
    }

    #[test]
    fn drafts_are_not_the_target() {
        let mut draft = msg("m2", me(), vec![], vec![]);
        draft.label_ids.insert(labels::DRAFT.into());
        let d = thread(vec![
            msg("m1", addr("", "a@example.org"), vec![me()], vec![]),
            draft,
        ]);
        assert_eq!(target(&d).unwrap().id, "m1");
    }

    #[test]
    fn reply_all_dedupes_and_removes_me() {
        let alice = addr("Alice", "alice@example.org");
        let bob = addr("Bob", "bob@example.org");
        let carol = addr("Carol", "carol@example.org");
        let m = msg(
            "m1",
            alice.clone(),
            vec![me(), bob.clone(), addr("Alice again", "ALICE@example.org")],
            vec![carol.clone(), addr("", "Bob@Example.org"), addr("", ME)],
        );
        let out = prefill(&thread(vec![m]), ComposeKind::ReplyAll, &me(), &date).unwrap();
        assert_eq!(out.to, vec![alice, bob]);
        assert_eq!(out.cc, vec![carol]);
    }

    #[test]
    fn subject_prefixes_are_idempotent() {
        assert_eq!(reply_subject("Lunch"), "Re: Lunch");
        assert_eq!(reply_subject("  RE: Lunch "), "RE: Lunch");
        assert_eq!(reply_subject("re:Lunch"), "re:Lunch");
        assert_eq!(reply_subject(""), "Re: ");
        assert_eq!(forward_subject("Lunch"), "Fwd: Lunch");
        assert_eq!(forward_subject("FWD: Lunch"), "FWD: Lunch");
        assert_eq!(forward_subject("Fw: Lunch"), "Fw: Lunch");
        assert_eq!(forward_subject("Re: Lunch"), "Fwd: Re: Lunch");
        // A decoded header can carry line breaks; the subject stays on one line.
        assert_eq!(reply_subject("Lunch\r\non Friday"), "Re: Lunch  on Friday");
        assert_eq!(forward_subject("Lunch\nplans"), "Fwd: Lunch plans");
        // Non-ASCII first characters do not split a char.
        assert_eq!(reply_subject("Ré"), "Re: Ré");
    }

    #[test]
    fn references_chain_dedupes_and_keeps_the_last_twenty() {
        let mut m = msg("m30", addr("", "a@example.org"), vec![], vec![]);
        m.references = (0..30).map(|i| format!("<m{i}@mail.example>")).collect();
        m.references.push("<m3@mail.example>".into());
        let refs = references(&m);
        assert_eq!(refs.len(), 20);
        assert_eq!(refs.first().map(String::as_str), Some("<m11@mail.example>"));
        // Its own id was already in the chain (m30 is not), so it comes last.
        assert_eq!(refs.last().map(String::as_str), Some("<m30@mail.example>"));
        let mut seen = BTreeSet::new();
        assert!(refs.iter().all(|r| seen.insert(r.clone())));
    }

    #[test]
    fn quoting_prefixes_every_line() {
        assert_eq!(quote("a\n\nb\r\nc"), "> a\n>\n> b\n> c");
        let d = thread(vec![msg(
            "m1",
            addr("Zoë", "zoe@example.org"),
            vec![me()],
            vec![],
        )]);
        let out = prefill(&d, ComposeKind::Reply, &me(), &date).unwrap();
        assert_eq!(
            out.body,
            "\n\nOn Thu 1 Jan 1970 00:00, Zoë <zoe@example.org> wrote:\n> Shall we?\n>\n> Noon works."
        );
    }

    #[test]
    fn forward_carries_a_header_block_and_attachments() {
        let mut m = msg(
            "m1",
            addr("Zoë", "zoe@example.org"),
            vec![me()],
            vec![addr("", "cc@example.org")],
        );
        m.attachments = vec![Attachment {
            part_id: "2".into(),
            attachment_id: Some("att".into()),
            filename: "plan.pdf".into(),
            mime_type: "application/pdf".into(),
            size: 1234,
        }];
        let out = prefill(&thread(vec![m]), ComposeKind::Forward, &me(), &date).unwrap();
        assert!(out.to.is_empty() && out.cc.is_empty());
        assert_eq!(out.subject, "Fwd: Lunch");
        assert_eq!(
            out.body,
            "\n\n---------- Forwarded message ---------\nFrom: Zoë <zoe@example.org>\n\
             Date: Thu 1 Jan 1970 00:00\nSubject: Lunch\nTo: you@example.com\n\
             Cc: cc@example.org\n\nShall we?\n\nNoon works."
        );
        assert_eq!(out.attachments.len(), 1);
        let a = &out.attachments[0];
        assert_eq!((a.filename.as_str(), a.size), ("plan.pdf", 1234));
        assert!(matches!(
            &a.source,
            AttachmentSource::Gmail { message_id, part_id } if message_id == "m1" && part_id == "2"
        ));
        assert_eq!(out.thread_id.as_deref(), Some("t1"));
    }

    #[test]
    fn new_and_empty_threads_have_no_prefill() {
        let d = thread(vec![msg("m1", addr("", "a@example.org"), vec![], vec![])]);
        assert!(prefill(&d, ComposeKind::New, &me(), &date).is_none());
        assert!(prefill(&thread(vec![]), ComposeKind::Reply, &me(), &date).is_none());
    }
}
