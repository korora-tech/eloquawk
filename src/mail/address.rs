//! Email addresses: parsing `Name <a@b>` lists (with quoted commas), formatting them back, and
//! the Gmail-style participants label shown in the thread list (SPEC 4.2).

use std::fmt;

use crate::model::Address;

use super::parse::decode_header;

/// Parses one address. Empty input gives `None`; malformed input is kept as text in `email`.
pub fn parse_address(input: &str) -> Option<Address> {
    let s = input.trim();
    if s.is_empty() {
        return None;
    }
    if let Some(lt) = find_unquoted(s, '<')
        && let Some(gt) = s[lt..].find('>')
    {
        let email = s[lt + 1..lt + gt].trim().to_string();
        let name = clean_name(&s[..lt]);
        if email.is_empty() {
            return name.map(|n| Address {
                name: None,
                email: n,
            });
        }
        return Some(Address { name, email });
    }
    // `a@b (Comment Name)`
    if let Some(open) = find_unquoted(s, '(')
        && s.ends_with(')')
    {
        let email = s[..open].trim();
        if is_bare_email(email) {
            return Some(Address {
                name: clean_name(&s[open + 1..s.len() - 1]),
                email: email.to_string(),
            });
        }
    }
    Some(Address {
        name: None,
        email: s.to_string(),
    })
}

/// Parses an address list such as a To or Cc header. Commas inside quotes, angle brackets and
/// comments do not split.
pub fn parse_list(input: &str) -> Vec<Address> {
    split_list(input)
        .into_iter()
        .filter_map(parse_address)
        .collect()
}

fn split_list(input: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut quoted, mut escaped, mut angle, mut paren) = (false, false, false, 0u32);
    let mut start = 0;
    for (i, c) in input.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            '<' if !quoted => angle = true,
            '>' if !quoted => angle = false,
            '(' if !quoted => paren += 1,
            ')' if !quoted => paren = paren.saturating_sub(1),
            ',' | ';' if !quoted && !angle && paren == 0 => {
                out.push(&input[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&input[start..]);
    out
}

/// Byte offset of `needle` outside double quotes.
fn find_unquoted(s: &str, needle: char) -> Option<usize> {
    let (mut quoted, mut escaped) = (false, false);
    for (i, c) in s.char_indices() {
        if escaped {
            escaped = false;
        } else if c == '\\' && quoted {
            escaped = true;
        } else if c == '"' {
            quoted = !quoted;
        } else if c == needle && !quoted {
            return Some(i);
        }
    }
    None
}

/// Trims, unquotes and RFC 2047-decodes a display name. Empty names become `None`.
fn clean_name(raw: &str) -> Option<String> {
    let mut s = raw.trim();
    let unquoted;
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        unquoted = s[1..s.len() - 1]
            .replace("\\\"", "\"")
            .replace("\\\\", "\\");
        s = &unquoted;
    }
    let name = decode_header(s.trim());
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_string())
}

fn is_bare_email(s: &str) -> bool {
    !s.is_empty() && s.contains('@') && !s.contains(char::is_whitespace)
}

/// Formats an address for a header or an editable field: `Name <a@b>`, quoting the name when
/// it holds specials.
pub fn format_address(a: &Address) -> String {
    match a.name.as_deref().filter(|n| !n.is_empty()) {
        None => a.email.clone(),
        Some(n) if n.contains(|c: char| ",;:<>@()[]\"\\.".contains(c)) => {
            format!(
                "\"{}\" <{}>",
                n.replace('\\', "\\\\").replace('"', "\\\""),
                a.email
            )
        }
        Some(n) => format!("{n} <{}>", a.email),
    }
}

/// Formats an address list, comma-separated.
pub fn format_list(list: &[Address]) -> String {
    list.iter()
        .map(format_address)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The name to show for an address: its display name, or the email when there is none.
pub fn display_name(a: &Address) -> &str {
    a.name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .unwrap_or(&a.email)
}

/// The short form used when a thread has more than two participants.
fn first_name(a: &Address) -> &str {
    match a.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        // "Last, First" keeps the part after the comma.
        Some(n) if n.contains(',') => n
            .split_once(',')
            .map(|(_, f)| f.trim())
            .and_then(|f| f.split_whitespace().next())
            .unwrap_or(n),
        Some(n) => n.split_whitespace().next().unwrap_or(n),
        None => a.email.split('@').next().unwrap_or(&a.email),
    }
}

/// Collects a thread's participants in message order, deduped by email (case-insensitive).
/// The flag is true when any of that participant's messages is unread. A later name fills in
/// a missing one.
pub fn participants(senders: impl IntoIterator<Item = (Address, bool)>) -> Vec<(Address, bool)> {
    let mut out: Vec<(Address, bool)> = Vec::new();
    for (addr, unread) in senders {
        if let Some((seen, seen_unread)) = out
            .iter_mut()
            .find(|(a, _)| a.email.eq_ignore_ascii_case(&addr.email))
        {
            *seen_unread |= unread;
            if seen.name.is_none() {
                seen.name = addr.name;
            }
        } else {
            out.push((addr, unread));
        }
    }
    out
}

/// A Gmail-style sender label: names in order, each marked unread or not, an elision marker
/// when more than three people took part, and the message count when above one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParticipantsLabel {
    /// The names to show and whether each is unread (bold in the UI).
    pub names: Vec<(String, bool)>,
    /// True when participants were left out between the first and the last two names.
    pub elided: bool,
    /// `Some(n)` when the thread has more than one message.
    pub count: Option<u32>,
}

impl fmt::Display for ParticipantsLabel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, (name, _)) in self.names.iter().enumerate() {
            match (i, self.elided) {
                (0, _) => {}
                (1, true) => f.write_str(" .. ")?,
                _ => f.write_str(", ")?,
            }
            f.write_str(name)?;
        }
        if let Some(n) = self.count {
            write!(f, " ({n})")?;
        }
        Ok(())
    }
}

/// Builds the sender label for a thread (SPEC 4.2). `me` is the account's own address and is
/// shown as "me". With more than two participants names shorten to first names; with more
/// than three, the first and the last two are kept.
pub fn participants_label(
    participants: &[(Address, bool)],
    me: &str,
    message_count: u32,
) -> ParticipantsLabel {
    let short = participants.len() > 2;
    let name_of = |(a, unread): &(Address, bool)| {
        let name = if a.email.eq_ignore_ascii_case(me) {
            "me".to_string()
        } else if short {
            first_name(a).to_string()
        } else {
            display_name(a).to_string()
        };
        (name, *unread)
    };
    let elided = participants.len() > 3;
    let names = if elided {
        let n = participants.len();
        [&participants[0], &participants[n - 2], &participants[n - 1]]
            .into_iter()
            .map(name_of)
            .collect()
    } else {
        participants.iter().map(name_of).collect()
    };
    ParticipantsLabel {
        names,
        elided,
        count: (message_count > 1).then_some(message_count),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(name: Option<&str>, email: &str) -> Address {
        Address::new(name, email)
    }

    #[test]
    fn parses_display_names_and_angle_only() {
        assert_eq!(
            parse_address("Alice Example <alice@example.com>"),
            Some(a(Some("Alice Example"), "alice@example.com"))
        );
        assert_eq!(
            parse_address("<bob@example.com>"),
            Some(a(None, "bob@example.com"))
        );
        assert_eq!(
            parse_address("  carol@example.com "),
            Some(a(None, "carol@example.com"))
        );
        assert_eq!(
            parse_address("dave@example.com (Dave D)"),
            Some(a(Some("Dave D"), "dave@example.com"))
        );
        assert_eq!(parse_address("   "), None);
    }

    #[test]
    fn quoted_commas_do_not_split() {
        let list =
            parse_list("\"Doe, Jane\" <jane@example.com>, Bob <bob@example.com>,carol@example.com");
        assert_eq!(
            list,
            vec![
                a(Some("Doe, Jane"), "jane@example.com"),
                a(Some("Bob"), "bob@example.com"),
                a(None, "carol@example.com"),
            ]
        );
        let escaped = parse_list(r#""Say \"hi\", ok" <x@example.com>"#);
        assert_eq!(escaped, vec![a(Some("Say \"hi\", ok"), "x@example.com")]);
        assert!(parse_list("").is_empty());
        assert!(parse_list(" , ").is_empty());
    }

    #[test]
    fn encoded_names_are_decoded() {
        assert_eq!(
            parse_address("=?utf-8?Q?Zo=C3=AB_=C3=85ngstr=C3=B6m?= <zoe@example.com>"),
            Some(a(Some("Zoë Ångström"), "zoe@example.com"))
        );
    }

    #[test]
    fn malformed_input_is_kept_as_text() {
        assert_eq!(
            parse_address("undisclosed-recipients:;"),
            Some(a(None, "undisclosed-recipients:;"))
        );
        assert_eq!(parse_address("Just A Name"), Some(a(None, "Just A Name")));
        assert_eq!(parse_address("Name <>"), Some(a(None, "Name")));
    }

    #[test]
    fn formats_and_round_trips() {
        let list = vec![
            a(Some("Doe, Jane"), "jane@example.com"),
            a(Some("Bob"), "bob@example.com"),
            a(None, "carol@example.com"),
            a(Some("Say \"hi\""), "x@example.com"),
        ];
        let text = format_list(&list);
        assert_eq!(
            text,
            "\"Doe, Jane\" <jane@example.com>, Bob <bob@example.com>, carol@example.com, \
             \"Say \\\"hi\\\"\" <x@example.com>"
        );
        assert_eq!(parse_list(&text), list);
    }

    #[test]
    fn display_name_falls_back_to_email() {
        assert_eq!(display_name(&a(Some(" "), "x@y.z")), "x@y.z");
        assert_eq!(display_name(&a(Some("Xavier"), "x@y.z")), "Xavier");
    }

    #[test]
    fn participants_dedupe_by_email_and_union_unread() {
        let p = participants([
            (a(None, "Alice@Example.com"), false),
            (a(Some("Bob"), "bob@example.com"), false),
            (a(Some("Alice"), "alice@example.com"), true),
        ]);
        assert_eq!(
            p,
            vec![
                (a(Some("Alice"), "Alice@Example.com"), true),
                (a(Some("Bob"), "bob@example.com"), false),
            ]
        );
    }

    const ME: &str = "me@example.com";

    #[test]
    fn label_single_sender_uses_full_name() {
        let l = participants_label(&[(a(Some("Alice Smith"), "alice@x.com"), true)], ME, 1);
        assert_eq!(l.to_string(), "Alice Smith");
        assert_eq!(l.names, vec![("Alice Smith".to_string(), true)]);
        assert_eq!(l.count, None);
    }

    #[test]
    fn label_two_participants_with_me_and_count() {
        let l = participants_label(
            &[
                (a(Some("Alice Smith"), "alice@x.com"), false),
                (a(Some("Me Myself"), "ME@example.com"), false),
            ],
            ME,
            4,
        );
        assert_eq!(l.to_string(), "Alice Smith, me (4)");
    }

    #[test]
    fn label_three_participants_use_first_names_and_mark_unread() {
        let l = participants_label(
            &[
                (a(Some("Alice Smith"), "alice@x.com"), false),
                (a(None, ME), false),
                (a(Some("Bob Jones"), "bob@x.com"), true),
            ],
            ME,
            4,
        );
        assert_eq!(l.to_string(), "Alice, me, Bob (4)");
        assert_eq!(l.names[2], ("Bob".to_string(), true));
        assert!(!l.names[0].1);
    }

    #[test]
    fn label_more_than_three_elides_the_middle() {
        let l = participants_label(
            &[
                (a(Some("Alice Smith"), "alice@x.com"), false),
                (a(Some("Bob Jones"), "bob@x.com"), false),
                (a(None, "carol@x.com"), false),
                (a(Some("Doe, Dan"), "dan@x.com"), true),
            ],
            ME,
            7,
        );
        assert_eq!(l.to_string(), "Alice .. carol, Dan (7)");
        assert!(l.elided);
    }

    #[test]
    fn label_only_me() {
        let l = participants_label(&[(a(Some("Me"), ME), false)], ME, 1);
        assert_eq!(l.to_string(), "me");
    }
}
