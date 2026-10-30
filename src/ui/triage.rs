//! AI triage in the reader and the list (SPEC 16.12): the pure reasons, local caution and link
//! guard decision, and the render helpers for the banner, the caution row, the failure caption
//! and the list badge.
//!
//! The reasons are Eloquawk's own observations from what the client measured, never Jev's (it
//! returns no explanations), and every host is the ASCII/punycode form, so a homograph cannot
//! pass for the real name.

use crate::mail::links::LinkTarget;
use crate::model::{AuthResult, Message, MessageSignals};

/// At most this many reasons are shown.
const MAX_REASONS: usize = 3;
/// Attachment types worth a reason.
const RISKY_EXTENSIONS: [&str; 7] = ["html", "htm", "exe", "js", "scr", "iso", "zip"];

fn failed(r: AuthResult) -> bool {
    matches!(r, AuthResult::Fail | AuthResult::SoftFail)
}

/// "Sender failed DMARC", or SPF / DKIM when DMARC is unknown.
fn auth_reason(s: &MessageSignals) -> Option<String> {
    let a = &s.auth;
    if a.dmarc == AuthResult::Fail {
        return Some("Sender failed DMARC".into());
    }
    if a.dmarc == AuthResult::Unknown {
        if failed(a.spf) {
            return Some("Sender failed SPF".into());
        }
        if failed(a.dkim) {
            return Some("Sender failed DKIM".into());
        }
    }
    None
}

/// "Link text shows <text host> but goes to <host>", for the first mismatched link.
fn link_reason(s: &MessageSignals) -> Option<String> {
    let l = s.links.iter().find(|l| l.mismatched())?;
    Some(format!(
        "Link text shows {} but goes to {}",
        l.text_host.as_deref().unwrap_or_default(),
        l.host
    ))
}

fn domain(email: &str) -> String {
    email
        .rsplit_once('@')
        .map(|(_, d)| d.trim().to_ascii_lowercase())
        .unwrap_or_default()
}

/// The reasons line under a banner (SPEC 16.12.1), built locally: at most three of the
/// authentication failure, the first mismatched link, a Reply-To in another domain and a
/// risky attachment type. M16 note: it takes the whole message, because Reply-To and the
/// attachments are not part of `MessageSignals`.
#[allow(dead_code)] // consumed in M16 by the reader (banner and link guard)
pub fn reasons(m: &Message) -> Vec<String> {
    let s = &m.signals;
    let from = domain(&m.from.email);
    let reply_to = m
        .reply_to
        .iter()
        .any(|a| {
            let d = domain(&a.email);
            !d.is_empty() && d != from
        })
        .then(|| "Reply-To differs from the sender's domain".to_string());
    let attachment = m.attachments.iter().find_map(|a| {
        let ext = a.filename.rsplit_once('.')?.1.to_ascii_lowercase();
        RISKY_EXTENSIONS
            .contains(&ext.as_str())
            .then(|| format!("Has an .{ext} attachment"))
    });
    [auth_reason(s), link_reason(s), reply_to, attachment]
        .into_iter()
        .flatten()
        .take(MAX_REASONS)
        .collect()
}

/// The local caution (SPEC 16.12.1): `Some(reasons)` when DMARC failed **and** a link's text
/// names another host. Needs no request, no key and no opt-in.
pub fn local_caution(s: &MessageSignals) -> Option<Vec<String>> {
    if s.auth.dmarc != AuthResult::Fail {
        return None;
    }
    let link = link_reason(s)?;
    Some(vec!["Sender failed DMARC".into(), link])
}

/// What a click on a link does under the link guard.
#[allow(dead_code)] // consumed in M16 by the reader (banner and link guard)
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GuardDecision {
    Open,
    /// Ask first: the notice text, and the URL its "Open link" button opens.
    Notice {
        text: String,
        url: String,
    },
}

/// Whether a click opens at once or asks first (SPEC 16.12.1). Mailto links always open the
/// composer; web links ask while `guarded`, naming the punycode host.
#[allow(dead_code)] // consumed in M16 by the reader (banner and link guard)
pub fn guard_decision(target: &LinkTarget, guarded: bool) -> GuardDecision {
    match target {
        LinkTarget::Mail(_) => GuardDecision::Open,
        LinkTarget::Web(_) if !guarded => GuardDecision::Open,
        LinkTarget::Web(url) => {
            let host = url::Url::parse(url)
                .ok()
                .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
                .unwrap_or_else(|| "this link".into());
            GuardDecision::Notice {
                text: format!("This message looks like phishing. Open {host}?"),
                url: url.clone(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::model::{Address, Attachment, AuthResults, BodySource, LinkSignal};

    fn signals(dmarc: AuthResult, links: Vec<LinkSignal>) -> MessageSignals {
        MessageSignals {
            auth: AuthResults {
                dmarc,
                ..AuthResults::default()
            },
            links,
            ..MessageSignals::default()
        }
    }

    fn link(text_host: &str, host: &str) -> LinkSignal {
        LinkSignal {
            text: text_host.into(),
            host: host.into(),
            text_host: Some(text_host.into()),
        }
    }

    fn message(s: MessageSignals) -> Message {
        Message {
            id: "m".into(),
            thread_id: "t".into(),
            label_ids: BTreeSet::new(),
            from: Address::new(None, "security@paypa1-verify.example"),
            to: vec![],
            cc: vec![],
            bcc: vec![],
            reply_to: vec![Address::new(None, "help@paypa1-support.example")],
            date: jiff::Timestamp::UNIX_EPOCH,
            subject: String::new(),
            message_id: None,
            in_reply_to: None,
            references: vec![],
            snippet: String::new(),
            body: String::new(),
            body_source: BodySource::Plain,
            body_truncated: false,
            attachments: vec![Attachment {
                part_id: "1".into(),
                attachment_id: None,
                filename: "review.HTML".into(),
                mime_type: "text/html".into(),
                size: 1,
            }],
            signals: s,
        }
    }

    #[test]
    fn reasons_are_local_ordered_and_capped() {
        let m = message(signals(
            AuthResult::Fail,
            vec![link("www.paypal.com", "paypa1-verify.example")],
        ));
        assert_eq!(
            reasons(&m),
            [
                "Sender failed DMARC",
                "Link text shows www.paypal.com but goes to paypa1-verify.example",
                "Reply-To differs from the sender's domain",
            ]
        );
        let mut plain = message(signals(AuthResult::Pass, vec![]));
        plain.reply_to.clear();
        assert_eq!(reasons(&plain), ["Has an .html attachment"]);
        plain.attachments.clear();
        assert!(reasons(&plain).is_empty());
        let mut spf = message(MessageSignals::default());
        spf.signals.auth.spf = AuthResult::SoftFail;
        assert_eq!(reasons(&spf)[0], "Sender failed SPF");
    }

    #[test]
    fn homograph_hosts_stay_punycode() {
        let s = signals(
            AuthResult::Fail,
            vec![link("paypal.com", "xn--l-7sba6dbr.com")],
        );
        let caution = local_caution(&s).unwrap();
        assert_eq!(
            caution[1],
            "Link text shows paypal.com but goes to xn--l-7sba6dbr.com"
        );
        assert!(reasons(&message(s))[1].contains("xn--"));
    }

    #[test]
    fn local_caution_needs_dmarc_fail_and_a_mismatch() {
        let mismatch = || vec![link("www.paypal.com", "paypa1-secure.example")];
        assert!(local_caution(&signals(AuthResult::Fail, mismatch())).is_some());
        assert!(local_caution(&signals(AuthResult::Pass, mismatch())).is_none());
        assert!(local_caution(&signals(AuthResult::Unknown, mismatch())).is_none());
        assert!(
            local_caution(&signals(
                AuthResult::Fail,
                vec![link("www.paypal.com", "paypal.com")]
            ))
            .is_none()
        );
    }

    #[test]
    fn guard_asks_for_web_links_only() {
        let web = LinkTarget::Web("https://paypa1-verify.example/signin?session=1".into());
        assert_eq!(
            guard_decision(&web, true),
            GuardDecision::Notice {
                text: "This message looks like phishing. Open paypa1-verify.example?".into(),
                url: "https://paypa1-verify.example/signin?session=1".into(),
            }
        );
        assert_eq!(guard_decision(&web, false), GuardDecision::Open);
        let idn = LinkTarget::Web("https://раураl.com/login".into());
        let GuardDecision::Notice { text, .. } = guard_decision(&idn, true) else {
            panic!("not guarded")
        };
        assert_eq!(
            text,
            "This message looks like phishing. Open xn--l-7sba6dbr.com?"
        );
        let mail = LinkTarget::Mail("mailto:help@paypa1-support.example".into());
        assert_eq!(guard_decision(&mail, true), GuardDecision::Open);
    }
}
