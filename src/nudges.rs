//! Nudges: old mail that wants a second look (openspec/specs/nudges).
//!
//! The Gmail API exposes no nudge label or flag: the web app works nudges out on the client.
//! This module does the same from what the summaries already carry, so it is an
//! approximation of Gmail's behaviour, not a copy of it:
//!
//! * **Reply**: the newest message is from someone else, in the inbox, and old enough.
//! * **Follow up**: the newest message is one the account owner sent, and nobody has answered.
//!
//! Automated senders and the Promotions, Social, Updates and Forums tabs never nudge. A
//! dismissal is kept per thread together with the newest message's date, so a new message
//! in the thread makes it eligible again.

use std::collections::HashMap;

use jiff::{SignedDuration, Timestamp};

use crate::model::{ThreadSummary, labels};

/// Mail newer than this is still fresh.
pub const MIN_AGE: SignedDuration = SignedDuration::from_hours(3 * 24);
/// Mail older than this has gone past nudging.
pub const MAX_AGE: SignedDuration = SignedDuration::from_hours(21 * 24);

/// The Gmail search that finds nudge candidates; [`classify`] then narrows the result.
pub const QUERY: &str = "((in:inbox category:primary) OR in:sent) older_than:3d newer_than:21d";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Nudge {
    /// Someone wrote and the account owner has not answered.
    Reply,
    /// The account owner wrote last and nobody has answered.
    FollowUp,
}

/// A dismissal per thread id: the newest message's date (ms) when it was dismissed.
pub type Dismissed = HashMap<String, i64>;

const CATEGORIES: [&str; 4] = [
    "CATEGORY_PROMOTIONS",
    "CATEGORY_SOCIAL",
    "CATEGORY_UPDATES",
    "CATEGORY_FORUMS",
];

/// Local parts that mark mail nobody reads replies to.
const AUTOMATED: [&str; 8] = [
    "noreply",
    "no-reply",
    "donotreply",
    "do-not-reply",
    "notifications",
    "notification",
    "mailer-daemon",
    "bounce",
];

fn automated(email: &str) -> bool {
    let local = email.split('@').next().unwrap_or("").to_ascii_lowercase();
    AUTOMATED.iter().any(|a| local.starts_with(a)) || local.contains("+noreply")
}

/// What kind of nudge `s` is at `now`, if any.
pub fn classify(s: &ThreadSummary, now: Timestamp, dismissed: &Dismissed) -> Option<Nudge> {
    let last = s.last.as_ref()?;
    let age = now.duration_since(s.last_date);
    if age < MIN_AGE || age > MAX_AGE {
        return None;
    }
    if dismissed.get(&s.key.id) == Some(&s.last_date.as_millisecond()) {
        return None;
    }
    if s.in_label(labels::TRASH) || s.in_label(labels::SPAM) || s.in_label(labels::DRAFT) {
        return None;
    }
    if CATEGORIES.iter().any(|c| s.in_label(c)) {
        return None;
    }
    if last.from_me {
        // Needs someone to have been written to, which `SENT` on a draft-less thread implies;
        // a thread of notes to self is the owner's, not a pending answer.
        let others = s.participants.iter().any(|(a, _)| !is_me(s, &a.email));
        return (s.in_label(labels::SENT) && others).then_some(Nudge::FollowUp);
    }
    if !s.in_label(labels::INBOX) || automated(&last.sender.email) {
        return None;
    }
    Some(Nudge::Reply)
}

/// Whether `email` is the account itself.
fn is_me(s: &ThreadSummary, email: &str) -> bool {
    s.key.account.eq_ignore_ascii_case(email)
}

/// "5 days ago" style text for the row tag.
pub fn age_text(s: &ThreadSummary, now: Timestamp) -> String {
    let days = (now.duration_since(s.last_date).as_hours() / 24).max(1);
    format!("{days} days ago")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Address, LastMessage, ThreadKey};
    use std::collections::BTreeSet;

    const ME: &str = "me@example.com";

    fn now() -> Timestamp {
        Timestamp::from_second(1_800_000_000).unwrap()
    }

    fn thread(days: i64, labels: &[&str], sender: &str, from_me: bool) -> ThreadSummary {
        ThreadSummary {
            key: ThreadKey::new(ME, "t1"),
            history_id: 1,
            subject: "Hello".into(),
            snippet: String::new(),
            participants: vec![
                (Address::new(None, "friend@example.org"), false),
                (Address::new(None, ME), false),
            ],
            last_date: now() - SignedDuration::from_hours(days * 24),
            message_count: 1,
            label_ids: labels
                .iter()
                .map(|l| l.to_string())
                .collect::<BTreeSet<_>>(),
            has_attachments: false,
            last: Some(LastMessage {
                sender: Address::new(None, sender),
                from_me,
            }),
        }
    }

    fn kind(s: &ThreadSummary) -> Option<Nudge> {
        classify(s, now(), &Dismissed::new())
    }

    #[test]
    fn old_unanswered_inbox_mail_is_a_reply_nudge() {
        let s = thread(5, &["INBOX"], "friend@example.org", false);
        assert_eq!(kind(&s), Some(Nudge::Reply));
    }

    #[test]
    fn sent_mail_nobody_answered_is_a_follow_up() {
        let s = thread(5, &["SENT"], ME, true);
        assert_eq!(kind(&s), Some(Nudge::FollowUp));
        let alone = ThreadSummary {
            participants: vec![(Address::new(None, ME), false)],
            ..thread(5, &["SENT"], ME, true)
        };
        assert_eq!(kind(&alone), None);
    }

    #[test]
    fn age_window_applies() {
        assert_eq!(kind(&thread(2, &["INBOX"], "a@b.c", false)), None);
        assert_eq!(
            kind(&thread(3, &["INBOX"], "a@b.c", false)),
            Some(Nudge::Reply)
        );
        assert_eq!(
            kind(&thread(21, &["INBOX"], "a@b.c", false)),
            Some(Nudge::Reply)
        );
        assert_eq!(kind(&thread(22, &["INBOX"], "a@b.c", false)), None);
    }

    #[test]
    fn bulk_automated_and_unknown_never_nudge() {
        for l in ["CATEGORY_PROMOTIONS", "CATEGORY_UPDATES", "TRASH", "SPAM"] {
            assert_eq!(kind(&thread(5, &["INBOX", l], "a@b.c", false)), None, "{l}");
        }
        assert_eq!(kind(&thread(5, &["INBOX"], "noreply@x.com", false)), None);
        assert_eq!(kind(&thread(5, &["INBOX"], "No-Reply@x.com", false)), None);
        let mut unknown = thread(5, &["INBOX"], "a@b.c", false);
        unknown.last = None;
        assert_eq!(kind(&unknown), None);
        // Archived: nothing is waiting in the inbox.
        assert_eq!(kind(&thread(5, &[], "a@b.c", false)), None);
    }

    #[test]
    fn dismissal_holds_until_a_new_message_arrives() {
        let s = thread(5, &["INBOX"], "a@b.c", false);
        let mut d = Dismissed::new();
        d.insert("t1".into(), s.last_date.as_millisecond());
        assert_eq!(classify(&s, now(), &d), None);
        let newer = ThreadSummary {
            last_date: s.last_date + SignedDuration::from_secs(60),
            ..s
        };
        assert_eq!(classify(&newer, now(), &d), Some(Nudge::Reply));
    }

    #[test]
    fn age_text_counts_days() {
        assert_eq!(
            age_text(&thread(5, &["INBOX"], "a@b.c", false), now()),
            "5 days ago"
        );
    }
}
