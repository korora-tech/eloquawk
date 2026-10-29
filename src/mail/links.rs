//! Link detection in plain-text bodies.
//!
//! Only three kinds of target are ever produced: `http`, `https` (including bare `www.` hosts,
//! opened as https) and `mailto` (including bare email addresses). Any other scheme, such as
//! `file:` or `javascript:`, is never detected, so the UI cannot open it.

use std::ops::Range;

/// Where a detected link goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkTarget {
    /// An `http` or `https` URL, opened in the browser.
    Web(String),
    /// A `mailto:` URL, opened in the composer.
    Mail(String),
}

impl LinkTarget {
    pub fn url(&self) -> &str {
        match self {
            LinkTarget::Web(u) | LinkTarget::Mail(u) => u,
        }
    }
}

const SCHEMES: [(&str, bool); 3] = [("https://", false), ("http://", false), ("mailto:", true)];

/// Finds links in `text`, returning byte ranges (into `text`) in order with their targets.
pub fn find_links(text: &str) -> Vec<(Range<usize>, LinkTarget)> {
    let bytes = text.as_bytes();
    let mut out: Vec<(Range<usize>, LinkTarget)> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if !at_boundary(text, i) {
            i += 1;
            continue;
        }
        if let Some((end, target)) = url_at(text, i) {
            out.push((i..end, target));
            i = end;
            continue;
        }
        i += 1;
    }
    // Bare email addresses outside the URLs found above.
    let mut emails = Vec::new();
    for (at, _) in text.match_indices('@') {
        if out.iter().any(|(r, _)| r.contains(&at)) {
            continue;
        }
        if let Some(r) = email_around(text, at)
            && emails
                .last()
                .is_none_or(|(prev, _): &(Range<usize>, _)| prev.end <= r.start)
        {
            let target = LinkTarget::Mail(format!("mailto:{}", &text[r.clone()]));
            emails.push((r, target));
        }
    }
    out.extend(emails);
    out.sort_by_key(|(r, _)| r.start);
    out
}

/// A link may start at the beginning or after a character that cannot be part of a scheme,
/// so `xhttp://` and `javascript:http://` do not yield a link at the inner `http`.
fn at_boundary(text: &str, i: usize) -> bool {
    if !text.is_char_boundary(i) {
        return false;
    }
    match text[..i].chars().next_back() {
        None => true,
        Some(c) => !(c.is_alphanumeric() || "+-.:/@_".contains(c)),
    }
}

/// True when `s` starts with the ASCII `prefix`, ignoring ASCII case. Byte-wise, so it never
/// slices inside a multi-byte character.
fn starts_with_ci(s: &str, prefix: &str) -> bool {
    s.as_bytes()
        .get(..prefix.len())
        .is_some_and(|b| b.eq_ignore_ascii_case(prefix.as_bytes()))
}

fn url_at(text: &str, i: usize) -> Option<(usize, LinkTarget)> {
    let rest = &text[i..];
    for (scheme, mail) in SCHEMES {
        if starts_with_ci(rest, scheme) {
            let end = i + url_len(rest);
            if end <= i + scheme.len() {
                return None;
            }
            let url = &text[i..end];
            if mail {
                // mailto: needs an address after it.
                if !url[scheme.len()..].contains('@') {
                    return None;
                }
                return Some((end, LinkTarget::Mail(url.to_string())));
            }
            return Some((end, LinkTarget::Web(url.to_string())));
        }
    }
    if starts_with_ci(rest, "www.") {
        let end = i + url_len(rest);
        // `url_len` strips trailing punctuation, so for a bare `www.` the end can fall
        // before the host would start.
        let host = text.get(i + 4..end)?.split(['/', '?', '#', ':']).next()?;
        if host.contains('.') || host.len() >= 2 {
            let url = &text[i..end];
            if url.len() > 4 && !url.ends_with('.') {
                return Some((end, LinkTarget::Web(format!("https://{url}"))));
            }
        }
    }
    None
}

/// Length of the URL at the start of `s`: up to whitespace or a delimiter, then trailing
/// punctuation and unbalanced closing brackets are dropped.
fn url_len(s: &str) -> usize {
    let mut end = s
        .char_indices()
        .find(|(_, c)| c.is_whitespace() || c.is_control() || "<>\"`{}|\\^".contains(*c))
        .map_or(s.len(), |(i, _)| i);
    while let Some(last) = s[..end].chars().next_back() {
        let strip = match last {
            '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '*' => true,
            ')' => count(&s[..end], '(') < count(&s[..end], ')'),
            ']' => count(&s[..end], '[') < count(&s[..end], ']'),
            _ => false,
        };
        if !strip {
            break;
        }
        end -= last.len_utf8();
    }
    end
}

fn count(s: &str, c: char) -> usize {
    s.chars().filter(|&x| x == c).count()
}

/// The bare address around the `@` at byte `at`, if it looks like one.
fn email_around(text: &str, at: usize) -> Option<Range<usize>> {
    let bytes = text.as_bytes();
    let local = |b: u8| b.is_ascii_alphanumeric() || b"._%+-".contains(&b);
    let domain = |b: u8| b.is_ascii_alphanumeric() || b".-".contains(&b);
    let mut start = at;
    while start > 0 && local(bytes[start - 1]) {
        start -= 1;
    }
    while start < at && bytes[start] == b'.' {
        start += 1;
    }
    let mut end = at + 1;
    while end < bytes.len() && domain(bytes[end]) {
        end += 1;
    }
    while end > at + 1 && matches!(bytes[end - 1], b'.' | b'-') {
        end -= 1;
    }
    let dom = &text[at + 1..end];
    let tld_ok = dom
        .rsplit_once('.')
        .is_some_and(|(host, tld)| !host.is_empty() && tld.len() >= 2);
    (start < at && tld_ok).then_some(start..end)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(text: &str) -> Vec<(&str, LinkTarget)> {
        find_links(text)
            .into_iter()
            .map(|(r, t)| (&text[r], t))
            .collect()
    }

    fn web(s: &str) -> LinkTarget {
        LinkTarget::Web(s.to_string())
    }

    #[test]
    fn detects_http_https_and_www() {
        let t = "See http://a.example/x and HTTPS://b.example/y?q=1#f or www.c.example/z.";
        assert_eq!(
            found(t),
            vec![
                ("http://a.example/x", web("http://a.example/x")),
                (
                    "HTTPS://b.example/y?q=1#f",
                    web("HTTPS://b.example/y?q=1#f")
                ),
                ("www.c.example/z", web("https://www.c.example/z")),
            ]
        );
    }

    #[test]
    fn bare_www_without_a_host_is_not_a_link() {
        for t in ["www.", "see www. now", "(www.)", "www.,", "WWW.!"] {
            assert_eq!(found(t), vec![], "{t}");
        }
    }

    #[test]
    fn detects_mailto_and_bare_addresses() {
        let t = "Write to mailto:help@example.com?subject=Hi or bob.smith+tag@mail.example.org.";
        assert_eq!(
            found(t),
            vec![
                (
                    "mailto:help@example.com?subject=Hi",
                    LinkTarget::Mail("mailto:help@example.com?subject=Hi".into())
                ),
                (
                    "bob.smith+tag@mail.example.org",
                    LinkTarget::Mail("mailto:bob.smith+tag@mail.example.org".into())
                ),
            ]
        );
    }

    #[test]
    fn trailing_punctuation_is_excluded() {
        assert_eq!(
            found("(see https://x.example/a)")[0].0,
            "https://x.example/a"
        );
        assert_eq!(
            found("go to https://x.example/a.")[0].0,
            "https://x.example/a"
        );
        assert_eq!(
            found("https://x.example/a, then")[0].0,
            "https://x.example/a"
        );
        assert_eq!(
            found("https://en.example/wiki/Foo_(bar)")[0].0,
            "https://en.example/wiki/Foo_(bar)"
        );
        assert_eq!(found("<https://x.example/>")[0].0, "https://x.example/");
        assert_eq!(found("\"https://x.example/q\"")[0].0, "https://x.example/q");
    }

    #[test]
    fn other_schemes_are_rejected() {
        assert!(find_links("file:///etc/passwd").is_empty());
        assert!(find_links("javascript:alert(1)").is_empty());
        assert!(find_links("myapp://open?x=1 ftp://x.example/").is_empty());
        assert!(find_links("javascript:http://x.example/").is_empty());
        assert!(find_links("xhttps://x.example/").is_empty());
        assert!(find_links("http:// nothing").is_empty());
        assert!(find_links("mailto:nobody").is_empty());
        assert!(find_links("a@b and @handle and x@localhost").is_empty());
    }

    #[test]
    fn byte_ranges_are_correct_around_non_ascii() {
        let t = "Café ☕: https://café.example/ü — Zoë <zoe@example.com>, zoë@example.com";
        let links = find_links(t);
        let texts: Vec<&str> = links.iter().map(|(r, _)| &t[r.clone()]).collect();
        // A non-ASCII local part is not detected, and neither is its ASCII tail.
        assert_eq!(texts, vec!["https://café.example/ü", "zoe@example.com"]);
        for (r, _) in &links {
            assert!(t.is_char_boundary(r.start) && t.is_char_boundary(r.end));
        }
    }
}
