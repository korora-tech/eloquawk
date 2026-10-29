//! `mailto:` URLs (RFC 6068) parsed into an [`Outgoing`] prefill for the composer.
//!
//! The parser is deliberately strict about what it honours: only `to`, `cc`, `bcc`,
//! `subject` and `body`. Every other hfield (`attach`, `attachment`, `from`, `in-reply-to`,
//! `references` and anything unknown) is ignored, so a link in a message can never attach a
//! local file, spoof the sender or thread the reply into another conversation. The prefill is
//! always a new message with no threading and no attachments, and it is never sent
//! automatically: it only opens the compose window.

use crate::mail::address::parse_list;
use crate::model::{AccountId, Address, ComposeKind, Outgoing};

/// Decodes `%XX` escapes (RFC 3986). `+` stays a plus sign, as RFC 6068 requires; invalid
/// escapes are kept as written and invalid UTF-8 is replaced.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hex) = bytes.get(i + 1..i + 3)
            && let Ok(hex) = std::str::from_utf8(hex)
            && let Ok(b) = u8::from_str_radix(hex, 16)
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parses `url` into a new-message prefill sent from `account` as `from`. `None` when `url`
/// is not a `mailto:` URL.
pub fn parse(url: &str, account: &AccountId, from: Address) -> Option<Outgoing> {
    let scheme = url.get(..7)?;
    if !scheme.eq_ignore_ascii_case("mailto:") {
        return None;
    }
    let rest = &url[7..];
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    let mut out = Outgoing {
        account: account.clone(),
        kind: ComposeKind::New,
        from,
        to: parse_list(&percent_decode(path)),
        cc: vec![],
        bcc: vec![],
        subject: String::new(),
        body: String::new(),
        thread_id: None,
        in_reply_to: None,
        references: vec![],
        attachments: vec![],
    };
    // A fragment is not part of the hfields.
    let query = query.split_once('#').map_or(query, |(q, _)| q);
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        let name = percent_decode(name).to_ascii_lowercase();
        let value = percent_decode(value);
        match name.as_str() {
            "to" => out.to.extend(parse_list(&value)),
            "cc" => out.cc.extend(parse_list(&value)),
            "bcc" => out.bcc.extend(parse_list(&value)),
            "subject" => out.subject = super::one_line(&value),
            "body" => out.body = value.replace("\r\n", "\n"),
            // attach, attachment, from, in-reply-to, references and anything unknown.
            _ => {}
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn me() -> Address {
        Address::new(None, "you@example.com")
    }

    fn go(url: &str) -> Outgoing {
        parse(url, &"you@example.com".to_string(), me()).expect("a mailto URL")
    }

    fn emails(list: &[Address]) -> Vec<&str> {
        list.iter().map(|a| a.email.as_str()).collect()
    }

    #[test]
    fn every_whitelisted_field() {
        let out = go(
            "mailto:ann@example.com?cc=bob@example.com&bcc=cleo@example.com\
                      &subject=Lunch&body=Are%20you%20free%3F",
        );
        assert_eq!(emails(&out.to), ["ann@example.com"]);
        assert_eq!(emails(&out.cc), ["bob@example.com"]);
        assert_eq!(emails(&out.bcc), ["cleo@example.com"]);
        assert_eq!(out.subject, "Lunch");
        assert_eq!(out.body, "Are you free?");
        assert_eq!(out.account, "you@example.com");
        assert_eq!(out.from, me());
    }

    #[test]
    fn subject_line_breaks_become_spaces() {
        let out = go("mailto:a@example.com?subject=x%0Ay%0D%0Az&body=one%0D%0Atwo");
        assert_eq!(out.subject, "x y  z");
        // The body keeps its line breaks.
        assert_eq!(out.body, "one\ntwo");
    }

    #[test]
    fn percent_encoding_and_utf8() {
        let out = go(
            "MAILTO:zo%C3%AB@example.com?subject=Caf%C3%A9%20%E2%98%95%20plans\
                      &body=line%20one%0D%0Aline+two%ZZ",
        );
        assert_eq!(emails(&out.to), ["zoë@example.com"]);
        assert_eq!(out.subject, "Café ☕ plans");
        // CRLF becomes LF; `+` stays a plus; a broken escape is kept as written.
        assert_eq!(out.body, "line one\nline+two%ZZ");
    }

    #[test]
    fn multiple_recipients() {
        let out = go("mailto:a@example.com,b@example.com?to=c@example.com\
                      &cc=d@example.com%2C%22Doe%2C%20E%22%20%3Ce@example.com%3E");
        assert_eq!(
            emails(&out.to),
            ["a@example.com", "b@example.com", "c@example.com"]
        );
        assert_eq!(emails(&out.cc), ["d@example.com", "e@example.com"]);
        assert_eq!(out.cc[1].name.as_deref(), Some("Doe, E"));
    }

    #[test]
    fn dangerous_hfields_are_ignored() {
        let out = go(
            "mailto:a@example.com?attach=/etc/passwd&attachment=file:///etc/shadow\
                      &from=boss@example.com&in-reply-to=%3Cx@y%3E&references=%3Cx@y%3E\
                      &X-Custom=1&subject=Hi",
        );
        assert_eq!(out.from, me());
        assert_eq!(out.kind, ComposeKind::New);
        assert!(out.thread_id.is_none());
        assert!(out.in_reply_to.is_none());
        assert!(out.references.is_empty());
        assert!(out.attachments.is_empty());
        assert_eq!(out.subject, "Hi");
        assert_eq!(emails(&out.to), ["a@example.com"]);
    }

    #[test]
    fn empty_and_foreign_urls() {
        let out = go("mailto:?subject=No%20one");
        assert!(out.to.is_empty());
        assert_eq!(out.subject, "No one");
        let me = me();
        let account = "you@example.com".to_string();
        assert!(parse("https://example.com", &account, me.clone()).is_none());
        assert!(parse("mail", &account, me.clone()).is_none());
        assert!(parse("", &account, me).is_none());
    }
}
