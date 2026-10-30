//! HTML mail to plain text, and entity decoding for Gmail snippets.
//!
//! Eloquawk never renders HTML. Bodies go through html2text with the `InlineLinks` decorator,
//! which drops emphasis, heading and image markup and keeps each link's real target visible
//! (`text <href>`), so a link whose text differs from its target is obvious. Images, and so
//! tracking pixels, are dropped: nothing remote is ever fetched. The input is capped and the
//! conversion runs under `catch_unwind`, so a hostile message cannot take the worker down.

use std::panic::{AssertUnwindSafe, catch_unwind};

use html2text::config;
use html2text::render::TaggedLine;
use html2text::render::TextDecorator;

/// HTML input larger than this is truncated (on a char boundary) before parsing.
pub const MAX_HTML_INPUT: usize = 2 * 1024 * 1024;

/// Wrap width handed to html2text. The UI wraps text itself, so this is effectively "never".
const RENDER_WIDTH: usize = 10_000;

/// The result of converting an HTML body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Converted {
    pub text: String,
    /// The input was over [`MAX_HTML_INPUT`] and only its start was converted.
    pub truncated: bool,
    /// Every link with a scheme, as `(display text, href)`, from the same spans that produce
    /// the `text <href>` output (SPEC 16.3). `mail::auth` keeps the http(s) and mailto ones.
    pub links: Vec<(String, String)>,
}

/// Converts HTML to plain text. `None` means the conversion failed (html2text returned an
/// error or panicked); the caller falls back to the snippet and flags the body as truncated.
pub fn html_to_text(html: &str) -> Option<Converted> {
    let truncated = html.len() > MAX_HTML_INPUT;
    let input = if truncated {
        &html[..floor_char_boundary(html, MAX_HTML_INPUT)]
    } else {
        html
    };
    let lines = guarded(|| {
        config::with_decorator(InlineLinks)
            .link_footnotes(false)
            .no_table_borders()
            .allow_width_overflow()
            .lines_from_read(input.as_bytes(), RENDER_WIDTH)
    })?;
    let mut links = Vec::new();
    let text = tidy(&render_lines(&lines, &mut links));
    Some(Converted {
        text,
        truncated,
        links,
    })
}

/// Runs a conversion, turning an html2text error or a panic into `None`.
fn guarded<T>(f: impl FnOnce() -> Result<T, html2text::Error>) -> Option<T> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(v)) => Some(v),
        Ok(Err(e)) => {
            eprintln!("eloquawk: html conversion failed: {e}");
            None
        }
        Err(_) => {
            eprintln!("eloquawk: html conversion panicked");
            None
        }
    }
}

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Annotation carried by html2text spans: the link target, if inside a link.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum LinkAnn {
    #[default]
    None,
    Link(String),
}

/// html2text decorator that emits no markup at all, only annotations for links. The link text
/// and target are combined afterwards in [`render_lines`], because the decorator's link-end
/// hook cannot see the text.
#[derive(Clone, Debug)]
struct InlineLinks;

impl TextDecorator for InlineLinks {
    type Annotation = LinkAnn;

    fn decorate_link_start(&mut self, url: &str) -> (String, LinkAnn) {
        (String::new(), LinkAnn::Link(url.to_string()))
    }
    fn decorate_link_end(&mut self) -> String {
        String::new()
    }
    fn decorate_em_start(&self) -> (String, LinkAnn) {
        Default::default()
    }
    fn decorate_em_end(&self) -> String {
        String::new()
    }
    fn decorate_strong_start(&self) -> (String, LinkAnn) {
        Default::default()
    }
    fn decorate_strong_end(&self) -> String {
        String::new()
    }
    fn decorate_strikeout_start(&self) -> (String, LinkAnn) {
        Default::default()
    }
    fn decorate_strikeout_end(&self) -> String {
        String::new()
    }
    fn decorate_code_start(&self) -> (String, LinkAnn) {
        Default::default()
    }
    fn decorate_code_end(&self) -> String {
        String::new()
    }
    fn decorate_preformat_first(&self) -> LinkAnn {
        LinkAnn::None
    }
    fn decorate_preformat_cont(&self) -> LinkAnn {
        LinkAnn::None
    }
    fn decorate_image(&mut self, _src: &str, _title: &str) -> (String, LinkAnn) {
        // Images (and so tracking pixels) are dropped, alt text included.
        Default::default()
    }
    fn header_prefix(&self, _level: usize) -> String {
        String::new()
    }
    fn quote_prefix(&self) -> String {
        "> ".to_string()
    }
    fn unordered_item_prefix(&self) -> String {
        // ASCII on purpose: html2text 0.17 subtracts the prefix's byte length from a width
        // estimate, which underflows (and panics in debug builds) for a multi-byte bullet.
        "- ".to_string()
    }
    fn ordered_item_prefix(&self, i: i64) -> String {
        format!("{i}. ")
    }
    fn make_subblock_decorator(&self) -> Self {
        InlineLinks
    }
    fn decorate_superscript_start(&self) -> (String, LinkAnn) {
        Default::default()
    }
    fn decorate_superscript_end(&self) -> String {
        String::new()
    }
    fn finalise(&mut self, _urls: Vec<String>) -> Vec<TaggedLine<LinkAnn>> {
        Vec::new()
    }
}

/// Joins html2text's tagged lines, writing each link as `text <href>`, or as the bare href
/// when the text already says the same thing.
fn render_lines(lines: &[TaggedLine<Vec<LinkAnn>>], links: &mut Vec<(String, String)>) -> String {
    let mut out = String::new();
    for line in lines {
        let mut link: Option<(String, String)> = None; // (href, text so far)
        for ts in line.tagged_strings() {
            let href = ts.tag.iter().rev().find_map(|a| match a {
                LinkAnn::Link(u) => Some(u.as_str()),
                LinkAnn::None => None,
            });
            let piece = &ts.s;
            match (&mut link, href) {
                (Some((cur, text)), Some(h)) if cur == h => text.push_str(piece),
                _ => {
                    if let Some((h, t)) = link.take() {
                        push_link(&mut out, links, &h, &t);
                    }
                    match href {
                        Some(h) => link = Some((h.to_string(), piece.clone())),
                        None => out.push_str(piece),
                    }
                }
            }
        }
        if let Some((h, t)) = link.take() {
            push_link(&mut out, links, &h, &t);
        }
        out.push('\n');
    }
    out
}

fn push_link(out: &mut String, links: &mut Vec<(String, String)>, href: &str, text: &str) {
    let href = href.trim();
    let shown = text.trim();
    // Relative and in-page links have no target worth showing.
    if !has_scheme(href) {
        out.push_str(text);
        return;
    }
    links.push((shown.to_string(), href.to_string()));
    let lead = &text[..text.len() - text.trim_start().len()];
    let trail = &text[text.trim_end().len()..];
    out.push_str(lead);
    if shown.is_empty() || shown == href {
        out.push_str(href);
    } else if same_target(shown, href) {
        // "example.com" for "https://example.com/": show the full, clickable target. A mailto
        // whose text is the address keeps the text, which is itself detected as a link.
        if href.to_ascii_lowercase().starts_with("mailto:") {
            out.push_str(shown);
        } else {
            out.push_str(href);
        }
    } else {
        out.push_str(shown);
        out.push_str(" <");
        out.push_str(href);
        out.push('>');
    }
    out.push_str(trail);
}

fn has_scheme(href: &str) -> bool {
    match href.find(':') {
        Some(i) if i > 0 => href[..i]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c)),
        _ => false,
    }
}

/// Loose comparison of link text and target: case, scheme, "www." and a trailing slash are
/// ignored.
fn same_target(text: &str, href: &str) -> bool {
    fn norm(s: &str) -> String {
        let s = s.trim().to_lowercase();
        let s = ["https://", "http://", "mailto:"]
            .iter()
            .find_map(|p| s.strip_prefix(p))
            .unwrap_or(&s);
        let s = s.strip_prefix("www.").unwrap_or(s);
        s.trim_end_matches('/').to_string()
    }
    norm(text) == norm(href)
}

/// Trims trailing whitespace, shrinks the column padding html2text puts between table cells
/// (runs of three or more spaces become two), collapses runs of blank lines and trims the ends.
fn tidy(text: &str) -> String {
    let mut lines: Vec<String> = text.lines().map(|l| squeeze_spaces(l.trim_end())).collect();
    lines.dedup_by(|a, b| a.is_empty() && b.is_empty());
    lines.join("\n").trim_matches('\n').to_string()
}

fn squeeze_spaces(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut run = 0;
    for c in line.chars() {
        if c == ' ' {
            run += 1;
            continue;
        }
        let keep = if run >= 3 { 2 } else { run };
        out.extend(std::iter::repeat_n(' ', keep));
        run = 0;
        out.push(c);
    }
    out
}

/// Decodes HTML character references in a Gmail snippet (`&#39;`, `&amp;`, `&hellip;`, ...).
/// Unknown or malformed references are kept as written.
pub fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let decoded = rest[1..]
            .find(';')
            .filter(|&semi| semi <= 32)
            .and_then(|semi| entity(&rest[1..=semi]).map(|c| (c, semi + 2)));
        match decoded {
            Some((c, len)) => {
                out.push(c);
                rest = &rest[len..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn entity(name: &str) -> Option<char> {
    if let Some(num) = name.strip_prefix('#') {
        let code = match num.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => num.parse().ok()?,
        };
        return char::from_u32(code).filter(|c| *c != '\0');
    }
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => '\u{a0}',
        "hellip" => '…',
        "mdash" => '—',
        "ndash" => '–',
        "lsquo" => '‘',
        "rsquo" => '’',
        "ldquo" => '“',
        "rdquo" => '”',
        "bull" => '•',
        "middot" => '·',
        "copy" => '©',
        "reg" => '®',
        "trade" => '™',
        "euro" => '€',
        "pound" => '£',
        "zwnj" => '\u{200c}',
        "zwj" => '\u{200d}',
        "shy" => '\u{ad}',
        "laquo" => '«',
        "raquo" => '»',
        "deg" => '°',
        "times" => '×',
        "divide" => '÷',
        "sect" => '§',
        "iexcl" => '¡',
        "iquest" => '¿',
        _ => return latin1_letter(name),
    })
}

/// Named references for the Latin-1 letters: `&eacute;`, `&Ouml;`, `&szlig;` and so on.
fn latin1_letter(name: &str) -> Option<char> {
    const MARKS: [(&str, &str, &str); 7] = [
        ("grave", "aeiou", "àèìòù"),
        ("acute", "aeiouy", "áéíóúý"),
        ("circ", "aeiou", "âêîôû"),
        ("tilde", "ano", "ãñõ"),
        ("uml", "aeiouy", "äëïöüÿ"),
        ("ring", "a", "å"),
        ("cedil", "c", "ç"),
    ];
    match name {
        "szlig" => return Some('ß'),
        "aelig" => return Some('æ'),
        "AElig" => return Some('Æ'),
        "oslash" => return Some('ø'),
        "Oslash" => return Some('Ø'),
        _ => {}
    }
    let mut chars = name.chars();
    let base = chars.next()?;
    let mark = chars.as_str();
    let (_, bases, accented) = MARKS.iter().find(|(m, _, _)| *m == mark)?;
    let pos = bases.chars().position(|b| b == base.to_ascii_lowercase())?;
    let c = accented.chars().nth(pos)?;
    if base.is_ascii_uppercase() {
        // Uppercase Ÿ is outside Latin-1 and has no named reference here.
        (c != 'ÿ').then(|| c.to_uppercase().next()).flatten()
    } else {
        Some(c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(html: &str) -> String {
        html_to_text(html).expect("converts").text
    }

    #[test]
    fn drops_script_and_style() {
        let t = text(
            "<html><head><style>p{color:red}</style><script>alert('x')</script></head>\
             <body><p>Hello</p><script>var a = 1;</script></body></html>",
        );
        assert_eq!(t, "Hello");
    }

    #[test]
    fn br_p_and_li() {
        let t =
            text("<p>One<br>Two</p><p>Three</p><ul><li>a</li><li>b</li></ul><ol><li>x</li></ol>");
        assert_eq!(t, "One\nTwo\n\nThree\n- a\n- b\n1. x");
    }

    #[test]
    fn entities_numeric_and_named() {
        assert_eq!(
            text("<p>It&#8217;s here&hellip; &amp; &lt;ok&gt;</p>"),
            "It’s here… & <ok>"
        );
    }

    #[test]
    fn images_are_dropped() {
        let t = text(
            "<p>Before<img src=\"https://t.example/pixel.gif\" width=1 height=1>\
             <img src=\"https://x.example/a.png\" alt=\"Logo\">After</p>",
        );
        assert_eq!(t, "BeforeAfter");
    }

    #[test]
    fn anchors_render_inline_without_footnotes_or_emphasis() {
        let t = text(
            "<h1>Big <em>news</em></h1><p><strong>Read</strong> \
             <a href=\"https://evil.example/login\">https://bank.example</a> or \
             <a href=\"https://example.com/a\">https://example.com/a</a> or \
             <a href=\"https://example.com/\">example.com</a> or \
             <a href=\"mailto:bob@example.com\">bob@example.com</a> or \
             <a href=\"#top\">top</a>.</p>",
        );
        assert_eq!(
            t,
            "Big news\n\nRead https://bank.example <https://evil.example/login> or \
             https://example.com/a or https://example.com/ or bob@example.com or top."
        );
        assert!(!t.contains('['));
        assert!(!t.contains('*'));
    }

    #[test]
    fn links_are_collected_with_their_text() {
        let c = html_to_text(
            "<p><a href=\"https://evil.example/login\">https://bank.example</a> \
             <a href=\"https://shop.example/x\">Shop <b>now</b></a> <a href=\"#top\">top</a> \
             <a href=\"javascript:x()\">js</a></p>",
        )
        .unwrap();
        assert_eq!(
            c.links,
            vec![
                (
                    "https://bank.example".to_string(),
                    "https://evil.example/login".to_string()
                ),
                ("Shop now".to_string(), "https://shop.example/x".to_string()),
                ("js".to_string(), "javascript:x()".to_string()),
            ]
        );
    }

    #[test]
    fn link_with_nested_markup_is_one_link() {
        let t = text("<a href=\"https://shop.example/x\">Shop <b>now</b> today</a>");
        assert_eq!(t, "Shop now today <https://shop.example/x>");
    }

    #[test]
    fn image_only_link_is_dropped_with_the_image() {
        // html2text prunes a link with no text before the decorator sees it. Known limitation:
        // a banner link without text disappears rather than showing its href.
        let t = text(
            "<p>A<a href=\"https://x.example/\"><img src=\"https://x.example/b.png\"></a>B</p>",
        );
        assert_eq!(t, "AB");
    }

    #[test]
    fn table_layout_is_flattened_and_blockquotes_quoted() {
        let t = text(
            "<table width=\"600\"><tr><td><h2>Weekly digest</h2><p>Item one.</p></td></tr>\
             <tr><td><a href=\"https://n.example/a\">Read more</a></td><td>Side</td></tr>\
             </table><blockquote>quoted<br>line</blockquote>",
        );
        assert_eq!(
            t,
            "Weekly digest\n\nItem one.\nRead more <https://n.example/a>  Side\n\n> quoted\n> line"
        );
    }

    #[test]
    fn long_paragraphs_are_not_hard_wrapped() {
        let para = "word ".repeat(200);
        let t = text(&format!("<p>{para}</p>"));
        assert_eq!(t.lines().count(), 1);
    }

    #[test]
    fn oversized_input_is_truncated_and_flagged() {
        let mut html = String::from("<p>");
        // A multi-byte char straddles the cap to exercise the char boundary.
        while html.len() < MAX_HTML_INPUT - 1 {
            html.push('a');
        }
        html.push_str("éééé</p><p>tail</p>");
        let c = html_to_text(&html).unwrap();
        assert!(c.truncated);
        assert!(!c.text.contains("tail"));
        let small = html_to_text("<p>hi</p>").unwrap();
        assert!(!small.truncated);
    }

    #[test]
    fn conversion_panics_and_errors_become_none() {
        assert_eq!(guarded::<()>(|| panic!("boom")), None);
        assert_eq!(guarded::<()>(|| Err(html2text::Error::Fail)), None);
        assert_eq!(guarded(|| Ok(1)), Some(1));
    }

    #[test]
    fn snippet_entities_decode() {
        assert_eq!(
            decode_entities(
                "Don&#39;t &quot;miss&quot; it &amp; more&hellip; &#x1F600; &bogus; & x"
            ),
            "Don't \"miss\" it & more… 😀 &bogus; & x"
        );
        assert_eq!(decode_entities("&#0; &#xZZ; &"), "&#0; &#xZZ; &");
        assert_eq!(
            decode_entities("caf&eacute; &Ouml;l &ntilde; &szlig; &Ccedil; &xacute;"),
            "café Öl ñ ß Ç &xacute;"
        );
    }
}
