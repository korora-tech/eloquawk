//! Sender authentication and link signals (SPEC 16.6.1, 16.6.2): what the mail client itself
//! measured about a message, as opposed to what its sender wrote.
//!
//! A sender can add its own `Authentication-Results` headers below the one the receiving
//! server writes, so only the first header counts, and only when Gmail wrote it
//! (`mx.google.com`). Hosts are always the lowercase ASCII (punycode) form, so a homograph
//! cannot pass for the real name. Pure, and it never panics on hostile input.

use std::collections::HashSet;

use crate::google::gmail::Header;
use crate::model::{AuthResult, AuthResults, LinkSignal, MessageSignals};

use super::links::{LinkTarget, find_links};
use super::parse::header;

/// The authserv-id of Gmail's own `Authentication-Results` header.
pub const GMAIL_AUTHSERV_ID: &str = "mx.google.com";
/// The most links a message's signals keep.
pub const MAX_LINKS: usize = 40;
/// The most DKIM signing domains kept.
pub const MAX_DKIM_DOMAINS: usize = 5;

/// The signals of a message from its top-level headers and its body's links, each a
/// `(display text, href)` pair.
pub fn signals(headers: &[Header], body_links: &[(String, String)]) -> MessageSignals {
    let auth = header(headers, "Authentication-Results")
        .and_then(gmail_results)
        .unwrap_or_else(|| AuthResults {
            spf: header(headers, "Received-SPF")
                .map(received_spf)
                .unwrap_or_default(),
            ..AuthResults::default()
        });
    MessageSignals {
        auth,
        return_path_domain: header(headers, "Return-Path").and_then(address_domain),
        list_unsubscribe: header(headers, "List-Unsubscribe").is_some(),
        links: link_signals(body_links),
    }
}

/// Removes `( … )` comments, which may nest, so a comment's words are never read as results.
fn strip_comments(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut depth = 0usize;
    for c in value.chars() {
        match c {
            '(' => depth += 1,
            ')' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

/// Parses an `Authentication-Results` value, when Gmail wrote it.
fn gmail_results(value: &str) -> Option<AuthResults> {
    let value = strip_comments(value);
    let mut parts = value.split(';');
    let authserv = parts.next()?.split_whitespace().next()?;
    if !authserv.eq_ignore_ascii_case(GMAIL_AUTHSERV_ID) {
        return None;
    }
    let mut out = AuthResults::default();
    let mut dkim_seen = false;
    for part in parts {
        let mut tokens = part.split_whitespace();
        let Some((method, result)) = tokens.next().and_then(|t| t.split_once('=')) else {
            continue;
        };
        let result = AuthResult::parse(result);
        match method.to_ascii_lowercase().as_str() {
            "spf" => out.spf = result,
            "dmarc" => out.dmarc = result,
            "dkim" => {
                if !dkim_seen || result == AuthResult::Pass {
                    out.dkim = result;
                }
                dkim_seen = true;
                for t in tokens {
                    let Some((k, v)) = t.split_once('=') else {
                        continue;
                    };
                    if k.eq_ignore_ascii_case("header.d") || k.eq_ignore_ascii_case("header.i") {
                        let domain = v.rsplit('@').next().unwrap_or(v);
                        let domain = domain
                            .trim_matches(|c: char| !c.is_ascii_alphanumeric())
                            .to_ascii_lowercase();
                        if !domain.is_empty()
                            && !out.dkim_domains.contains(&domain)
                            && out.dkim_domains.len() < MAX_DKIM_DOMAINS
                        {
                            out.dkim_domains.push(domain);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    Some(out)
}

/// The SPF result of a `Received-SPF` header: its first word.
fn received_spf(value: &str) -> AuthResult {
    value
        .split_whitespace()
        .next()
        .map(AuthResult::parse)
        .unwrap_or_default()
}

/// The lowercase domain of an address such as `<bounce@x.example>`.
fn address_domain(value: &str) -> Option<String> {
    let value = value.trim().trim_start_matches('<');
    let value = value.split(['>', ' ', '?']).next()?;
    let (_, domain) = value.rsplit_once('@')?;
    ascii_host(domain)
}

/// A domain as lowercase ASCII (IDNA), or `None` when it is not a valid domain.
fn ascii_host(domain: &str) -> Option<String> {
    let domain = domain.trim().trim_end_matches('.');
    if domain.is_empty() {
        return None;
    }
    match url::Host::parse(domain).ok()? {
        url::Host::Domain(d) => Some(d.to_ascii_lowercase()),
        url::Host::Ipv4(ip) => Some(ip.to_string()),
        url::Host::Ipv6(ip) => Some(format!("[{ip}]")),
    }
}

/// Where a link goes: the host of an http(s) URL or the domain of a `mailto:` address. Any
/// other scheme gives `None` and the link is dropped.
pub fn link_host(href: &str) -> Option<String> {
    let url = url::Url::parse(href.trim()).ok()?;
    match url.scheme() {
        "http" | "https" => url.host_str().map(str::to_ascii_lowercase),
        "mailto" => {
            let addr = url.path().split(',').next()?;
            let addr = url::form_urlencoded::parse(addr.as_bytes())
                .next()
                .map(|(k, _)| k.into_owned())
                .unwrap_or_default();
            address_domain(&addr)
        }
        _ => None,
    }
}

/// Whether a token looks like a bare domain: dot-separated labels of letters, digits and
/// hyphens, with an alphabetic top-level label of two or more characters.
fn domain_shaped(host: &str) -> bool {
    let labels: Vec<&str> = host.split('.').collect();
    labels.len() >= 2
        && labels.iter().all(|l| {
            !l.is_empty()
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.chars().all(|c| c.is_alphanumeric() || c == '-')
        })
        && labels
            .last()
            .is_some_and(|tld| tld.chars().count() >= 2 && tld.chars().all(char::is_alphabetic))
}

/// The host that a link's display text names, when the text is URL-shaped: an http(s) URL or
/// a `www.` link (the rules of `find_links`), or a bare domain with an optional path.
pub fn text_host(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() || text.contains(char::is_whitespace) {
        return None;
    }
    if let [(range, LinkTarget::Web(url))] = find_links(text).as_slice()
        && range.start == 0
        && range.end == text.len()
    {
        return link_host(url);
    }
    let host = text.split(['/', '?', '#']).next()?;
    let host = host.split(':').next()?;
    if !host.contains('@') && domain_shaped(host) {
        return ascii_host(host);
    }
    None
}

/// The link signals of a body: http(s) and mailto links only, deduplicated by (text, host)
/// and capped at [`MAX_LINKS`].
fn link_signals(links: &[(String, String)]) -> Vec<LinkSignal> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for (text, href) in links {
        if out.len() == MAX_LINKS {
            break;
        }
        let Some(host) = link_host(href) else {
            continue;
        };
        let text = text.trim().to_string();
        if !seen.insert((text.clone(), host.clone())) {
            continue;
        }
        out.push(LinkSignal {
            text_host: text_host(&text),
            text,
            host,
        });
    }
    out
}

/// The `(text, href)` pairs of a plain-text body: each detected link's text is the URL itself.
pub fn plain_links(body: &str) -> Vec<(String, String)> {
    find_links(body)
        .into_iter()
        .map(|(range, target)| (body[range].to_string(), target.url().to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(name: &str, value: &str) -> Header {
        Header {
            name: name.into(),
            value: value.into(),
        }
    }

    const GMAIL_PASS: &str = "mx.google.com;\n       dkim=pass header.i=@paypal.com header.s=pp-dkim1 \
         header.b=abc;\n       spf=pass (google.com: domain of service@paypal.com designates \
         1.2.3.4 as permitted sender) smtp.mailfrom=service@paypal.com;\n       \
         dmarc=pass (p=REJECT sp=REJECT dis=NONE) header.from=paypal.com";

    #[test]
    fn gmails_first_header_passes() {
        let s = signals(&[h("Authentication-Results", GMAIL_PASS)], &[]);
        assert_eq!(s.auth.spf, AuthResult::Pass);
        assert_eq!(s.auth.dkim, AuthResult::Pass);
        assert_eq!(s.auth.dmarc, AuthResult::Pass);
        assert_eq!(s.auth.dkim_domains, vec!["paypal.com".to_string()]);
    }

    #[test]
    fn a_forged_later_header_is_ignored() {
        let s = signals(
            &[
                h(
                    "Authentication-Results",
                    "mx.google.com; spf=softfail smtp.mailfrom=x@paypa1.example; dkim=none; \
                     dmarc=fail (p=NONE) header.from=paypal.com",
                ),
                h("Authentication-Results", GMAIL_PASS),
            ],
            &[],
        );
        assert_eq!(s.auth.spf, AuthResult::SoftFail);
        assert_eq!(s.auth.dkim, AuthResult::None);
        assert_eq!(s.auth.dmarc, AuthResult::Fail);
    }

    #[test]
    fn another_authserv_id_falls_back_to_received_spf() {
        let s = signals(
            &[
                h(
                    "Authentication-Results",
                    "evil.example; spf=pass; dmarc=pass",
                ),
                h(
                    "Received-SPF",
                    "neutral (google.com: 1.2.3.4 is neither permitted nor denied)",
                ),
                h("Received-SPF", "pass"),
            ],
            &[],
        );
        assert_eq!(s.auth.spf, AuthResult::Neutral);
        assert_eq!(s.auth.dkim, AuthResult::Unknown);
        assert_eq!(s.auth.dmarc, AuthResult::Unknown);
    }

    #[test]
    fn any_dkim_pass_counts_and_domains_are_collected() {
        let s = signals(
            &[h(
                "Authentication-Results",
                "MX.GOOGLE.COM; dkim=fail header.d=evil.example; dkim=pass header.d=Mail.Example \
                 header.s=s1; dkim=pass header.i=bounce@news.example.org",
            )],
            &[],
        );
        assert_eq!(s.auth.dkim, AuthResult::Pass);
        assert_eq!(
            s.auth.dkim_domains,
            vec!["evil.example", "mail.example", "news.example.org"]
        );
        let none = signals(
            &[h(
                "Authentication-Results",
                "mx.google.com; dkim=neutral (body hash did not verify) header.d=x.example; \
                 dkim=fail header.d=y.example",
            )],
            &[],
        );
        assert_eq!(none.auth.dkim, AuthResult::Neutral);
    }

    #[test]
    fn garbage_gives_unknown_and_never_panics() {
        for v in [
            "",
            ";;;",
            "mx.google.com; spf; dkim=; dmarc=maybe",
            "mx.google.com; (((",
            "\u{202e}mx.google.com",
        ] {
            let s = signals(&[h("Authentication-Results", v), h("Return-Path", v)], &[]);
            assert_ne!(s.auth.spf, AuthResult::Pass, "{v}");
            assert_ne!(s.auth.dmarc, AuthResult::Pass, "{v}");
        }
        let s = signals(
            &[h(
                "Authentication-Results",
                "mx.google.com; spf=bogus; dmarc=maybe",
            )],
            &[],
        );
        assert_eq!(s.auth.spf, AuthResult::Unknown);
        assert_eq!(s.auth.dmarc, AuthResult::Unknown);
    }

    #[test]
    fn return_path_unsubscribe_and_header_case() {
        let s = signals(
            &[
                h("return-path", "<Bounce@Bounce.PAYPA1-verify.example>"),
                h("Return-Path", "<other@x.example>"),
                h("list-unsubscribe", "<mailto:u@x.example>"),
                h("authentication-results", "mx.google.com; dmarc=FAIL"),
            ],
            &[],
        );
        assert_eq!(
            s.return_path_domain.as_deref(),
            Some("bounce.paypa1-verify.example")
        );
        assert!(s.list_unsubscribe);
        assert_eq!(s.auth.dmarc, AuthResult::Fail);
        let bare = signals(&[h("Return-Path", "<>")], &[]);
        assert_eq!(bare.return_path_domain, None);
        assert!(!bare.list_unsubscribe);
    }

    fn pair(text: &str, href: &str) -> (String, String) {
        (text.into(), href.into())
    }

    #[test]
    fn link_hosts_and_text_hosts() {
        let s = signals(
            &[],
            &[
                pair(
                    "https://www.paypal.com/signin",
                    "https://paypa1-verify.example/a?b=1",
                ),
                pair("www.paypal.com", "https://paypal.com/"),
                pair("paypal.com/help", "https://paypal.com/help"),
                pair("Read more", "https://n.example/a"),
                pair("javascript", "javascript:alert(1)"),
                pair("Write", "mailto:Help@Support.Example?subject=x"),
                pair("pay", "https://раураl.com/login"),
            ],
        );
        let got: Vec<(&str, &str, Option<&str>, bool)> = s
            .links
            .iter()
            .map(|l| {
                (
                    l.text.as_str(),
                    l.host.as_str(),
                    l.text_host.as_deref(),
                    l.mismatched(),
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                (
                    "https://www.paypal.com/signin",
                    "paypa1-verify.example",
                    Some("www.paypal.com"),
                    true
                ),
                (
                    "www.paypal.com",
                    "paypal.com",
                    Some("www.paypal.com"),
                    false
                ),
                ("paypal.com/help", "paypal.com", Some("paypal.com"), false),
                ("Read more", "n.example", None, false),
                ("Write", "support.example", None, false),
                ("pay", "xn--l-7sba6dbr.com", None, false),
            ]
        );
        assert_eq!(
            text_host("раураl.com").as_deref(),
            Some("xn--l-7sba6dbr.com")
        );
    }

    #[test]
    fn links_are_deduplicated_and_capped() {
        let mut links = vec![
            pair("a", "https://a.example/1"),
            pair("a", "https://a.example/2"),
        ];
        links.extend((0..60).map(|i| pair(&format!("t{i}"), &format!("https://h{i}.example/"))));
        let s = signals(&[], &links);
        assert_eq!(s.links.len(), MAX_LINKS);
        assert_eq!(s.links.iter().filter(|l| l.text == "a").count(), 1);
    }

    #[test]
    fn plain_bodies_use_find_links() {
        let body = "Reset: https://x.example/reset?token=abc and www.y.example/p or bob@z.example";
        let links = plain_links(body);
        assert_eq!(
            links,
            vec![
                pair(
                    "https://x.example/reset?token=abc",
                    "https://x.example/reset?token=abc"
                ),
                pair("www.y.example/p", "https://www.y.example/p"),
                pair("bob@z.example", "mailto:bob@z.example"),
            ]
        );
        let s = signals(&[], &links);
        assert_eq!(s.links[0].text_host.as_deref(), Some("x.example"));
        assert!(!s.links[0].mismatched());
        assert_eq!(s.links[2].host, "z.example");
    }
}
