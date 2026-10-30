//! Quoted-reply folding shared by the reader and the AI prompt builder: a body's trailing
//! quote (a final block of `>` lines with its "On … wrote:" attribution) is split from what
//! the sender wrote this time. Pure, so non-UI modules can use it (SPEC 16.3).

/// Splits a body into what is shown and a trailing quote that folds behind "• • •": a final
/// block of `>` lines (blank lines allowed inside), with an "On … wrote:" line just above it
/// included. Nothing folds when the quote would be all there is.
pub fn split_quote(body: &str) -> (&str, Option<&str>) {
    // Byte offsets of each line start, with the line.
    let mut lines: Vec<(usize, &str)> = Vec::new();
    let mut at = 0;
    for line in body.split_inclusive('\n') {
        lines.push((at, line));
        at += line.len();
    }
    let is_blank = |l: &str| l.trim().is_empty();
    let is_quote = |l: &str| l.trim_start().starts_with('>');
    let mut i = lines.len();
    while i > 0 && is_blank(lines[i - 1].1) {
        i -= 1;
    }
    let mut start = None;
    while i > 0 && (is_quote(lines[i - 1].1) || is_blank(lines[i - 1].1)) {
        i -= 1;
        if is_quote(lines[i].1) {
            start = Some(i);
        }
    }
    let Some(mut start) = start else {
        return (body, None);
    };
    // An attribution line ("On Mon, Zoë wrote:"), possibly after blank lines.
    let mut j = start;
    while j > 0 && is_blank(lines[j - 1].1) {
        j -= 1;
    }
    if j > 0 {
        let l = lines[j - 1].1.trim();
        if l.starts_with("On ") && l.ends_with("wrote:") {
            start = j - 1;
        }
    }
    let cut = lines[start].0;
    let shown = body[..cut].trim_end();
    if shown.is_empty() {
        return (body, None);
    }
    (shown, Some(body[cut..].trim_end()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trailing_quote_folds() {
        let body = "Sounds good.\n\n> earlier\n> more\n";
        assert_eq!(
            split_quote(body),
            ("Sounds good.", Some("> earlier\n> more"))
        );
    }

    #[test]
    fn attribution_line_folds_with_the_quote() {
        let body = "Yes!\n\nOn Mon, 3 Oct 2026, Zoë wrote:\n> Lunch?\n>\n> > nested\n\n";
        assert_eq!(
            split_quote(body),
            (
                "Yes!",
                Some("On Mon, 3 Oct 2026, Zoë wrote:\n> Lunch?\n>\n> > nested")
            )
        );
    }

    #[test]
    fn inline_quotes_and_quote_only_bodies_do_not_fold() {
        let inline = "> question\nanswer\n";
        assert_eq!(split_quote(inline), (inline, None));
        let only = "> all quote\n> here";
        assert_eq!(split_quote(only), (only, None));
        assert_eq!(split_quote("plain"), ("plain", None));
        assert_eq!(split_quote(""), ("", None));
    }
}
