//! Mail domain logic that needs no network or UI: base64, payload parsing, HTML to text,
//! link detection, `mailto:` parsing, addresses, local query matching, and the composer's
//! reply prefills and outgoing MIME, and the naming rules for saved attachments.

pub mod address;
pub mod b64;
pub mod compose;
pub mod files;
pub mod html;
pub mod links;
pub mod mailto;
pub mod parse;
pub mod query;
pub mod quote;
pub mod reply;

/// `s` with every CR and LF turned into a space, for one-line values such as a subject. A
/// decoded header or a `mailto:` subject can carry line breaks, and gpui's `shape_line`
/// asserts there are none.
pub fn one_line(s: &str) -> String {
    s.replace(['\r', '\n'], " ")
}

#[cfg(test)]
mod tests {
    #[test]
    fn one_line_flattens_line_breaks() {
        assert_eq!(super::one_line("a\r\nb\nc\rd"), "a  b c d");
        assert_eq!(super::one_line("plain"), "plain");
    }
}
