//! The keyboard shortcut overlay (SPEC 9), toggled with `?` and closed with escape or a click
//! outside the card. It renders [`SHORTCUTS`], the one copy of the section 9 table; the README
//! table is copied from it by hand, and a test checks it against the key map in `ui/mod.rs`.

use gpui::prelude::*;
use gpui::{
    AnyElement, ClickEvent, Context, Div, FontWeight, SharedString, Stateful, div, px, rgb,
};

use super::main_view::MainView;
use super::theme::{Palette, palette};

/// One row of the shortcut table.
#[derive(Clone, Copy, Debug)]
pub struct Shortcut {
    /// The bound keystrokes, as `KeyBinding` parses them; alternatives are separate entries.
    pub keys: &'static [&'static str],
    /// The action's name in `ui/mod.rs`.
    pub action: &'static str,
    /// The key context it is bound in.
    pub context: &'static str,
    /// What the help overlay shows.
    pub description: &'static str,
}

const fn row(
    keys: &'static [&'static str],
    action: &'static str,
    context: &'static str,
    description: &'static str,
) -> Shortcut {
    Shortcut {
        keys,
        action,
        context,
        description,
    }
}

/// The section 9 table, in its order.
pub const SHORTCUTS: &[Shortcut] = &[
    row(&["j", "down"], "NextThread", "Mail", "Next conversation"),
    row(&["k", "up"], "PrevThread", "Mail", "Previous conversation"),
    row(&["n"], "NextMessage", "Mail", "Next message"),
    row(&["p"], "PrevMessage", "Mail", "Previous message"),
    row(
        &["o", "enter"],
        "OpenThread",
        "Mail",
        "Open, or toggle message",
    ),
    row(&["u", "escape"], "BackToList", "Mail", "Back to the list"),
    row(
        &["space", "pagedown"],
        "ScrollReaderDown",
        "Mail",
        "Scroll down a page",
    ),
    row(
        &["shift-space", "pageup"],
        "ScrollReaderUp",
        "Mail",
        "Scroll up a page",
    ),
    row(&["x"], "ToggleSelect", "Mail", "Select conversation"),
    row(&["e"], "Archive", "Mail", "Archive, or Not spam"),
    row(&["#"], "Trash", "Mail", "Trash, or Restore"),
    row(&["s"], "ToggleStar", "Mail", "Star or unstar"),
    row(&["shift-i"], "MarkRead", "Mail", "Mark as read"),
    row(&["shift-u"], "MarkUnread", "Mail", "Mark as unread"),
    row(&["!"], "ReportSpam", "Mail", "Report spam"),
    row(&["l"], "OpenLabelPicker", "Mail", "Labels"),
    row(&["r"], "Reply", "Mail", "Reply"),
    row(&["a"], "ReplyAll", "Mail", "Reply all"),
    row(&["f"], "Forward", "Mail", "Forward"),
    row(&["c"], "Compose", "Mail", "Compose"),
    row(&["z"], "Undo", "Mail", "Undo"),
    row(&[";"], "ExpandAll", "Mail", "Expand all messages"),
    row(&[":"], "CollapseAll", "Mail", "Collapse all messages"),
    row(&["/"], "FocusSearch", "Mail", "Search"),
    row(&["g i"], "GoInbox", "Mail", "Go to Inbox"),
    row(&["g s"], "GoStarred", "Mail", "Go to Starred"),
    row(&["g t"], "GoSent", "Mail", "Go to Sent"),
    row(&["g d"], "GoDrafts", "Mail", "Go to Drafts"),
    row(&["g a"], "GoAllMail", "Mail", "Go to All Mail"),
    row(&["g n"], "NextAccount", "Mail", "Next account"),
    row(&["g p"], "PrevAccount", "Mail", "Previous account"),
    row(&["?"], "ToggleHelp", "Mail", "Keyboard shortcuts"),
    row(
        &["1"],
        "ApplySuggestion1",
        "Mail",
        "Apply suggested label 1",
    ),
    row(
        &["2"],
        "ApplySuggestion2",
        "Mail",
        "Apply suggested label 2",
    ),
    row(
        &["3"],
        "ApplySuggestion3",
        "Mail",
        "Apply suggested label 3",
    ),
    row(&["ctrl-c"], "Copy", "ReadOnlyText", "Copy the selection"),
    row(
        &["ctrl-a"],
        "SelectAll",
        "ReadOnlyText",
        "Select the whole message",
    ),
    row(
        &["escape"],
        "ClosePicker",
        "LabelPicker",
        "Close the picker",
    ),
    row(&["up"], "PickerUp", "LabelPicker", "Previous label"),
    row(&["down"], "PickerDown", "LabelPicker", "Next label"),
    row(&["enter"], "Submit", "Search", "Search this account"),
    row(
        &["escape"],
        "CancelSearch",
        "Search",
        "Clear and leave search",
    ),
    row(&["ctrl-enter"], "Send", "Compose", "Send"),
    row(
        &["escape", "ctrl-w"],
        "CloseCompose",
        "Compose",
        "Close the message",
    ),
    row(&["ctrl-q"], "Quit", "Compose", "Quit"),
    row(&["escape"], "CloseSettings", "Settings", "Close Settings"),
    row(&["ctrl-r"], "Refresh", "Eloquawk", "Refresh"),
    row(&["ctrl-,"], "OpenSettings", "Eloquawk", "Settings"),
    row(&["ctrl-n"], "Compose", "Eloquawk", "Compose"),
    row(&["ctrl-w"], "CloseWindow", "Eloquawk", "Close the window"),
    row(&["ctrl-q"], "Quit", "Eloquawk", "Quit"),
];

/// The heading a context's rows sit under.
fn context_title(context: &str) -> &'static str {
    match context {
        "Mail" => "Mail",
        "ReadOnlyText" => "Message text",
        "LabelPicker" => "Label picker",
        "Search" => "Search",
        "Compose" => "Compose window",
        "Settings" => "Settings",
        _ => "Anywhere",
    }
}

/// How a bound keystroke is shown: `shift-i` as "Shift I", `g i` as "g then i".
pub fn key_label(key: &str) -> String {
    key.split(' ')
        .map(|stroke| {
            stroke
                .split('-')
                .enumerate()
                .map(|(i, part)| {
                    let last = i == stroke.matches('-').count();
                    match part {
                        "ctrl" => "Ctrl".to_string(),
                        "shift" => "Shift".to_string(),
                        "alt" => "Alt".to_string(),
                        "space" => "Space".to_string(),
                        "enter" => "Enter".to_string(),
                        "escape" => "Esc".to_string(),
                        "pagedown" => "Page Down".to_string(),
                        "pageup" => "Page Up".to_string(),
                        "up" => "↑".to_string(),
                        "down" => "↓".to_string(),
                        p if last && p.len() == 1 && stroke.contains("shift-") => p.to_uppercase(),
                        p => p.to_string(),
                    }
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join(" then ")
}

fn key_chip(p: &Palette, key: &str) -> Div {
    div()
        .flex_shrink_0()
        .px(px(6.0))
        .h(px(20.0))
        .flex()
        .items_center()
        .rounded(px(4.0))
        .border_1()
        .border_color(rgb(p.border))
        .bg(rgb(p.panel_raised))
        .text_size(px(11.5))
        .line_height(px(16.0))
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(p.text))
        .child(key_label(key))
}

fn shortcut_row(p: &Palette, s: &Shortcut) -> Stateful<Div> {
    let mut keys = div()
        .w(px(176.0))
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap(px(4.0));
    for (i, key) in s.keys.iter().enumerate() {
        if i > 0 {
            keys = keys.child(
                div()
                    .text_size(px(11.0))
                    .text_color(rgb(p.caption))
                    .child("or"),
            );
        }
        keys = keys.child(key_chip(p, key));
    }
    div()
        // Named by context and action, which together identify a row.
        .id(SharedString::from(format!(
            "help-{}-{}",
            s.context, s.action
        )))
        .h(px(26.0))
        .flex()
        .items_center()
        .gap(px(10.0))
        .child(keys)
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .truncate()
                .text_size(px(12.5))
                .line_height(px(18.0))
                .text_color(rgb(p.secondary))
                .child(s.description),
        )
}

fn heading(p: &Palette, text: &'static str) -> Div {
    div()
        .pt(px(10.0))
        .pb(px(4.0))
        .text_size(px(11.0))
        .line_height(px(16.0))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(p.caption))
        .child(text.to_uppercase())
}

/// The rows grouped under their headings, split into three columns: the Mail rows fill the
/// first two, the other contexts the third.
fn columns(p: &Palette) -> [Div; 3] {
    let column = || div().flex_1().min_w(px(0.0)).flex().flex_col();
    let mail: Vec<&Shortcut> = SHORTCUTS.iter().filter(|s| s.context == "Mail").collect();
    let half = mail.len().div_ceil(2);
    let mut first = column().child(heading(p, "Mail"));
    for s in &mail[..half] {
        first = first.child(shortcut_row(p, s));
    }
    let mut second = column().child(heading(p, "\u{00a0}"));
    for s in &mail[half..] {
        second = second.child(shortcut_row(p, s));
    }
    let mut third = column();
    let mut last_context = "";
    for s in SHORTCUTS.iter().filter(|s| s.context != "Mail") {
        if s.context != last_context {
            third = third.child(heading(p, context_title(s.context)));
            last_context = s.context;
        }
        third = third.child(shortcut_row(p, s));
    }
    [first, second, third]
}

impl MainView {
    /// The overlay: a scrim that closes it on click, and the shortcut card.
    pub(super) fn help_overlay(&self, cx: &Context<Self>) -> AnyElement {
        let p = palette(cx);
        let [a, b, c] = columns(p);
        div()
            .id("help-scrim")
            // Without this the panes underneath stay hovered and get the same click (a click
            // here would also open or star the row under it) plus hover and scroll.
            .occlude()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .p(px(24.0))
            .bg(gpui::rgba((p.bg << 8) | 0xb8))
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.set_help(false, window, cx);
            }))
            .child(
                div()
                    .id("help")
                    .w(px(1200.0))
                    .max_w_full()
                    .max_h_full()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .rounded(px(10.0))
                    .border_1()
                    .border_color(rgb(p.selected_border))
                    .bg(rgb(p.panel))
                    .shadow_lg()
                    .px(px(24.0))
                    .pt(px(18.0))
                    .pb(px(20.0))
                    // Clicks on the card do not reach the scrim.
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(px(17.0))
                                    .line_height(px(24.0))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(rgb(p.text))
                                    .child("Keyboard shortcuts"),
                            )
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .text_color(rgb(p.caption))
                                    .child("Press ? or Esc to close"),
                            ),
                    )
                    .child(div().flex().gap(px(28.0)).child(a).child(b).child(c)),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    /// Every Mail, LabelPicker, Search, Compose and Eloquawk row is bound to its action, and every such
    /// binding is listed.
    #[test]
    fn shortcuts_match_the_key_map() {
        let bound: BTreeSet<(String, String)> = crate::ui::bindings()
            .iter()
            .map(|b| {
                let keys = b
                    .keystrokes()
                    .iter()
                    .map(|k| k.unparse())
                    .collect::<Vec<_>>()
                    .join(" ");
                let name = b.action().name();
                let action = name.rsplit("::").next().unwrap_or(name).to_string();
                (keys, action)
            })
            .collect();
        // Enter in search is the field's Submit.
        let listed: BTreeSet<(String, String)> = SHORTCUTS
            .iter()
            .filter(|s| {
                matches!(
                    s.context,
                    "Mail" | "Eloquawk" | "LabelPicker" | "Search" | "Compose" | "Settings"
                ) && s.action != "Submit"
            })
            .flat_map(|s| s.keys.iter().map(|k| (k.to_string(), s.action.to_string())))
            .collect();
        assert_eq!(bound, listed);
    }

    #[test]
    fn mail_bindings_use_the_mail_predicate() {
        let mail = gpui::KeyBindingContextPredicate::parse(crate::ui::MAIL).ok();
        let mail_rows: BTreeSet<(String, &str)> = SHORTCUTS
            .iter()
            .filter(|s| s.context == "Mail")
            .flat_map(|s| s.keys.iter().map(|k| (k.to_string(), s.action)))
            .collect();
        let mut seen = 0;
        for b in crate::ui::bindings() {
            let name = b.action().name();
            let action = name.rsplit("::").next().unwrap_or(name);
            let keys: Vec<String> = b.keystrokes().iter().map(|k| k.unparse()).collect();
            if mail_rows.contains(&(keys.join(" "), action)) {
                seen += 1;
                assert_eq!(
                    b.predicate().as_deref(),
                    mail.as_ref(),
                    "{action} on {keys:?}"
                );
            }
        }
        assert_eq!(seen, mail_rows.len());
    }

    /// The README's shortcut table row for `s`: the keys as the overlay shows them, the
    /// description and where the keys work.
    fn readme_row(s: &Shortcut) -> String {
        let keys = s
            .keys
            .iter()
            .map(|k| format!("`{}`", key_label(k)))
            .collect::<Vec<_>>()
            .join(" or ");
        format!(
            "| {keys} | {} | {} |",
            s.description,
            context_title(s.context)
        )
    }

    /// The README's table is a copy of [`SHORTCUTS`], row for row. The Nix package's source
    /// holds no README or docs, so its check phase skips this; in a checkout (which has
    /// `docs/`) a missing README fails, and CI's `cargo test` runs it in full.
    #[test]
    fn readme_lists_every_shortcut() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let readme = match std::fs::read_to_string(root.join("README.md")) {
            Ok(readme) => readme,
            Err(e) => {
                assert!(!root.join("docs").is_dir(), "README.md is unreadable: {e}");
                return;
            }
        };
        // Every table row except the header and the separator, so a row written without
        // backticks is compared too rather than skipped.
        let listed: Vec<&str> = readme
            .lines()
            .skip_while(|l| *l != "## Keyboard shortcuts")
            .skip(1)
            .take_while(|l| !l.starts_with("## "))
            .filter(|l| l.starts_with('|'))
            .filter(|l| !l.starts_with("| Keys") && !l.starts_with("| ---"))
            .collect();
        let expected: Vec<String> = SHORTCUTS.iter().map(readme_row).collect();
        assert_eq!(
            listed,
            expected,
            "README table should be:\n{}",
            expected.join("\n")
        );
    }

    #[test]
    fn keys_are_shown_readably() {
        assert_eq!(key_label("j"), "j");
        assert_eq!(key_label("shift-i"), "Shift I");
        assert_eq!(key_label("g i"), "g then i");
        assert_eq!(key_label("ctrl-,"), "Ctrl ,");
        assert_eq!(key_label("shift-space"), "Shift Space");
        assert_eq!(key_label("escape"), "Esc");
        assert_eq!(key_label("#"), "#");
    }
}
