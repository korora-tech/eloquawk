//! AI triage in the reader and the list (SPEC 16.12): the pure reasons, local caution and link
//! guard decision, and the render helpers for the banner, the caution row, the failure caption
//! and the list badge.
//!
//! The reasons are Eloquawk's own observations from what the client measured, never Jev's (it
//! returns no explanations), and every host is the ASCII/punycode form, so a homograph cannot
//! pass for the real name.

use gpui::prelude::*;
use gpui::{App, Context, Div, FontWeight, SharedString, Window, div, px, rgb};

use super::theme::{Palette, tint};
use super::widgets::{ButtonStyle, OnClick, button};
use crate::ai::jev::{Kind, Level, Verdict};
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

/// The banner's title (SPEC 16.12.1).
pub fn title(v: &Verdict) -> &'static str {
    match (v.level, v.kind) {
        (Level::Danger, Kind::Scam) => "This looks like a scam",
        (Level::Danger, _) => "This looks like phishing",
        (Level::Warning, Kind::Scam) => "This may be a scam",
        (Level::Warning, _) => "This may be phishing",
        _ => "This looks like spam",
    }
}

/// The banner's right-hand text: qualitative, since the thresholds are not calibrated.
pub fn strength(v: &Verdict) -> String {
    let base = match v.level {
        Level::Danger => "Jev: very likely",
        Level::Warning => "Jev: possible",
        _ => "Jev: likely spam",
    };
    let also = match v.kind {
        Kind::Phishing if v.p_scam >= 0.5 => " · also possibly a scam",
        Kind::Scam if v.p_phishing >= 0.5 => " · also possibly phishing",
        _ => "",
    };
    format!("{base}{also}")
}

/// The list badge's text (SPEC 16.12.2).
pub fn badge_text(level: Level, kind: Kind) -> Option<&'static str> {
    match (level, kind) {
        (Level::Danger, Kind::Scam) => Some("Scam"),
        (Level::Danger, _) => Some("Phishing"),
        (Level::Warning, _) => Some("Suspicious"),
        (Level::Spam, _) => Some("Spam"),
        (Level::None, _) => None,
    }
}

/// The level's border colour and text token.
fn colours(p: &Palette, level: Level) -> (u32, u32) {
    if level == Level::Danger {
        (p.red, p.red_text)
    } else {
        (p.amber, p.amber_text)
    }
}

/// A plain text tooltip.
struct Tip(SharedString);

impl Render for Tip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = super::theme::palette(cx);
        div()
            .rounded(px(6.0))
            .border_1()
            .border_color(rgb(p.border))
            .bg(rgb(p.panel_raised))
            .px(px(8.0))
            .py(px(4.0))
            .text_size(px(11.5))
            .text_color(rgb(p.text))
            .child(self.0.clone())
    }
}

/// The reader banner (SPEC 16.12.1). `report` is `None` in the Spam view, which hides it.
pub fn banner(
    p: &Palette,
    v: &Verdict,
    reasons: &[String],
    report: Option<OnClick>,
    dismiss: OnClick,
) -> Div {
    let (border, _) = colours(p, v.level);
    let score = match v.kind {
        Kind::Phishing => v.p_phishing,
        Kind::Scam => v.p_scam,
        Kind::Spam => v.p_spam,
    };
    let tip: SharedString = format!("Model score {score:.2}, not a probability of harm").into();
    let reasons = if reasons.is_empty() {
        "Jev's assessment of the content".to_string()
    } else {
        reasons.join(" \u{b7} ")
    };
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .rounded(px(8.0))
        .border_l(px(3.0))
        .border_color(rgb(border))
        .bg(rgb(tint(border, p.panel, 0.12)))
        .px(px(12.0))
        .py(px(10.0))
        .child(
            div()
                .flex()
                .items_baseline()
                .justify_between()
                .gap(px(10.0))
                .child(
                    div()
                        .text_size(px(14.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(p.text))
                        .child(title(v)),
                )
                .child(
                    div()
                        .id("triage-score")
                        .flex_shrink_0()
                        .text_size(px(12.0))
                        .text_color(rgb(p.secondary))
                        .tooltip(move |_, cx: &mut App| cx.new(|_| Tip(tip.clone())).into())
                        .child(strength(v)),
                ),
        )
        .child(
            div()
                .text_size(px(12.5))
                .line_height(px(18.0))
                .text_color(rgb(p.secondary))
                .child(reasons),
        )
        .child(
            div()
                .flex()
                .gap(px(6.0))
                .when_some(report, |d, on| {
                    d.child(button(
                        p,
                        "triage-report",
                        "Report spam",
                        ButtonStyle::Secondary,
                        Some(on),
                    ))
                })
                .child(button(
                    p,
                    "triage-dismiss",
                    "Dismiss",
                    ButtonStyle::Secondary,
                    Some(dismiss),
                )),
        )
}

/// The local caution row (SPEC 16.12.1): amber, no score, no Jev wording, no Dismiss.
pub fn caution(p: &Palette, reasons: &[String]) -> Div {
    div()
        .w_full()
        .rounded(px(8.0))
        .border_l(px(3.0))
        .border_color(rgb(p.amber))
        .bg(rgb(tint(p.amber, p.panel, 0.12)))
        .px(px(12.0))
        .py(px(8.0))
        .text_size(px(12.5))
        .line_height(px(18.0))
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(p.amber_text))
        .child(format!("Eloquawk noticed: {}", reasons.join(" \u{b7} ")))
}

/// The muted caption after a failed check, with "Retry" when a retry may help, or the pause.
pub fn failure_caption(p: &Palette, text: String, retry: Option<OnClick>) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(4.0))
        .text_size(px(12.0))
        .line_height(px(16.0))
        .text_color(rgb(p.muted))
        .child(text)
        .when_some(retry, |d, on| {
            d.child("\u{b7}").child(
                div()
                    .id("triage-retry")
                    .text_color(rgb(p.link))
                    .underline()
                    .cursor_pointer()
                    .on_click(on)
                    .child("Retry"),
            )
        })
}

/// The thread row's pill (SPEC 16.12.2), on `row_bg`.
pub fn badge(p: &Palette, level: Level, kind: Kind, row_bg: u32) -> Option<Div> {
    let text = badge_text(level, kind)?;
    let (colour, text_colour) = colours(p, level);
    Some(
        div()
            .flex_shrink_0()
            .px(px(6.0))
            .py(px(1.0))
            .rounded(px(4.0))
            .bg(rgb(tint(colour, row_bg, 0.15)))
            .text_size(px(11.0))
            .line_height(px(16.0))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(rgb(text_colour))
            .child(text),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::ai::jev::{THRESHOLDS, verdict_from};
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
    fn titles_strengths_and_badges() {
        let v = |p, s, sp| verdict_from(p, s, sp, &THRESHOLDS);
        assert_eq!(title(&v(0.97, 0.0, 0.0)), "This looks like phishing");
        assert_eq!(title(&v(0.0, 0.91, 0.0)), "This looks like a scam");
        assert_eq!(title(&v(0.62, 0.0, 0.0)), "This may be phishing");
        assert_eq!(title(&v(0.0, 0.6, 0.0)), "This may be a scam");
        assert_eq!(title(&v(0.0, 0.0, 0.9)), "This looks like spam");
        assert_eq!(strength(&v(0.97, 0.0, 0.0)), "Jev: very likely");
        assert_eq!(
            strength(&v(0.97, 0.55, 0.0)),
            "Jev: very likely · also possibly a scam"
        );
        assert_eq!(strength(&v(0.6, 0.0, 0.0)), "Jev: possible");
        assert_eq!(strength(&v(0.0, 0.0, 0.9)), "Jev: likely spam");
        assert_eq!(badge_text(Level::Danger, Kind::Scam), Some("Scam"));
        assert_eq!(badge_text(Level::Danger, Kind::Phishing), Some("Phishing"));
        assert_eq!(badge_text(Level::Warning, Kind::Scam), Some("Suspicious"));
        assert_eq!(badge_text(Level::Spam, Kind::Spam), Some("Spam"));
        assert_eq!(badge_text(Level::None, Kind::Phishing), None);
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
