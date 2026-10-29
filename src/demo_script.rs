//! `--demo-actions` scripts (SPEC 11): a pure parser from the comma-separated step list to
//! [`Step`]s, and the [`demo_action`] table for `act:` steps. `main` runs the steps against
//! the focused window; nothing here touches a window, so it is all tested.

use gpui::{Action, Keystroke};

use crate::config::ThemePref;
use crate::ui;

/// What `focus=` can name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FocusTarget {
    List,
    Reader,
    Search,
    Compose,
}

impl FocusTarget {
    pub fn name(self) -> &'static str {
        match self {
            FocusTarget::List => "list",
            FocusTarget::Reader => "reader",
            FocusTarget::Search => "search",
            FocusTarget::Compose => "compose",
        }
    }
}

/// An `expect:` condition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cond {
    /// The cursor is on row N.
    Cursor(usize),
    /// The reader shows row N, or nothing.
    Open(Option<usize>),
    /// The view is the label with this id (`ALL_MAIL` for All Mail), or `search`.
    View(String),
    Focus(FocusTarget),
    /// The search field holds this text.
    Search(String),
    /// The selected body text.
    Selection(String),
}

/// One script step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// Sleep this many milliseconds.
    Wait(u64),
    Theme(ThemePref),
    /// Print `ELOQUAWK_READY <title>`.
    Shot,
    /// Put the cursor on row N and open it.
    Open(usize),
    /// Focus card N's body in the open thread and select all of it.
    SelectAll(usize),
    /// Dispatch the named action (see [`demo_action`]).
    Act(String),
    /// Send each character as a keystroke.
    Type(String),
    /// Check a condition; `raw` is printed with the result.
    Expect {
        raw: String,
        cond: Cond,
    },
    /// Keystrokes, already normalised (`comma` is `,`), sent one after another.
    Keys(Vec<String>),
    /// Could not be parsed; reported as `ELOQUAWK_EXPECT fail step <step>` and skipped.
    Unknown(String),
}

/// Splits on commas not escaped as `\,`, and unescapes them.
fn split_steps(script: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = script.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&',') => {
                cur.push(',');
                chars.next();
            }
            ',' => out.push(std::mem::take(&mut cur)),
            c => cur.push(c),
        }
    }
    out.push(cur);
    out.into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Named keys accepted in key steps, besides single characters.
const NAMED_KEYS: &[&str] = &[
    "enter",
    "escape",
    "space",
    "tab",
    "backspace",
    "delete",
    "up",
    "down",
    "left",
    "right",
    "home",
    "end",
    "pageup",
    "pagedown",
    "insert",
];

/// Normalises and validates one keystroke token: `comma` is `,`, and the key must be a
/// single character, a named key or an F key.
fn key_token(token: &str) -> Option<String> {
    let token = if token == "comma" {
        ",".to_string()
    } else if let Some(mods) = token.strip_suffix("-comma") {
        format!("{mods}-,")
    } else {
        token.to_string()
    };
    let ks = Keystroke::parse(&token).ok()?;
    let key = ks.key.as_str();
    let f_key = key
        .strip_prefix('f')
        .and_then(|n| n.parse::<u8>().ok())
        .is_some_and(|n| (1..=24).contains(&n));
    (key.chars().count() == 1 || NAMED_KEYS.contains(&key) || f_key).then_some(token)
}

fn parse_cond(value: &str) -> Option<Cond> {
    let (name, arg) = value.split_once('=')?;
    let cond = match name {
        "cursor" => Cond::Cursor(arg.parse().ok()?),
        "open" if arg == "none" => Cond::Open(None),
        "open" => Cond::Open(Some(arg.parse().ok()?)),
        "view" if !arg.is_empty() => Cond::View(arg.to_string()),
        "focus" => Cond::Focus(match arg {
            "list" => FocusTarget::List,
            "reader" => FocusTarget::Reader,
            "search" => FocusTarget::Search,
            "compose" => FocusTarget::Compose,
            _ => return None,
        }),
        "search" => Cond::Search(arg.to_string()),
        "selection" => Cond::Selection(arg.to_string()),
        _ => return None,
    };
    Some(cond)
}

fn parse_step(step: &str) -> Step {
    let unknown = || Step::Unknown(step.to_string());
    if step == "shot" {
        return Step::Shot;
    }
    // A command step is `word:value`; anything else (including the `:` key) is keystrokes.
    if let Some((cmd, value)) = step.split_once(':')
        && !cmd.is_empty()
        && cmd.chars().all(|c| c.is_ascii_lowercase())
    {
        return match cmd {
            "wait" => value.parse().map(Step::Wait).unwrap_or_else(|_| unknown()),
            "theme" => match value {
                "light" => Step::Theme(ThemePref::Light),
                "dark" => Step::Theme(ThemePref::Dark),
                _ => unknown(),
            },
            "open" => value.parse().map(Step::Open).unwrap_or_else(|_| unknown()),
            "selectall" => value
                .parse()
                .map(Step::SelectAll)
                .unwrap_or_else(|_| unknown()),
            "act" if demo_action(value).is_some() => Step::Act(value.to_string()),
            "type" if !value.is_empty() => Step::Type(value.to_string()),
            "expect" => match parse_cond(value) {
                Some(cond) => Step::Expect {
                    raw: value.to_string(),
                    cond,
                },
                None => unknown(),
            },
            _ => unknown(),
        };
    }
    let keys: Option<Vec<String>> = step.split_whitespace().map(key_token).collect();
    match keys {
        Some(keys) if !keys.is_empty() => Step::Keys(keys),
        _ => unknown(),
    }
}

/// Parses a `--demo-actions` script. Never fails: bad steps become [`Step::Unknown`].
pub fn parse_steps(script: &str) -> Vec<Step> {
    split_steps(script).iter().map(|s| parse_step(s)).collect()
}

/// The actions `act:` can dispatch, by name; the test checks [`demo_action`] covers them.
#[cfg(test)]
pub const DEMO_ACTIONS: &[&str] = &[
    "NextThread",
    "PrevThread",
    "OpenThread",
    "BackToList",
    "Refresh",
];

/// The action for an `act:` step.
pub fn demo_action(name: &str) -> Option<Box<dyn Action>> {
    let action: Box<dyn Action> = match name {
        "NextThread" => Box::new(ui::NextThread),
        "PrevThread" => Box::new(ui::PrevThread),
        "OpenThread" => Box::new(ui::OpenThread),
        "BackToList" => Box::new(ui::BackToList),
        "Refresh" => Box::new(ui::Refresh),
        _ => return None,
    };
    Some(action)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(k: &[&str]) -> Step {
        Step::Keys(k.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn every_step_kind_parses() {
        let steps = parse_steps(
            "wait:800,theme:light,theme:dark,shot,open:3,act:NextThread,type:hello,\
             expect:cursor=2,selectall:0,j,g i",
        );
        assert_eq!(
            steps,
            vec![
                Step::Wait(800),
                Step::Theme(ThemePref::Light),
                Step::Theme(ThemePref::Dark),
                Step::Shot,
                Step::Open(3),
                Step::Act("NextThread".into()),
                Step::Type("hello".into()),
                Step::Expect {
                    raw: "cursor=2".into(),
                    cond: Cond::Cursor(2)
                },
                Step::SelectAll(0),
                keys(&["j"]),
                keys(&["g", "i"]),
            ]
        );
    }

    #[test]
    fn expect_conditions() {
        let cond = |s: &str| match parse_step(&format!("expect:{s}")) {
            Step::Expect { cond, .. } => Some(cond),
            _ => None,
        };
        assert_eq!(cond("open=none"), Some(Cond::Open(None)));
        assert_eq!(cond("open=4"), Some(Cond::Open(Some(4))));
        assert_eq!(cond("view=INBOX"), Some(Cond::View("INBOX".into())));
        assert_eq!(cond("view=search"), Some(Cond::View("search".into())));
        assert_eq!(cond("focus=list"), Some(Cond::Focus(FocusTarget::List)));
        assert_eq!(cond("focus=reader"), Some(Cond::Focus(FocusTarget::Reader)));
        assert_eq!(cond("focus=search"), Some(Cond::Focus(FocusTarget::Search)));
        assert_eq!(
            cond("focus=compose"),
            Some(Cond::Focus(FocusTarget::Compose))
        );
        assert_eq!(
            cond("search=from:zoë"),
            Some(Cond::Search("from:zoë".into()))
        );
        assert_eq!(
            cond("selection=482 913"),
            Some(Cond::Selection("482 913".into()))
        );
        assert_eq!(cond("focus=nowhere"), None);
        assert_eq!(cond("cursor=x"), None);
        assert_eq!(cond("colour=red"), None);
    }

    #[test]
    fn comma_escapes() {
        assert_eq!(
            parse_steps(r"type:a\,b,comma,ctrl-comma"),
            vec![Step::Type("a,b".into()), keys(&[","]), keys(&["ctrl-,"])]
        );
        assert_eq!(
            parse_steps(r"expect:search=x\, y"),
            vec![Step::Expect {
                raw: "search=x, y".into(),
                cond: Cond::Search("x, y".into())
            }]
        );
    }

    #[test]
    fn chords_split_into_keystrokes() {
        assert_eq!(parse_steps("g i"), vec![keys(&["g", "i"])]);
        assert_eq!(
            parse_steps("shift-i, ctrl-r ,#,:,;"),
            vec![
                keys(&["shift-i"]),
                keys(&["ctrl-r"]),
                keys(&["#"]),
                keys(&[":"]),
                keys(&[";"]),
            ]
        );
        assert_eq!(parse_steps("pagedown"), vec![keys(&["pagedown"])]);
    }

    #[test]
    fn unknown_steps_are_reported() {
        assert_eq!(
            parse_steps("bogus:1,wait:x,theme:sepia,act:Nope,selectall:x,frobnicate,,"),
            vec![
                Step::Unknown("bogus:1".into()),
                Step::Unknown("wait:x".into()),
                Step::Unknown("theme:sepia".into()),
                Step::Unknown("act:Nope".into()),
                Step::Unknown("selectall:x".into()),
                Step::Unknown("frobnicate".into()),
            ]
        );
    }

    #[test]
    fn demo_action_table_covers_every_name() {
        for name in DEMO_ACTIONS {
            let action = demo_action(name).unwrap_or_else(|| panic!("{name} missing"));
            assert!(
                action.name().ends_with(name),
                "{name} maps to {}",
                action.name()
            );
        }
        assert!(demo_action("Nope").is_none());
        assert!(demo_action("").is_none());
    }
}
