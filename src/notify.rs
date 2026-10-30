//! Desktop notifications for new mail.
//!
//! The worker reports new unread INBOX threads with `SyncEvent::NewMail` (never on an
//! account's first sync, SPEC 7.3 and 7.4), the store queues them while notifications are on,
//! and `main` drains the queue into [`new_mail_notes`] and shows each [`Note`] with [`show`]
//! on a background task, because notify-rust blocks on D-Bus.
//!
//! Everything but [`show`] is pure. The body of a freedesktop notification may be read as
//! markup, so mail text in it is escaped; the summary is plain text by the specification, so
//! it is only cleaned. Both lose control and bidirectional-override characters, so a subject
//! cannot reorder or hide the rest of the text.

use std::collections::HashMap;

use jiff::Timestamp;

use crate::branding::APP_ID;
use crate::mail::address::display_name;
use crate::model::{ThreadKey, ThreadSummary, labels};

/// More new threads than this at once become one summary note.
const GROUP_ABOVE: usize = 3;
/// The snippet in a note's body is cut to this many characters.
const SNIPPET_CHARS: usize = 140;
/// The seen set is cleared once it holds this many threads, so it cannot grow without bound.
const SEEN_CAP: usize = 5_000;

/// One desktop notification: `summary` is plain text, `body` is escaped markup.
#[derive(Clone, PartialEq, Eq)]
pub struct Note {
    pub summary: String,
    pub body: String,
}

/// Threads already notified, by their newest message's date. A thread notifies again only when
/// a newer message arrives in it.
pub type Seen = HashMap<ThreadKey, Timestamp>;

/// The notes for `threads`, a `NewMail` delivery. Threads that are no longer unread in the
/// inbox, or were notified before at the same or a later date, are skipped; the rest are
/// remembered in `seen`. Up to [`GROUP_ABOVE`] threads give one note each, more give a single
/// summary note.
pub fn new_mail_notes(threads: &[ThreadSummary], seen: &mut Seen) -> Vec<Note> {
    if seen.len() > SEEN_CAP {
        seen.clear();
    }
    let mut fresh: Vec<&ThreadSummary> = Vec::new();
    for t in threads {
        if !t.unread() || !t.in_label(labels::INBOX) {
            continue;
        }
        if seen.get(&t.key).is_some_and(|&at| at >= t.last_date) {
            continue;
        }
        if fresh.iter().any(|f| f.key == t.key) {
            continue;
        }
        seen.insert(t.key.clone(), t.last_date);
        fresh.push(t);
    }
    // Newest first, as the inbox lists them.
    fresh.sort_by_key(|t| std::cmp::Reverse(t.last_date));
    if fresh.len() > GROUP_ABOVE {
        return vec![group_note(&fresh)];
    }
    fresh.into_iter().map(thread_note).collect()
}

/// The person a new thread is from: its last participant with an unread message, else its
/// last participant.
fn sender(t: &ThreadSummary) -> String {
    let who = t
        .participants
        .iter()
        .rev()
        .find(|(_, unread)| *unread)
        .or_else(|| t.participants.last())
        .map(|(a, _)| clean(display_name(a)));
    match who {
        Some(name) if !name.trim().is_empty() => name,
        _ => "New message".to_string(),
    }
}

fn thread_note(t: &ThreadSummary) -> Note {
    let subject = clean(&t.subject);
    let subject = if subject.trim().is_empty() {
        "(no subject)".to_string()
    } else {
        subject
    };
    let snippet = shorten(&collapse(&clean(&t.snippet)), SNIPPET_CHARS);
    let mut body = escape(&subject);
    if !snippet.is_empty() {
        body.push('\n');
        body.push_str(&escape(&snippet));
    }
    Note {
        summary: sender(t),
        body,
    }
}

fn group_note(threads: &[&ThreadSummary]) -> Note {
    let mut names: Vec<String> = Vec::new();
    for t in threads {
        let name = sender(t);
        if !names.contains(&name) {
            names.push(name);
        }
    }
    let shown = names.len().min(GROUP_ABOVE);
    let rest = names.len() - shown;
    let mut from = names[..shown].join(", ");
    match rest {
        0 => {}
        1 => from.push_str(" and 1 other"),
        n => from.push_str(&format!(" and {n} others")),
    }
    Note {
        summary: format!("{} new conversations", threads.len()),
        body: escape(&format!("From {from}")),
    }
}

/// True for the characters that reorder or hide text: the bidirectional embeddings,
/// overrides and isolates, and the directional marks.
fn is_bidi_control(c: char) -> bool {
    matches!(
        c,
        '\u{200E}' | '\u{200F}' | '\u{061C}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
    )
}

/// Drops control and bidi characters; line breaks and tabs become spaces.
fn clean(s: &str) -> String {
    s.chars()
        .filter_map(|c| match c {
            '\n' | '\r' | '\t' => Some(' '),
            c if c.is_control() || is_bidi_control(c) => None,
            c => Some(c),
        })
        .collect::<String>()
        .trim()
        .to_string()
}

/// Collapses runs of whitespace into one space.
fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Cuts `s` to at most `max` characters, ending with an ellipsis when cut.
fn shorten(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.truncate(out.trim_end().len());
    out.push('…');
    out
}

/// Escapes text for a notification body, which servers may read as a small markup subset.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c => out.push(c),
        }
    }
    out
}

/// Shows `note` on the desktop. Blocks on D-Bus, so it is only called from a background task.
pub fn show(note: Note) {
    use notify_rust::{Hint, Notification, Timeout};
    let mut n = Notification::new();
    n.appname("Eloquawk")
        .summary(&note.summary)
        .body(&note.body)
        .icon(APP_ID)
        .hint(Hint::Category("email.arrived".into()))
        .hint(Hint::DesktopEntry(APP_ID.into()))
        .timeout(Timeout::Default);
    if let Err(e) = n.show() {
        eprintln!("eloquawk: could not show a notification: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Address;

    fn thread(n: i64, from: &str, subject: &str, snippet: &str) -> ThreadSummary {
        ThreadSummary {
            key: ThreadKey::new("you@example.com", format!("t{n}")),
            history_id: 1,
            subject: subject.into(),
            snippet: snippet.into(),
            participants: vec![
                (Address::new(Some("You"), "you@example.com"), false),
                (Address::new(Some(from), "someone@example.org"), true),
            ],
            last_date: Timestamp::from_second(1_800_000_000 + n).unwrap(),
            message_count: 2,
            label_ids: ["INBOX", "UNREAD"].iter().map(|s| s.to_string()).collect(),
            has_attachments: false,
        }
    }

    #[test]
    fn one_note_per_thread_with_sender_subject_and_snippet() {
        let mut seen = Seen::new();
        let notes = new_mail_notes(
            &[thread(
                1,
                "Zoë Ångström",
                "Café ☕ plans",
                "Shall we\n  meet at ten?",
            )],
            &mut seen,
        );
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].summary, "Zoë Ångström");
        assert_eq!(notes[0].body, "Café ☕ plans\nShall we meet at ten?");
    }

    #[test]
    fn the_seen_set_notifies_a_thread_once_per_new_message() {
        let mut seen = Seen::new();
        let t = thread(1, "Ann", "Hello", "");
        assert_eq!(new_mail_notes(std::slice::from_ref(&t), &mut seen).len(), 1);
        // The same delivery again (a retried round) stays quiet.
        assert!(new_mail_notes(std::slice::from_ref(&t), &mut seen).is_empty());
        // A duplicate within one delivery notifies once.
        let other = thread(2, "Bob", "Hi", "");
        assert_eq!(
            new_mail_notes(&[other.clone(), other.clone()], &mut seen).len(),
            1
        );
        // A newer message in the same thread notifies again.
        let mut newer = t.clone();
        newer.last_date = Timestamp::from_second(1_800_000_100).unwrap();
        assert_eq!(new_mail_notes(&[newer], &mut seen).len(), 1);
    }

    #[test]
    fn threads_read_or_out_of_the_inbox_by_now_are_skipped() {
        // The first sync never emits NewMail (the worker's job, SPEC 7.3); what reaches the
        // notifier is only ever re-checked against the summary it carries.
        let mut seen = Seen::new();
        let mut read = thread(1, "Ann", "Read", "");
        read.label_ids.remove("UNREAD");
        let mut archived = thread(2, "Bob", "Archived", "");
        archived.label_ids.remove("INBOX");
        assert!(new_mail_notes(&[read, archived], &mut seen).is_empty());
        assert!(seen.is_empty(), "skipped threads are not remembered");
        assert!(new_mail_notes(&[], &mut seen).is_empty());
    }

    #[test]
    fn more_than_three_threads_become_one_summary_note() {
        let mut seen = Seen::new();
        let three: Vec<_> = (0..3)
            .map(|n| thread(n, &format!("P{n}"), "S", ""))
            .collect();
        assert_eq!(new_mail_notes(&three, &mut seen).len(), 3);

        let five: Vec<_> = (10..15)
            .map(|n| thread(n, &format!("P{n}"), "S", ""))
            .collect();
        let notes = new_mail_notes(&five, &mut seen);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].summary, "5 new conversations");
        assert_eq!(notes[0].body, "From P14, P13, P12 and 2 others");

        let mut four: Vec<_> = (20..24).map(|n| thread(n, "Ann", "S", "")).collect();
        four[0].participants[1].0 = Address::new(Some("Bob"), "bob@example.org");
        let notes = new_mail_notes(&four, &mut seen);
        assert_eq!(notes[0].summary, "4 new conversations");
        assert_eq!(notes[0].body, "From Ann, Bob");
    }

    #[test]
    fn markup_in_mail_is_escaped() {
        let mut seen = Seen::new();
        let notes = new_mail_notes(
            &[thread(
                1,
                "Eve <eve@example.org>",
                "<img src=x> & you",
                "<a href=\"https://evil.example\">click</a> & more",
            )],
            &mut seen,
        );
        assert_eq!(notes[0].summary, "Eve <eve@example.org>");
        assert_eq!(
            notes[0].body,
            "&lt;img src=x&gt; &amp; you\n&lt;a href=\"https://evil.example\"&gt;click&lt;/a&gt; &amp; more"
        );
        assert!(!notes[0].body.contains('<'));

        let five: Vec<_> = (10..15)
            .map(|n| thread(n, &format!("<b>{n}</b>"), "S", ""))
            .collect();
        let group = new_mail_notes(&five, &mut seen);
        assert!(!group[0].body.contains('<'));
        assert!(group[0].body.contains("&lt;b&gt;14&lt;/b&gt;"));
    }

    #[test]
    fn control_and_bidi_characters_are_stripped() {
        let mut seen = Seen::new();
        let notes = new_mail_notes(
            &[thread(
                1,
                "Mal\u{202E}lory\u{0007}",
                "Invoice\u{2066} fdp.exe\u{2069}\u{200F}",
                "line\u{0000}one\r\nline two\u{202D}",
            )],
            &mut seen,
        );
        assert_eq!(notes[0].summary, "Mallory");
        assert_eq!(notes[0].body, "Invoice fdp.exe\nlineone line two");
        for c in notes[0].summary.chars().chain(notes[0].body.chars()) {
            assert!(c == '\n' || !c.is_control(), "{c:?}");
            assert!(!is_bidi_control(c), "{c:?}");
        }
    }

    #[test]
    fn empty_subject_and_long_snippet() {
        let mut seen = Seen::new();
        let long = "word ".repeat(100);
        let notes = new_mail_notes(&[thread(1, "Ann", "  ", &long)], &mut seen);
        let (subject, snippet) = notes[0].body.split_once('\n').unwrap();
        assert_eq!(subject, "(no subject)");
        assert!(snippet.ends_with('…'));
        assert!(snippet.chars().count() <= SNIPPET_CHARS);
    }

    #[test]
    fn a_thread_without_participants_still_has_a_summary() {
        let mut seen = Seen::new();
        let mut t = thread(1, "Ann", "Hi", "");
        t.participants.clear();
        assert_eq!(new_mail_notes(&[t], &mut seen)[0].summary, "New message");
    }
}
