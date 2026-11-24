//! Signatures: Gmail keeps them as HTML per send-as address; the composer is plain text, so
//! the HTML is converted once and placed under the "-- " delimiter (RFC 3676) the way every
//! plain-text client does. The block is swapped when the user changes From.

use crate::model::ComposeKind;

use super::html;

/// The delimiter line: two dashes and a space, so clients can strip the signature on reply.
const DELIMITER: &str = "-- \n";

/// A signature longer than this is cut: it is a signature, not a document.
const MAX_LEN: usize = 4000;

/// Gmail's HTML signature as trimmed plain text; empty when it has no text.
pub fn from_html(raw: &str) -> String {
    if raw.trim().is_empty() {
        return String::new();
    }
    let text = html::html_to_text(raw).map_or_else(|| html::decode_entities(raw), |c| c.text);
    let text = text.replace("\r\n", "\n");
    let trimmed = text
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n");
    let trimmed = trimmed.trim();
    match trimmed.char_indices().nth(MAX_LEN) {
        Some((cut, _)) => trimmed[..cut].trim_end().to_string(),
        None => trimmed.to_string(),
    }
}

fn block(signature: &str) -> String {
    format!("{DELIMITER}{signature}")
}

/// `body` with `signature` added. A reply or forward opens with two empty lines and the quoted
/// text: the signature goes between them, so the cursor stays on top and the quote stays
/// below. Anything else gets it at the end. An empty signature changes nothing.
pub fn insert(body: &str, signature: &str, kind: ComposeKind) -> String {
    if signature.is_empty() {
        return body.to_string();
    }
    let sig = block(signature);
    let quoted = !matches!(kind, ComposeKind::New);
    match body.strip_prefix("\n\n") {
        Some(rest) if quoted && !rest.is_empty() => format!("\n\n{sig}\n\n{rest}"),
        _ if body.trim().is_empty() => format!("\n\n{sig}"),
        _ => format!("{}\n\n{sig}", body.trim_end()),
    }
}

/// Swaps the signature block of `old` for `new` after From changes. A block the user edited
/// no longer matches and is left alone; when there is none to swap, `new` is added only if
/// `old` was empty, since otherwise the user removed it on purpose.
pub fn swap(body: &str, old: &str, new: &str, kind: ComposeKind) -> String {
    if old == new {
        return body.to_string();
    }
    if old.is_empty() {
        return if body.contains(DELIMITER) && !new.is_empty() {
            body.to_string()
        } else {
            insert(body, new, kind)
        };
    }
    let old_block = block(old);
    let Some(at) = body.find(&old_block) else {
        return body.to_string();
    };
    if new.is_empty() {
        // Drop the block and the blank lines that set it apart.
        let before = body[..at].trim_end_matches('\n');
        let after = body[at + old_block.len()..].trim_start_matches('\n');
        return match (before.is_empty(), after.is_empty()) {
            (_, true) => before.to_string(),
            (true, false) => format!("\n\n{after}"),
            (false, false) => format!("{before}\n\n{after}"),
        };
    }
    format!(
        "{}{}{}",
        &body[..at],
        block(new),
        &body[at + old_block.len()..]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_becomes_trimmed_text() {
        let s = from_html("<div>Ann Lee<br>Engineer</div><div><br></div>");
        assert_eq!(s, "Ann Lee\nEngineer");
        assert_eq!(from_html("  "), "");
    }

    #[test]
    fn new_message_gets_the_block_at_the_end() {
        assert_eq!(insert("", "Ann", ComposeKind::New), "\n\n-- \nAnn");
        assert_eq!(insert("Hi", "Ann", ComposeKind::New), "Hi\n\n-- \nAnn");
        assert_eq!(insert("Hi", "", ComposeKind::New), "Hi");
    }

    #[test]
    fn reply_gets_the_block_above_the_quote() {
        let body = "\n\nOn Mon, Bob wrote:\n> hi";
        assert_eq!(
            insert(body, "Ann", ComposeKind::Reply),
            "\n\n-- \nAnn\n\nOn Mon, Bob wrote:\n> hi"
        );
    }

    #[test]
    fn swapping_replaces_removes_and_adds() {
        let body = insert("Hi", "Ann", ComposeKind::New);
        assert_eq!(
            swap(&body, "Ann", "Ann (work)", ComposeKind::New),
            "Hi\n\n-- \nAnn (work)"
        );
        assert_eq!(swap(&body, "Ann", "", ComposeKind::New), "Hi");
        assert_eq!(swap("Hi", "", "Ann", ComposeKind::New), "Hi\n\n-- \nAnn");
    }

    #[test]
    fn edited_or_removed_blocks_are_left_alone() {
        assert_eq!(swap("Hi", "Ann", "Bo", ComposeKind::New), "Hi");
        assert_eq!(
            swap("Hi\n\n-- \nMine", "", "Bo", ComposeKind::New),
            "Hi\n\n-- \nMine"
        );
    }

    #[test]
    fn swapping_to_nothing_keeps_a_reply_quote() {
        let body = insert("\n\nOn Mon, Bob wrote:\n> hi", "Ann", ComposeKind::Reply);
        assert_eq!(
            swap(&body, "Ann", "", ComposeKind::Reply),
            "\n\nOn Mon, Bob wrote:\n> hi"
        );
    }
}
