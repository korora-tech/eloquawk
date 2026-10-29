//! A subset of Gmail's search syntax, parsed and matched locally (SPEC 9, "Search behaviour").
//!
//! The Google worker sends queries to Gmail verbatim; this module serves demo mode and the
//! store's optimistic list membership. Parsing never fails: anything not understood is
//! matched as free text. Like Gmail, threads in Spam and Trash only match when the query asks
//! for them (`in:spam`, `in:trash`, `in:anywhere` or the matching `label:`).

use jiff::civil::Date;
use jiff::{Timestamp, ToSpan, Zoned};

use crate::model::{Label, ThreadDetail, ThreadSummary, labels};

use super::address::display_name;

/// A parsed query: every group must match; a group matches when any of its terms does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Query {
    groups: Vec<Vec<Term>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Term {
    negated: bool,
    kind: Kind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Kind {
    Text(String),
    From(String),
    To(String),
    Cc(String),
    Subject(String),
    Label(String),
    In(In),
    Is(Is),
    HasAttachment,
    NewerThan(Age),
    OlderThan(Age),
    After(Date),
    Before(Date),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum In {
    Inbox,
    Sent,
    Trash,
    Spam,
    Anywhere,
    Starred,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Is {
    Unread,
    Read,
    Starred,
    Important,
}

/// `newer_than:` / `older_than:` amount: days, months or years.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Age {
    Days(i64),
    Months(i64),
    Years(i64),
}

/// One raw token: negation, the value and whether it was a quoted phrase.
struct Token {
    negated: bool,
    text: String,
    quoted: bool,
}

fn tokenize(input: &str) -> Vec<Token> {
    let chars: Vec<char> = input.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_whitespace() {
            i += 1;
            continue;
        }
        let negated = chars[i] == '-' && chars.get(i + 1).is_some_and(|c| !c.is_whitespace());
        if negated {
            i += 1;
        }
        let mut text = String::new();
        let mut quoted = false;
        if chars[i] == '"' {
            quoted = true;
            i += 1;
            while i < chars.len() && chars[i] != '"' {
                text.push(chars[i]);
                i += 1;
            }
            i += 1; // closing quote, if any
        } else {
            while i < chars.len() && !chars[i].is_whitespace() {
                // `op:"quoted value"`
                if chars[i] == '"' && text.ends_with(':') {
                    i += 1;
                    while i < chars.len() && chars[i] != '"' {
                        text.push(chars[i]);
                        i += 1;
                    }
                    i += 1;
                    break;
                }
                text.push(chars[i]);
                i += 1;
            }
        }
        out.push(Token {
            negated,
            text,
            quoted,
        });
    }
    out
}

fn parse_age(v: &str) -> Option<Age> {
    // Split on the last character, not the last byte: the unit may be multi-byte input.
    let unit = v.chars().next_back()?;
    let num = &v[..v.len() - unit.len_utf8()];
    let n: i64 = num.parse().ok().filter(|n| *n >= 0 && *n <= 10_000)?;
    match unit {
        'd' => Some(Age::Days(n)),
        'm' => Some(Age::Months(n)),
        'y' => Some(Age::Years(n)),
        _ => None,
    }
}

fn parse_date(v: &str) -> Option<Date> {
    let mut it = v.split(['/', '-']);
    let (y, m, d) = (it.next()?, it.next()?, it.next()?);
    if it.next().is_some() {
        return None;
    }
    Date::new(y.parse().ok()?, m.parse().ok()?, d.parse().ok()?).ok()
}

fn kind_of(token: &Token) -> Kind {
    let lower = token.text.to_lowercase();
    if token.quoted {
        return Kind::Text(lower);
    }
    let Some((op, value)) = lower.split_once(':') else {
        return Kind::Text(lower);
    };
    if value.is_empty() {
        return Kind::Text(lower);
    }
    let v = value.to_string();
    let parsed = match op {
        "from" => Some(Kind::From(v)),
        "to" => Some(Kind::To(v)),
        "cc" => Some(Kind::Cc(v)),
        "subject" => Some(Kind::Subject(v)),
        "label" => Some(Kind::Label(v)),
        "in" => match value {
            "inbox" => Some(Kind::In(In::Inbox)),
            "sent" => Some(Kind::In(In::Sent)),
            "trash" => Some(Kind::In(In::Trash)),
            "spam" => Some(Kind::In(In::Spam)),
            "anywhere" => Some(Kind::In(In::Anywhere)),
            "starred" => Some(Kind::In(In::Starred)),
            _ => None,
        },
        "is" => match value {
            "unread" => Some(Kind::Is(Is::Unread)),
            "read" => Some(Kind::Is(Is::Read)),
            "starred" => Some(Kind::Is(Is::Starred)),
            "important" => Some(Kind::Is(Is::Important)),
            _ => None,
        },
        "has" if value == "attachment" => Some(Kind::HasAttachment),
        "newer_than" => parse_age(value).map(Kind::NewerThan),
        "older_than" => parse_age(value).map(Kind::OlderThan),
        "after" => parse_date(value).map(Kind::After),
        "before" => parse_date(value).map(Kind::Before),
        _ => None,
    };
    parsed.unwrap_or(Kind::Text(lower))
}

impl Query {
    /// Parses a query. Never fails; unknown operators become free text.
    pub fn parse(input: &str) -> Query {
        let tokens = tokenize(input);
        let mut groups: Vec<Vec<Term>> = Vec::new();
        let mut join_next = false;
        for (i, tok) in tokens.iter().enumerate() {
            let is_or = !tok.quoted && !tok.negated && tok.text == "OR";
            // `OR` joins its neighbours; repeated `OR`s collapse into one, and a leading or
            // trailing `OR` is free text.
            if is_or && !groups.is_empty() && i + 1 < tokens.len() {
                join_next = true;
                continue;
            }
            let term = Term {
                negated: tok.negated,
                kind: kind_of(tok),
            };
            match (join_next, groups.last_mut()) {
                (true, Some(group)) => group.push(term),
                _ => groups.push(vec![term]),
            }
            join_next = false;
        }
        Query { groups }
    }

    /// True for an empty query (nothing but whitespace).
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    /// Whether the query asks for Spam or Trash, which are otherwise excluded.
    fn wants_spam_or_trash(&self, all_labels: &[Label]) -> bool {
        self.groups.iter().flatten().any(|t| {
            !t.negated
                && match &t.kind {
                    Kind::In(In::Trash | In::Spam | In::Anywhere) => true,
                    Kind::Label(name) => resolve_labels(name, all_labels)
                        .iter()
                        .any(|id| id == labels::TRASH || id == labels::SPAM),
                    _ => false,
                }
        })
    }

    /// Matches a thread. `detail`, when loaded, lets body text, To and Cc match; `labels` are
    /// the account's labels, for `label:` by name; `now` (with its time zone) anchors
    /// `newer_than:`, `older_than:`, `after:` and `before:`.
    pub fn matches(
        &self,
        summary: &ThreadSummary,
        detail: Option<&ThreadDetail>,
        all_labels: &[Label],
        now: &Zoned,
    ) -> bool {
        let hidden = summary.in_label(labels::SPAM) || summary.in_label(labels::TRASH);
        if hidden && !self.wants_spam_or_trash(all_labels) {
            return false;
        }
        let cx = Candidate {
            summary,
            detail,
            labels: all_labels,
            now,
        };
        self.groups
            .iter()
            .all(|group| group.iter().any(|t| cx.term(&t.kind) != t.negated))
    }
}

/// Label ids whose id or name matches a `label:` value. Gmail accepts `-` for spaces and
/// slashes, so `label:projects-eloquawk` matches "Projects/Eloquawk".
fn resolve_labels(value: &str, all_labels: &[Label]) -> Vec<String> {
    let norm = |s: &str| s.to_lowercase().replace([' ', '/'], "-");
    let want = norm(value);
    let mut ids: Vec<String> = all_labels
        .iter()
        .filter(|l| norm(&l.name) == want || norm(&l.id) == want)
        .map(|l| l.id.clone())
        .collect();
    if ids.is_empty() {
        ids.push(value.to_uppercase());
    }
    ids
}

struct Candidate<'a> {
    summary: &'a ThreadSummary,
    detail: Option<&'a ThreadDetail>,
    labels: &'a [Label],
    now: &'a Zoned,
}

fn contains(hay: &str, needle: &str) -> bool {
    hay.to_lowercase().contains(needle)
}

fn addr_matches(a: &crate::model::Address, needle: &str) -> bool {
    contains(&a.email, needle) || contains(display_name(a), needle)
}

impl Candidate<'_> {
    fn messages(&self) -> impl Iterator<Item = &crate::model::Message> {
        self.detail.into_iter().flat_map(|d| d.messages.iter())
    }

    fn term(&self, kind: &Kind) -> bool {
        let s = self.summary;
        match kind {
            Kind::Text(t) => self.text(t),
            Kind::From(v) => {
                s.participants.iter().any(|(a, _)| addr_matches(a, v))
                    || self.messages().any(|m| addr_matches(&m.from, v))
            }
            Kind::To(v) => self
                .messages()
                .any(|m| m.to.iter().chain(&m.bcc).any(|a| addr_matches(a, v))),
            Kind::Cc(v) => self
                .messages()
                .any(|m| m.cc.iter().any(|a| addr_matches(a, v))),
            Kind::Subject(v) => contains(&s.subject, v),
            Kind::Label(v) => resolve_labels(v, self.labels)
                .iter()
                .any(|id| s.in_label(id)),
            Kind::In(place) => match place {
                In::Inbox => s.in_label(labels::INBOX),
                In::Sent => s.in_label(labels::SENT),
                In::Trash => s.in_label(labels::TRASH),
                In::Spam => s.in_label(labels::SPAM),
                In::Starred => s.starred(),
                In::Anywhere => true,
            },
            Kind::Is(flag) => match flag {
                Is::Unread => s.unread(),
                Is::Read => !s.unread(),
                Is::Starred => s.starred(),
                Is::Important => s.in_label(labels::IMPORTANT),
            },
            Kind::HasAttachment => {
                s.has_attachments || self.messages().any(|m| !m.attachments.is_empty())
            }
            Kind::NewerThan(age) => self.cutoff(*age).is_some_and(|c| s.last_date > c),
            Kind::OlderThan(age) => self.cutoff(*age).is_some_and(|c| s.last_date < c),
            Kind::After(d) => self.midnight(*d).is_some_and(|c| s.last_date >= c),
            Kind::Before(d) => self.midnight(*d).is_some_and(|c| s.last_date < c),
        }
    }

    fn text(&self, needle: &str) -> bool {
        let s = self.summary;
        contains(&s.subject, needle)
            || contains(&s.snippet, needle)
            || s.participants.iter().any(|(a, _)| addr_matches(a, needle))
            || self.messages().any(|m| {
                contains(&m.body, needle)
                    || m.to.iter().chain(&m.cc).any(|a| addr_matches(a, needle))
                    || m.attachments.iter().any(|a| contains(&a.filename, needle))
            })
    }

    fn cutoff(&self, age: Age) -> Option<Timestamp> {
        let span = match age {
            Age::Days(n) => n.days(),
            Age::Months(n) => n.months(),
            Age::Years(n) => n.years(),
        };
        self.now.checked_sub(span).ok().map(|z| z.timestamp())
    }

    fn midnight(&self, date: Date) -> Option<Timestamp> {
        date.to_zoned(self.now.time_zone().clone())
            .ok()
            .map(|z| z.timestamp())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::model::{Address, Attachment, BodySource, LabelKind, Message, ThreadKey};
    use jiff::tz::TimeZone;

    fn now() -> Zoned {
        "2026-10-29T12:00:00+00:00[Europe/London]".parse().unwrap()
    }

    fn summary(subject: &str, label_ids: &[&str], days_ago: i64) -> ThreadSummary {
        ThreadSummary {
            key: ThreadKey::new("you@example.com", subject),
            history_id: 1,
            subject: subject.into(),
            snippet: format!("snippet of {subject}"),
            participants: vec![(Address::new(Some("Zoë Ångström"), "zoe@example.org"), false)],
            last_date: now().timestamp() - jiff::SignedDuration::from_hours(24 * days_ago),
            message_count: 1,
            label_ids: label_ids.iter().map(|s| s.to_string()).collect(),
            has_attachments: false,
        }
    }

    fn detail(s: &ThreadSummary, body: &str) -> ThreadDetail {
        ThreadDetail {
            key: s.key.clone(),
            subject: s.subject.clone(),
            label_ids: s.label_ids.clone(),
            messages: vec![Message {
                id: "m".into(),
                thread_id: s.key.id.clone(),
                label_ids: BTreeSet::new(),
                from: Address::new(Some("Zoë Ångström"), "zoe@example.org"),
                to: vec![Address::new(Some("You"), "you@example.com")],
                cc: vec![Address::new(Some("Carol"), "carol@example.com")],
                bcc: vec![],
                reply_to: vec![],
                date: s.last_date,
                subject: s.subject.clone(),
                message_id: None,
                in_reply_to: None,
                references: vec![],
                snippet: String::new(),
                body: body.into(),
                body_source: BodySource::Plain,
                body_truncated: false,
                attachments: vec![Attachment {
                    part_id: "1".into(),
                    attachment_id: None,
                    filename: "boarding-pass.pdf".into(),
                    mime_type: "application/pdf".into(),
                    size: 1,
                }],
            }],
        }
    }

    fn user_label(id: &str, name: &str) -> Label {
        Label {
            account: "you@example.com".into(),
            id: id.into(),
            name: name.into(),
            kind: LabelKind::User,
            color: None,
            threads_unread: 0,
            threads_total: 0,
            messages_total: 0,
            hidden: false,
        }
    }

    fn labels_list() -> Vec<Label> {
        vec![
            user_label("Label_1", "Receipts"),
            user_label("Label_2", "Projects/Eloquawk"),
        ]
    }

    fn m(q: &str, s: &ThreadSummary) -> bool {
        Query::parse(q).matches(s, None, &labels_list(), &now())
    }

    fn md(q: &str, s: &ThreadSummary, body: &str) -> bool {
        let d = detail(s, body);
        Query::parse(q).matches(s, Some(&d), &labels_list(), &now())
    }

    #[test]
    fn free_terms_are_anded_and_case_insensitive() {
        let s = summary("Café ☕ plans", &["INBOX"], 1);
        assert!(m("CAFÉ plans", &s));
        assert!(m("café zoë", &s));
        assert!(!m("café tea", &s));
        assert!(m("", &s));
        assert!(Query::parse("  ").is_empty());
    }

    #[test]
    fn phrases_match_as_a_whole() {
        let s = summary("Café plans for Friday", &["INBOX"], 1);
        assert!(m("\"plans for\"", &s));
        assert!(!m("\"for plans\"", &s));
        assert!(m("subject:\"for friday\"", &s));
        // An unterminated quote takes the rest.
        assert!(m("\"plans for", &s));
    }

    #[test]
    fn negation() {
        let s = summary("Weekly report", &["INBOX", "UNREAD"], 1);
        assert!(m("report -invoice", &s));
        assert!(!m("report -weekly", &s));
        assert!(!m("-is:unread", &s));
        assert!(m("-from:bob", &s));
        // A lone dash is text.
        assert!(!m("-", &s));
    }

    #[test]
    fn or_between_terms() {
        let s = summary("Invoice 42", &["INBOX"], 1);
        assert!(m("invoice OR receipt", &s));
        assert!(m("receipt OR invoice", &s));
        assert!(!m("receipt OR order", &s));
        assert!(m("from:zoe OR from:bob invoice", &s));
        // Lowercase "or" is a plain term.
        assert!(!m("invoice or receipt", &s));
        // A leading or trailing OR is plain text.
        assert_eq!(Query::parse("invoice OR"), Query::parse("invoice \"or\""));
        assert_eq!(Query::parse("OR invoice"), Query::parse("\"or\" invoice"));
        // Repeated ORs collapse into one.
        assert_eq!(
            Query::parse("invoice OR OR receipt"),
            Query::parse("invoice OR receipt")
        );
    }

    #[test]
    fn address_operators() {
        let s = summary("Hello", &["INBOX"], 1);
        assert!(m("from:zoë", &s));
        assert!(m("from:zoe@example.org", &s));
        assert!(!m("from:bob", &s));
        // To and Cc need the detail.
        assert!(!m("to:you", &s));
        assert!(md("to:you@example.com", &s, ""));
        assert!(md("cc:carol", &s, ""));
        assert!(!md("cc:you", &s, ""));
    }

    #[test]
    fn subject_and_body_text() {
        let s = summary("Your code", &["INBOX"], 1);
        assert!(m("subject:code", &s));
        assert!(!m("subject:zoë", &s));
        assert!(md("483920", &s, "Your verification code is 483920."));
        assert!(!m("483920", &s));
        assert!(md("boarding", &s, ""));
    }

    #[test]
    fn label_and_in_operators() {
        let s = summary(
            "Receipt",
            &["INBOX", "Label_1", "Label_2", "STARRED", "SENT"],
            1,
        );
        assert!(m("label:receipts", &s));
        assert!(m("label:projects-eloquawk", &s));
        assert!(m("label:projects/eloquawk", &s));
        assert!(m("label:inbox", &s));
        assert!(!m("label:travel", &s));
        assert!(m("in:inbox", &s));
        assert!(m("in:sent", &s));
        assert!(m("in:starred", &s));
        assert!(m("in:anywhere", &s));
        assert!(!m("in:trash", &s));
    }

    #[test]
    fn spam_and_trash_only_when_asked_for() {
        let spam = summary("Win money", &["SPAM"], 1);
        let trash = summary("Old news", &["TRASH"], 1);
        assert!(!m("win", &spam));
        assert!(m("win in:spam", &spam));
        assert!(m("win in:anywhere", &spam));
        assert!(m("label:spam", &spam));
        assert!(!m("old", &trash));
        assert!(m("in:trash old", &trash));
        assert!(!m("-in:trash old", &trash));
    }

    #[test]
    fn is_and_has_operators() {
        let mut s = summary("Hi", &["INBOX", "UNREAD", "IMPORTANT"], 1);
        assert!(m("is:unread", &s));
        assert!(!m("is:read", &s));
        assert!(m("is:important", &s));
        assert!(!m("is:starred", &s));
        assert!(!m("has:attachment", &s));
        assert!(md("has:attachment", &s, ""));
        s.has_attachments = true;
        assert!(m("has:attachment", &s));
    }

    #[test]
    fn unknown_operators_are_free_text() {
        let s = summary("Note about size:large items", &["INBOX"], 1);
        assert!(m("size:large", &s));
        assert!(!m("larger:5M", &s));
        assert!(m("is:bogus OR note", &s));
        assert!(!m("is:bogus", &s));
        assert!(!m("newer_than:soon", &s));
        assert!(!m("newer_than:5é", &s));
        assert!(!m("older_than:é", &s));
        assert!(!m("newer_than:日", &s));
        assert!(!m("after:yesterday", &s));
        assert!(!m("from:", &s));
    }

    #[test]
    fn relative_dates_use_injected_now() {
        let recent = summary("Recent", &["INBOX"], 2);
        let old = summary("Old", &["INBOX"], 400);
        assert!(m("newer_than:3d", &recent));
        assert!(!m("newer_than:1d", &recent));
        assert!(m("older_than:1y", &old));
        assert!(!m("older_than:1y", &recent));
        assert!(m("newer_than:14m", &old));
        assert!(!m("newer_than:12m", &old));
    }

    #[test]
    fn absolute_dates_use_now_time_zone() {
        // 2026-10-27T12:00Z
        let s = summary("Two days ago", &["INBOX"], 2);
        assert!(m("after:2026/10/27", &s));
        assert!(m("after:2026-10-26 before:2026/10/28", &s));
        assert!(!m("after:2026/10/28", &s));
        assert!(!m("before:2026/10/27", &s));
        // In a zone twelve hours ahead, 27 Oct 12:00Z is already the 28th.
        let ahead = Zoned::new(now().timestamp(), TimeZone::fixed(jiff::tz::offset(13)));
        assert!(Query::parse("after:2026/10/28").matches(&s, None, &[], &ahead));
    }
}
