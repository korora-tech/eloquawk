//! HTML mail as a small, safe display model.
//!
//! The reader never hands mail HTML to an engine. This module tokenises it itself and builds a
//! [`RichDoc`]: runs of styled text (bold, italic, monospace, strike), links, and remote
//! images, and nothing else. Whatever the HTML says about scripts, styles, colours, fonts,
//! forms, frames, positioning or event handlers has no representation in the output, so it
//! cannot take effect. The output is a typed model that gpui draws; it is never re-serialised
//! to HTML, so parser differences do not matter.
//!
//! What the model keeps:
//! - Text, with whitespace collapsed the way a browser would, `<br>` as line breaks, and
//!   paragraphs, headings, list items, block quotes, rules and `<pre>` as separate lines.
//!   Tables and other layout containers flow as blocks.
//! - Links whose target is `http`, `https` or `mailto`. A link whose visible text does not
//!   name the real host gets that host appended, like the plain-text view's `text <href>`.
//! - Images, as [`RichImage`] placeholders. Nothing is fetched here; whether and when an image
//!   loads is the reader's decision. Tracking pixels (1x1 and hidden images) are dropped
//!   outright, so they cannot even be offered.
//! - A trailing quoted reply (Gmail, Apple Mail, Thunderbird, Yahoo), split off so the reader
//!   can fold it. A quote followed by more content is left in place.
//!
//! Hidden content (`display:none`, `hidden`, zero font size, preheader padding) is dropped, as
//! are invisible and bidirectional control characters, which spoof text order.

use std::ops::Range;

use url::Url;

use super::html::{MAX_HTML_INPUT, decode_entities};
use super::links::LinkTarget;

/// Images past this many in one message are not offered.
pub const MAX_IMAGES: usize = 200;
/// Elements nested deeper than this are flattened into their parent.
const MAX_DEPTH: usize = 128;
/// Attribute values and URLs longer than this are ignored.
const MAX_URL: usize = 2048;
/// Rendered text stops growing here; the document is flagged truncated.
const MAX_TEXT: usize = 1_500_000;

/// Text styling, as a small set of flags.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Marks(u8);

impl Marks {
    pub const BOLD: Marks = Marks(1);
    pub const ITALIC: Marks = Marks(2);
    pub const MONO: Marks = Marks(4);
    pub const STRIKE: Marks = Marks(8);

    pub fn contains(self, other: Marks) -> bool {
        self.0 & other.0 == other.0
    }

    pub fn union(self, other: Marks) -> Marks {
        Marks(self.0 | other.0)
    }

    fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The flags as stored by the thread cache.
    pub fn bits(self) -> u8 {
        self.0
    }

    /// Flags from [`Marks::bits`]; bits no flag owns are dropped.
    pub fn from_bits(bits: u8) -> Marks {
        Marks(bits & 0x0f)
    }
}

/// A run of text with its styled and linked byte ranges. Ranges are ascending, do not
/// overlap, and lie on char boundaries.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RichText {
    pub text: String,
    pub marks: Vec<(Range<usize>, Marks)>,
    pub links: Vec<(Range<usize>, LinkTarget)>,
}

impl RichText {
    /// Drops trailing whitespace, clipping the ranges.
    fn trim_end(&mut self) {
        let keep = self.text.trim_end().len();
        self.text.truncate(keep);
        clip_end(&mut self.marks, keep);
        clip_end(&mut self.links, keep);
    }

    /// Drops leading whitespace, shifting the ranges.
    fn trim_start(&mut self) {
        let cut = self.text.len() - self.text.trim_start().len();
        if cut > 0 {
            self.text.drain(..cut);
            shift_start(&mut self.marks, cut);
            shift_start(&mut self.links, cut);
        }
    }

    /// Splits at byte `at` (a char boundary): the text before and the text from `at`.
    fn split_at(mut self, at: usize) -> (RichText, RichText) {
        let tail_text = self.text.split_off(at);
        let tail_marks = tail_of(&self.marks, at);
        let tail_links = tail_of(&self.links, at);
        clip_end(&mut self.marks, at);
        clip_end(&mut self.links, at);
        (
            self,
            RichText {
                text: tail_text,
                marks: tail_marks,
                links: tail_links,
            },
        )
    }
}

fn clip_end<T>(ranges: &mut Vec<(Range<usize>, T)>, end: usize) {
    ranges.retain(|(r, _)| r.start < end);
    for (r, _) in ranges.iter_mut() {
        r.end = r.end.min(end);
    }
}

fn shift_start<T>(ranges: &mut Vec<(Range<usize>, T)>, cut: usize) {
    ranges.retain(|(r, _)| r.end > cut);
    for (r, _) in ranges.iter_mut() {
        r.start = r.start.saturating_sub(cut);
        r.end -= cut;
    }
}

fn tail_of<T: Clone>(ranges: &[(Range<usize>, T)], at: usize) -> Vec<(Range<usize>, T)> {
    ranges
        .iter()
        .filter(|(r, _)| r.end > at)
        .map(|(r, v)| (r.start.saturating_sub(at)..r.end - at, v.clone()))
        .collect()
}

/// How an image's `src` can be handled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageKind {
    /// An `https` URL: loadable when the user allows it.
    Remote,
    /// A plain `http` URL: never loaded, since the request and the image are unprotected.
    Insecure,
}

/// An image the message refers to. Nothing is fetched for it until the reader decides to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RichImage {
    pub url: String,
    pub kind: ImageKind,
    pub alt: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Where a click on the image goes, when it sits inside a link.
    pub link: Option<LinkTarget>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Segment {
    Text(RichText),
    Image(RichImage),
}

/// A sanitised message body.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RichDoc {
    pub main: Vec<Segment>,
    /// The trailing quoted reply, to fold behind "• • •".
    pub quote: Option<Vec<Segment>>,
    /// The input was over [`MAX_HTML_INPUT`] or the text cap and only the start was used.
    pub truncated: bool,
}

impl RichDoc {
    /// Every image in the document, quote included, in order.
    pub fn images(&self) -> impl Iterator<Item = &RichImage> {
        self.main
            .iter()
            .chain(self.quote.iter().flatten())
            .filter_map(|s| match s {
                Segment::Image(i) => Some(i),
                Segment::Text(_) => None,
            })
    }
}

/// Builds a [`RichDoc`] from an HTML body. Never panics on any input: the tokeniser and the
/// builder only index at positions they found themselves.
pub fn sanitise(html: &str) -> RichDoc {
    let truncated = html.len() > MAX_HTML_INPUT;
    let input = if truncated {
        let mut end = MAX_HTML_INPUT;
        while end > 0 && !html.is_char_boundary(end) {
            end -= 1;
        }
        &html[..end]
    } else {
        html
    };
    let mut b = Builder::new();
    tokenize(input, &mut |tok| match tok {
        Tok::Text(t) => b.text(&decode_entities(t)),
        Tok::Start { name, attrs } => b.start(&name, &attrs),
        Tok::End(name) => b.end(&name),
    });
    let mut doc = b.finish();
    doc.truncated |= truncated;
    doc
}

// ----- tokeniser ----------------------------------------------------------------------------

enum Tok<'a> {
    Text(&'a str),
    Start {
        name: String,
        attrs: Vec<(String, String)>,
    },
    End(String),
}

/// Elements whose content is not markup; it is skipped up to the matching end tag.
const RAW_TEXT: [&str; 4] = ["script", "style", "textarea", "title"];

/// Attributes worth keeping; everything else is dropped as it is read.
const KEPT_ATTRS: [&str; 9] = [
    "href", "src", "alt", "width", "height", "style", "class", "hidden", "type",
];

fn tokenize(input: &str, sink: &mut dyn FnMut(Tok<'_>)) {
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let Some(lt) = bytes[i..].iter().position(|&b| b == b'<').map(|p| i + p) else {
            sink(Tok::Text(&input[i..]));
            return;
        };
        if lt > i {
            sink(Tok::Text(&input[i..lt]));
        }
        i = lt;
        let rest = &bytes[i..];
        if rest.starts_with(b"<!--") {
            i = find(bytes, i + 4, b"-->").map_or(bytes.len(), |e| e + 3);
        } else if rest.starts_with(b"<!") || rest.starts_with(b"<?") {
            i = find(bytes, i + 2, b">").map_or(bytes.len(), |e| e + 1);
        } else if rest.starts_with(b"</") && rest.get(2).is_some_and(u8::is_ascii_alphabetic) {
            let (name, end) = read_name(input, i + 2);
            i = find(bytes, end, b">").map_or(bytes.len(), |e| e + 1);
            sink(Tok::End(name));
        } else if rest.get(1).is_some_and(u8::is_ascii_alphabetic) {
            let (name, attrs, end) = read_tag(input, i + 1);
            i = end;
            let raw = RAW_TEXT.contains(&name.as_str());
            sink(Tok::Start {
                name: name.clone(),
                attrs,
            });
            if raw {
                i = skip_raw(input, i, &name);
                sink(Tok::End(name));
            }
        } else {
            // A lone `<` is text.
            sink(Tok::Text("<"));
            i += 1;
        }
    }
}

fn find(bytes: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if from > bytes.len() {
        return None;
    }
    bytes[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| from + p)
}

/// Reads a tag name starting at `at`, lower-cased, and the index just past it.
fn read_name(input: &str, at: usize) -> (String, usize) {
    let bytes = input.as_bytes();
    let mut end = at;
    while end < bytes.len() && !matches!(bytes[end], b' ' | b'\t' | b'\n' | b'\r' | b'/' | b'>') {
        end += 1;
    }
    (input[at..end].to_ascii_lowercase(), end)
}

/// Reads a start tag's name and kept attributes, returning the index just past `>`.
fn read_tag(input: &str, at: usize) -> (String, Vec<(String, String)>, usize) {
    let bytes = input.as_bytes();
    let (name, mut i) = read_name(input, at);
    let mut attrs = Vec::new();
    loop {
        while i < bytes.len() && matches!(bytes[i], b' ' | b'\t' | b'\n' | b'\r' | b'/') {
            i += 1;
        }
        if i >= bytes.len() {
            return (name, attrs, bytes.len());
        }
        if bytes[i] == b'>' {
            return (name, attrs, i + 1);
        }
        let start = i;
        while i < bytes.len()
            && !matches!(bytes[i], b' ' | b'\t' | b'\n' | b'\r' | b'/' | b'>' | b'=')
        {
            i += 1;
        }
        let attr = input[start..i].to_ascii_lowercase();
        while i < bytes.len() && matches!(bytes[i], b' ' | b'\t' | b'\n' | b'\r') {
            i += 1;
        }
        let mut value = "";
        if i < bytes.len() && bytes[i] == b'=' {
            i += 1;
            while i < bytes.len() && matches!(bytes[i], b' ' | b'\t' | b'\n' | b'\r') {
                i += 1;
            }
            if i < bytes.len() && matches!(bytes[i], b'"' | b'\'') {
                let quote = bytes[i];
                let from = i + 1;
                let to = bytes[from..]
                    .iter()
                    .position(|&b| b == quote)
                    .map_or(bytes.len(), |p| from + p);
                value = &input[from..to];
                i = (to + 1).min(bytes.len());
            } else {
                let from = i;
                while i < bytes.len() && !matches!(bytes[i], b' ' | b'\t' | b'\n' | b'\r' | b'>') {
                    i += 1;
                }
                value = &input[from..i];
            }
        }
        if KEPT_ATTRS.contains(&attr.as_str()) && attrs.len() < 24 && value.len() <= 4 * MAX_URL {
            attrs.push((attr, decode_entities(value)));
        }
    }
}

/// Skips raw-text content up to and including `</name ... >`, returning the index after it.
fn skip_raw(input: &str, from: usize, name: &str) -> usize {
    let bytes = input.as_bytes();
    let mut i = from;
    while let Some(p) = find(bytes, i, b"</") {
        let start = p + 2;
        let end = start + name.len();
        if end <= bytes.len() && bytes[start..end].eq_ignore_ascii_case(name.as_bytes()) {
            return find(bytes, end, b">").map_or(bytes.len(), |e| e + 1);
        }
        i = start;
    }
    bytes.len()
}

// ----- builder ------------------------------------------------------------------------------

/// Elements dropped with everything inside them.
const SKIPPED: [&str; 16] = [
    "script", "style", "head", "title", "template", "noscript", "iframe", "object", "embed", "svg",
    "math", "select", "canvas", "video", "audio", "applet",
];

/// Elements with no end tag.
const VOID: [&str; 14] = [
    "br", "hr", "img", "input", "meta", "link", "base", "area", "col", "embed", "param", "source",
    "track", "wbr",
];

#[derive(Clone)]
struct Frame {
    name: String,
    marks: Marks,
    link: Option<LinkTarget>,
    pre: bool,
    hidden: bool,
    quote: usize,
    /// `Some(ordered)` on `ul` and `ol`.
    list: Option<bool>,
    counter: u32,
    list_depth: usize,
    /// Where the text inside a link began, for the host marker.
    link_from: usize,
    link_segments: usize,
}

impl Frame {
    fn root() -> Self {
        Frame {
            name: String::new(),
            marks: Marks::default(),
            link: None,
            pre: false,
            hidden: false,
            quote: 0,
            list: None,
            counter: 0,
            list_depth: 0,
            link_from: 0,
            link_segments: 0,
        }
    }
}

struct Fold {
    segment: usize,
    offset: usize,
    /// The stack depth below the quote element; the quote ends when the stack shrinks to it.
    depth: usize,
    closed: bool,
}

struct Builder {
    stack: Vec<Frame>,
    segments: Vec<Segment>,
    cur: RichText,
    pending_break: usize,
    pending_prefix: Option<String>,
    space_pending: bool,
    images: usize,
    total: usize,
    truncated: bool,
    fold: Option<Fold>,
    content_after_fold: bool,
}

/// The blank lines (1 = a line break, 2 = a blank line) a block element puts around itself.
fn block_gap(name: &str) -> usize {
    match name {
        "p" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "blockquote" | "pre" | "hr" | "ul"
        | "ol" | "dl" | "table" => 2,
        "div" | "section" | "article" | "header" | "footer" | "main" | "nav" | "aside"
        | "figure" | "figcaption" | "tr" | "li" | "dt" | "dd" | "address" | "form" | "fieldset"
        | "center" | "caption" | "details" | "summary" => 1,
        _ => 0,
    }
}

impl Builder {
    fn new() -> Self {
        Builder {
            stack: vec![Frame::root()],
            segments: Vec::new(),
            cur: RichText::default(),
            pending_break: 0,
            pending_prefix: None,
            space_pending: false,
            images: 0,
            total: 0,
            truncated: false,
            fold: None,
            content_after_fold: false,
        }
    }

    fn top(&self) -> &Frame {
        self.stack.last().expect("the root frame is never popped")
    }

    fn brk(&mut self, n: usize) {
        self.pending_break = self.pending_break.max(n);
        if n > 0 {
            self.space_pending = false;
        }
    }

    fn start(&mut self, name: &str, attrs: &[(String, String)]) {
        let attr = |k: &str| attrs.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        if name == "body" {
            // A head that was never closed must not swallow the body.
            self.stack.truncate(1);
            return;
        }
        let void = VOID.contains(&name);
        let mut f = self.top().clone();
        f.name = name.to_string();
        if SKIPPED.contains(&name) {
            f.hidden = true;
        }
        if attr("hidden").is_some() || attr("style").is_some_and(style_hides) {
            f.hidden = true;
        }
        let was_hidden = self.top().hidden;
        if !f.hidden {
            self.brk(block_gap(name));
        }
        match name {
            "b" | "strong" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                f.marks = f.marks.union(Marks::BOLD)
            }
            "i" | "em" | "cite" | "dfn" | "var" => f.marks = f.marks.union(Marks::ITALIC),
            "code" | "tt" | "kbd" | "samp" => f.marks = f.marks.union(Marks::MONO),
            "s" | "strike" | "del" => f.marks = f.marks.union(Marks::STRIKE),
            "pre" => {
                f.pre = true;
                f.marks = f.marks.union(Marks::MONO);
            }
            "blockquote" => f.quote += 1,
            "ul" | "ol" => {
                f.list = Some(name == "ol");
                f.counter = 0;
                f.list_depth += 1;
            }
            "a" => {
                f.link = attr("href").and_then(safe_href);
                f.link_from = self.cur.text.len();
                f.link_segments = self.segments.len();
            }
            _ => {}
        }
        if !f.hidden {
            match name {
                "li" => {
                    let list = self.stack.iter_mut().rev().find(|f| f.list.is_some());
                    let (marker, depth) = match list {
                        Some(l) if l.list == Some(true) => {
                            l.counter += 1;
                            (format!("{}. ", l.counter), l.list_depth)
                        }
                        Some(l) => ("• ".to_string(), l.list_depth),
                        None => ("• ".to_string(), 1),
                    };
                    self.pending_prefix =
                        Some(format!("{}{marker}", "  ".repeat(depth.saturating_sub(1))));
                }
                "td" | "th" => self.space_pending = true,
                "br" => self.line_break(),
                "hr" => {
                    self.text_plain("────────");
                    self.brk(2);
                }
                "img" => self.image(attrs),
                _ => {}
            }
            if self.fold.is_none() && opens_quote(name, attrs) {
                self.fold = Some(Fold {
                    segment: self.segments.len(),
                    offset: self.cur.text.len(),
                    depth: self.stack.len(),
                    closed: false,
                });
            }
        }
        let _ = was_hidden;
        if !void && self.stack.len() < MAX_DEPTH {
            self.stack.push(f);
        }
    }

    fn end(&mut self, name: &str) {
        let Some(at) = self.stack.iter().rposition(|f| f.name == name) else {
            return;
        };
        if at == 0 {
            return;
        }
        let frame = self.stack[at].clone();
        if !frame.hidden {
            if name == "a" {
                self.link_marker(&frame);
            }
            if name == "li" {
                self.pending_prefix = None;
            }
            self.brk(block_gap(name));
        }
        if let Some(fold) = self.fold.as_mut()
            && !fold.closed
            && at <= fold.depth
        {
            fold.closed = true;
        }
        self.stack.truncate(at);
    }

    /// After a link, names its host when the visible text does not.
    fn link_marker(&mut self, frame: &Frame) {
        let Some(target @ LinkTarget::Web(url)) = &frame.link else {
            return;
        };
        // An image or other break inside the link: the text is not in one piece.
        if self.segments.len() != frame.link_segments {
            return;
        }
        let shown = self.cur.text[frame.link_from.min(self.cur.text.len())..]
            .trim()
            .to_lowercase();
        let Some(host) = Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
        else {
            return;
        };
        let bare = host.strip_prefix("www.").unwrap_or(&host);
        if shown.is_empty() || shown.contains(bare) {
            return;
        }
        let _ = target;
        self.space_pending = false;
        self.append(&format!(" ({})", clip(&host, 60)), Some(frame));
    }

    fn line_break(&mut self) {
        if self.cur.text.is_empty() || self.cur.text.ends_with("\n\n") {
            return;
        }
        self.cur.text.push('\n');
        self.space_pending = false;
        self.pending_break = 0;
    }

    fn text(&mut self, s: &str) {
        if self.top().hidden {
            return;
        }
        let pre = self.top().pre;
        for c in s.chars() {
            if is_invisible(c) {
                continue;
            }
            if c == '\n' || c == '\r' || c == '\t' || c == ' ' || c == '\u{c}' {
                if pre {
                    match c {
                        '\n' => self.pre_newline(),
                        '\t' => self.push_char(' ', true),
                        ' ' => self.push_char(' ', true),
                        _ => {}
                    }
                } else {
                    self.space_pending = true;
                }
                continue;
            }
            if c.is_control() {
                continue;
            }
            self.push_char(c, false);
        }
    }

    fn pre_newline(&mut self) {
        if self.cur.text.is_empty() {
            return;
        }
        self.cur.text.push('\n');
        self.pending_break = 0;
        self.space_pending = false;
    }

    /// Appends one char, first applying any pending line breaks, space and line prefixes.
    fn push_char(&mut self, c: char, keep_space: bool) {
        if self.total >= MAX_TEXT {
            self.truncated = true;
            return;
        }
        let mut buf = [0u8; 4];
        let s: &str = c.encode_utf8(&mut buf);
        if c == ' ' && keep_space && self.cur.text.is_empty() {
            return;
        }
        self.begin_content();
        if self.space_pending && !self.at_line_start() {
            self.append_space();
        }
        self.space_pending = false;
        self.append(s, None);
    }

    fn text_plain(&mut self, s: &str) {
        self.begin_content();
        self.append(s, None);
    }

    fn at_line_start(&self) -> bool {
        self.cur.text.is_empty() || self.cur.text.ends_with('\n')
    }

    /// Emits the pending block gap and the quote and list prefixes before real content.
    fn begin_content(&mut self) {
        if self.pending_break > 0 {
            if !self.cur.text.is_empty() {
                let have = self
                    .cur
                    .text
                    .bytes()
                    .rev()
                    .take_while(|&b| b == b'\n')
                    .count();
                for _ in have..self.pending_break {
                    self.cur.text.push('\n');
                }
            }
            self.pending_break = 0;
            self.space_pending = false;
        }
        if self.at_line_start() {
            let depth = self.top().quote;
            if depth > 0 {
                let p = "> ".repeat(depth);
                self.append_unstyled(&p);
            }
            if let Some(p) = self.pending_prefix.take() {
                self.append_unstyled(&p);
            }
        }
        if self.fold.as_ref().is_some_and(|f| f.closed) {
            self.content_after_fold = true;
        }
    }

    /// Appends a space that takes the styles and link of the text before it only when the
    /// current element continues them, so a space between words never starts an underline.
    fn append_space(&mut self) {
        let len = self.cur.text.len();
        let top = self.top();
        let marks = match self.cur.marks.last() {
            Some((r, m)) if r.end == len && *m == top.marks => *m,
            _ => Marks::default(),
        };
        let link = match (self.cur.links.last(), &top.link) {
            (Some((r, t)), Some(now)) if r.end == len && t == now => Some(now.clone()),
            _ => None,
        };
        let frame = Frame {
            marks,
            link,
            ..Frame::root()
        };
        self.append(" ", Some(&frame));
    }

    fn append_unstyled(&mut self, s: &str) {
        self.total += s.len();
        self.cur.text.push_str(s);
    }

    /// Appends `s` with the current styles, or `frame`'s when given.
    fn append(&mut self, s: &str, frame: Option<&Frame>) {
        let (marks, link) = match frame {
            Some(f) => (f.marks, f.link.clone()),
            None => {
                let t = self.top();
                (t.marks, t.link.clone())
            }
        };
        let start = self.cur.text.len();
        self.cur.text.push_str(s);
        self.total += s.len();
        let end = self.cur.text.len();
        if !marks.is_empty() {
            match self.cur.marks.last_mut() {
                Some((r, m)) if r.end == start && *m == marks => r.end = end,
                _ => self.cur.marks.push((start..end, marks)),
            }
        }
        if let Some(target) = link {
            match self.cur.links.last_mut() {
                Some((r, t)) if r.end == start && *t == target => r.end = end,
                _ => self.cur.links.push((start..end, target)),
            }
        }
    }

    fn image(&mut self, attrs: &[(String, String)]) {
        let attr = |k: &str| attrs.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        let style = attr("style").unwrap_or_default();
        let width = dimension(attr("width")).or_else(|| css_dimension(style, "width"));
        let height = dimension(attr("height")).or_else(|| css_dimension(style, "height"));
        let alt = clean_inline(attr("alt").unwrap_or_default());
        // Tracking pixels are not offered at all.
        if width.is_some_and(|w| w <= 2) || height.is_some_and(|h| h <= 2) {
            return;
        }
        let src = attr("src").map(clean_url).unwrap_or_default();
        let kind = match Url::parse(&src) {
            Ok(u) if u.host_str().is_some_and(|h| !h.is_empty()) => match u.scheme() {
                "https" => Some((u.to_string(), ImageKind::Remote)),
                "http" => Some((u.to_string(), ImageKind::Insecure)),
                _ => None,
            },
            _ => None,
        };
        let Some((url, kind)) = kind.filter(|_| self.images < MAX_IMAGES) else {
            // `cid:` and `data:` images and over-limit ones: the alt text stands in.
            if !alt.is_empty() {
                self.space_pending = true;
                self.text(&format!("[{alt}]"));
            }
            return;
        };
        self.images += 1;
        self.finish_text();
        if self.fold.as_ref().is_some_and(|f| f.closed) {
            self.content_after_fold = true;
        }
        self.pending_break = 0;
        self.space_pending = false;
        self.segments.push(Segment::Image(RichImage {
            url,
            kind,
            alt,
            width,
            height,
            link: self.top().link.clone(),
        }));
    }

    /// Pushes the text built so far as a segment.
    fn finish_text(&mut self) {
        let mut t = std::mem::take(&mut self.cur);
        t.trim_end();
        if !t.text.is_empty() {
            self.segments.push(Segment::Text(t));
        }
    }

    fn finish(mut self) -> RichDoc {
        let fold = self.fold.take();
        let pushed_text = !self.cur.text.trim().is_empty();
        let cur_index = self.segments.len();
        self.finish_text();
        let mut segments = self.segments;
        let mut quote = None;
        if let Some(f) = fold
            && !self.content_after_fold
        {
            let (main, rest) = split_segments(segments.clone(), f.segment, f.offset);
            let _ = (pushed_text, cur_index);
            if main.iter().any(has_content) && !rest.is_empty() {
                segments = main;
                quote = Some(rest);
            }
        }
        RichDoc {
            main: segments,
            quote,
            truncated: self.truncated,
        }
    }
}

fn has_content(s: &Segment) -> bool {
    match s {
        Segment::Text(t) => !t.text.trim().is_empty(),
        Segment::Image(_) => true,
    }
}

/// Splits a segment list at (`segment`, `offset`): everything before, and everything from.
fn split_segments(
    mut segments: Vec<Segment>,
    segment: usize,
    offset: usize,
) -> (Vec<Segment>, Vec<Segment>) {
    if segment > segments.len() {
        return (segments, Vec::new());
    }
    let mut rest = segments.split_off(segment);
    if rest.is_empty() {
        return (segments, rest);
    }
    if offset > 0
        && let Segment::Text(t) = rest.remove(0)
    {
        if offset >= t.text.len() {
            let mut head = t;
            head.trim_end();
            if !head.text.is_empty() {
                segments.push(Segment::Text(head));
            }
        } else {
            let (mut head, mut tail) = t.split_at(offset);
            head.trim_end();
            tail.trim_start();
            if !head.text.is_empty() {
                segments.push(Segment::Text(head));
            }
            if !tail.text.is_empty() {
                rest.insert(0, Segment::Text(tail));
            }
        }
    } else if let Some(Segment::Text(t)) = rest.first_mut() {
        t.trim_start();
        if t.text.is_empty() {
            rest.remove(0);
        }
    }
    (segments, rest)
}

/// Gmail, Apple Mail, Thunderbird and Yahoo mark the quoted reply they append.
fn opens_quote(name: &str, attrs: &[(String, String)]) -> bool {
    let attr = |k: &str| attrs.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    let class = attr("class").unwrap_or_default().to_ascii_lowercase();
    class
        .split_ascii_whitespace()
        .any(|c| matches!(c, "gmail_quote" | "yahoo_quoted" | "moz-cite-prefix"))
        || (name == "blockquote" && attr("type").is_some_and(|t| t.eq_ignore_ascii_case("cite")))
}

// ----- small helpers ------------------------------------------------------------------------

/// A `style` attribute that hides its element.
fn style_hides(style: &str) -> bool {
    let s: String = style
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_ascii_lowercase();
    s.contains("display:none")
        || s.contains("visibility:hidden")
        || s.contains("mso-hide:all")
        || s.contains("opacity:0;")
        || s.ends_with("opacity:0")
        || zero_property(&s, "font-size")
        || zero_property(&s, "max-height")
}

/// `prop:0`, `prop:0px` and the like, as a whole declaration.
fn zero_property(style: &str, prop: &str) -> bool {
    style.split(';').any(|decl| {
        decl.strip_prefix(prop)
            .and_then(|v| v.strip_prefix(':'))
            .is_some_and(|v| {
                let v = v.trim_end_matches("!important");
                matches!(v, "0" | "0px" | "0pt" | "0em" | "0%")
            })
    })
}

/// A plain pixel size from an attribute (`600`, `600px`); percentages are not sizes.
fn dimension(v: Option<&str>) -> Option<u32> {
    let v = v?.trim().trim_end_matches("px");
    v.parse::<u32>().ok().filter(|&n| n <= 10_000)
}

/// A pixel size for `prop` from a `style` attribute.
fn css_dimension(style: &str, prop: &str) -> Option<u32> {
    style.split(';').find_map(|decl| {
        let (name, value) = decl.split_once(':')?;
        if name.trim().eq_ignore_ascii_case(prop) {
            let v = value.trim().to_ascii_lowercase();
            let v = v.trim_end_matches("!important").trim();
            dimension(Some(v.trim_end_matches("px")))
        } else {
            None
        }
    })
}

/// A link target from an `href`: only `http`, `https` and `mailto`.
fn safe_href(raw: &str) -> Option<LinkTarget> {
    let href = clean_url(raw);
    let lower = href.to_ascii_lowercase();
    if lower.starts_with("https://") || lower.starts_with("http://") {
        let url = Url::parse(&href).ok()?;
        url.host_str().filter(|h| !h.is_empty())?;
        return Some(LinkTarget::Web(url.to_string()));
    }
    if lower.starts_with("mailto:") && href.len() > "mailto:".len() {
        return Some(LinkTarget::Mail(href));
    }
    None
}

/// A URL with whitespace and controls (which browsers ignore inside a scheme) removed, and
/// anything over the length cap dropped.
fn clean_url(raw: &str) -> String {
    let s: String = raw
        .chars()
        .filter(|c| !c.is_control() && !c.is_whitespace())
        .collect();
    if s.len() > MAX_URL { String::new() } else { s }
}

/// Alt text and other inline strings: one line, without invisible or control characters.
fn clean_inline(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .filter(|&c| !is_invisible(c) && !c.is_control())
        .collect();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    clip(&collapsed, 200)
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}

/// Characters that show nothing but change how text reads: bidirectional overrides and
/// isolates (they reorder what the user sees), zero-width characters and soft hyphens (they pad
/// preheaders and split words past filters), and the byte order mark.
fn is_invisible(c: char) -> bool {
    matches!(
        c,
        '\u{ad}'
            | '\u{34f}'
            | '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{2069}'
            | '\u{feff}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(doc: &RichDoc) -> String {
        doc.main
            .iter()
            .filter_map(|s| match s {
                Segment::Text(t) => Some(t.text.as_str()),
                Segment::Image(_) => None,
            })
            .collect::<Vec<_>>()
            .join("|")
    }

    fn only_text(doc: &RichDoc) -> &RichText {
        match doc.main.as_slice() {
            [Segment::Text(t)] => t,
            other => panic!("expected one text segment, got {other:?}"),
        }
    }

    #[test]
    fn scripts_styles_and_hidden_content_never_reach_the_model() {
        let doc = sanitise(
            r#"<html><head><title>T</title><style>p{color:red}</style></head><body>
            <script>alert(1)</script><p onclick="x()">Hello <b>there</b></p>
            <div style="display: none">preheader</div>
            <span style="font-size:0px">tiny</span>
            <iframe src="https://evil.example/"></iframe><form><input value="x"></form>
            <svg><text>vector</text></svg>
            <p hidden>secret</p></body></html>"#,
        );
        assert_eq!(text_of(&doc), "Hello there");
        assert!(doc.images().next().is_none());
    }

    #[test]
    fn an_unclosed_head_does_not_hide_the_body() {
        let doc = sanitise("<html><head><meta charset=utf-8><body><p>Visible</p>");
        assert_eq!(text_of(&doc), "Visible");
    }

    #[test]
    fn whitespace_collapses_and_blocks_become_lines() {
        let doc = sanitise("<p>one   two\n three</p><p>next</p><div>a<br>b</div>");
        assert_eq!(only_text(&doc).text, "one two three\n\nnext\n\na\nb");
    }

    #[test]
    fn styles_become_marks_on_the_right_bytes() {
        let doc = sanitise("<p>a <b>bold <i>both</i></b> <code>m</code> z</p>");
        let t = only_text(&doc);
        assert_eq!(t.text, "a bold both m z");
        let marked = |m: Marks| -> Vec<&str> {
            t.marks
                .iter()
                .filter(|(_, x)| x.contains(m))
                .map(|(r, _)| &t.text[r.clone()])
                .collect()
        };
        assert_eq!(marked(Marks::BOLD), ["bold", "both"]);
        assert_eq!(marked(Marks::ITALIC), ["both"]);
        assert_eq!(marked(Marks::MONO), ["m"]);
    }

    #[test]
    fn lists_and_quotes_get_prefixes() {
        let doc = sanitise(
            "<ul><li>one</li><li>two</li></ul><ol><li>a</li><li>b</li></ol><blockquote>q</blockquote>",
        );
        assert_eq!(only_text(&doc).text, "• one\n• two\n\n1. a\n2. b\n\n> q");
    }

    #[test]
    fn only_web_and_mail_links_survive() {
        let doc = sanitise(
            r#"<a href="javascript:alert(1)">js</a> <a href="data:text/html,x">d</a>
            <a href="/relative">rel</a> <a href="  jav&#x09;ascript:1">tab</a>
            <a href="https://example.com/x">example.com</a>
            <a href="mailto:a@example.org">mail</a>"#,
        );
        let t = only_text(&doc);
        let targets: Vec<&LinkTarget> = t.links.iter().map(|(_, l)| l).collect();
        assert_eq!(
            targets,
            [
                &LinkTarget::Web("https://example.com/x".into()),
                &LinkTarget::Mail("mailto:a@example.org".into())
            ]
        );
    }

    #[test]
    fn a_link_that_hides_its_host_names_it() {
        let doc = sanitise(
            r#"<a href="https://evil.example/login">Your PayPal account</a>
            <a href="https://www.paypal.com/x">www.paypal.com</a>
            <a href="https://xn--pypal-4ve.com/">paypal.com</a>"#,
        );
        let t = only_text(&doc);
        assert!(
            t.text.contains("Your PayPal account (evil.example)"),
            "{}",
            t.text
        );
        assert!(t.text.contains("www.paypal.com "), "{}", t.text);
        assert!(!t.text.contains("www.paypal.com ("), "{}", t.text);
        // The marker is part of the clickable range.
        let (r, _) = &t.links[0];
        assert_eq!(&t.text[r.clone()], "Your PayPal account (evil.example)");
    }

    #[test]
    fn images_are_placeholders_and_tracking_pixels_vanish() {
        let doc = sanitise(
            r#"<p>Hi</p><img src="https://cdn.example/a.png" alt="Logo" width="600">
            <img src="https://track.example/p.gif" width="1" height="1">
            <img src="https://track.example/q.gif" style="width:1px;height:1px">
            <img src="https://track.example/h.gif" style="display:none">
            <img src="http://plain.example/b.png">
            <img src="cid:abc" alt="inline">"#,
        );
        let images: Vec<&RichImage> = doc.images().collect();
        assert_eq!(images.len(), 2);
        assert_eq!(images[0].url, "https://cdn.example/a.png");
        assert_eq!(images[0].kind, ImageKind::Remote);
        assert_eq!(images[0].alt, "Logo");
        assert_eq!(images[0].width, Some(600));
        assert_eq!(images[1].kind, ImageKind::Insecure);
        assert!(text_of(&doc).contains("[inline]"));
    }

    #[test]
    fn an_image_inside_a_link_keeps_the_link_target() {
        let doc = sanitise(
            r#"<a href="https://example.com/go"><img src="https://cdn.example/b.png"></a>"#,
        );
        let img = doc.images().next().expect("image");
        assert_eq!(
            img.link,
            Some(LinkTarget::Web("https://example.com/go".into()))
        );
    }

    #[test]
    fn bidi_and_zero_width_characters_are_removed() {
        let doc = sanitise("<p>a\u{202e}b\u{200b}c\u{2066}d&zwnj;e&shy;f</p>");
        assert_eq!(only_text(&doc).text, "abcdef");
    }

    #[test]
    fn a_trailing_quote_is_split_off() {
        let doc = sanitise(
            r#"<div>Thanks!</div><div class="gmail_quote"><div class="gmail_attr">On Tue, A wrote:</div>
            <blockquote class="gmail_quote">old text <b>here</b></blockquote></div>"#,
        );
        assert_eq!(only_text(&doc).text, "Thanks!");
        let quote = doc.quote.as_ref().expect("quote");
        let Segment::Text(q) = &quote[0] else {
            panic!("text")
        };
        assert!(q.text.starts_with("On Tue, A wrote:"), "{}", q.text);
        assert!(q.text.contains("> old text here"), "{}", q.text);
    }

    #[test]
    fn a_quote_followed_by_more_content_stays_inline() {
        let doc = sanitise(r#"<blockquote type="cite">asked</blockquote><p>my answer below</p>"#);
        assert!(doc.quote.is_none());
        assert!(only_text(&doc).text.contains("my answer below"));
        assert!(only_text(&doc).text.contains("asked"));
    }

    #[test]
    fn a_message_that_is_all_quote_is_not_folded() {
        let doc = sanitise(r#"<blockquote type="cite">forwarded</blockquote>"#);
        assert!(doc.quote.is_none());
        assert!(only_text(&doc).text.contains("forwarded"));
    }

    #[test]
    fn hostile_input_neither_panics_nor_escapes() {
        for html in [
            "",
            "<",
            "<<<<>>>>",
            "<a href=",
            "<a href=\"unterminated",
            "<!-- never closed",
            "<script>never closed",
            "<p style='display:none'><p>x</p></p> trailing",
            "</p></p></div>text",
            "<table><tr><td>é€𝄞</td></tr></table>",
            "<img src=\"https://\" width=abc>",
            "<a href=\"https://exa mple.com\">x</a>",
        ] {
            let _ = sanitise(html);
        }
        let deep = "<div>".repeat(5000) + "deep" + &"</div>".repeat(5000);
        assert_eq!(text_of(&sanitise(&deep)), "deep");
        let wide = "<img src=\"https://a.example/i.png\">".repeat(MAX_IMAGES + 50);
        assert_eq!(sanitise(&wide).images().count(), MAX_IMAGES);
    }

    #[test]
    fn oversized_input_is_cut_and_flagged() {
        let html = format!("<p>start</p>{}", "x".repeat(MAX_HTML_INPUT));
        let doc = sanitise(&html);
        assert!(doc.truncated);
        assert!(text_of(&doc).starts_with("start"));
    }

    #[test]
    fn table_cells_do_not_run_together() {
        let doc = sanitise(
            "<table><tr><td>Name</td><td>Qty</td></tr><tr><td>Pen</td><td>3</td></tr></table>",
        );
        assert_eq!(only_text(&doc).text, "Name Qty\nPen 3");
    }

    #[test]
    fn pre_keeps_its_lines_and_is_monospace() {
        let doc = sanitise("<pre>a\n  b</pre>");
        let t = only_text(&doc);
        assert_eq!(t.text, "a\n  b");
        assert!(t.marks.iter().all(|(_, m)| m.contains(Marks::MONO)));
    }
}
