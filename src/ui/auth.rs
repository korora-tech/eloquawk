//! The Auth screen's cards (SPEC 8.2): shown instead of the panes while the keyring is being
//! read or is locked, when no Google client is set up, and when no account is signed in.

use gpui::prelude::*;
use gpui::{AnyElement, ClickEvent, ClipboardItem, Context, Div, div, px};

use super::main_view::MainView;
use super::theme::{Palette, palette};
use super::widgets::{ButtonStyle, OnClick, body_text, button, card};
use crate::protocol::{AuthState, Command};
use crate::store::Store;

const KEYRING_READ_ERROR: &str = concat!(
    "Eloquawk could not read your Google credentials from the system keyring. ",
    "Unlock it (your desktop may have shown a prompt), then retry.",
);
const KEYRING_STARTING_INFO: &str = concat!(
    "Eloquawk keeps your Google credentials in the system keyring. ",
    "Your desktop may ask you to unlock it.",
);
const NEEDS_CLIENT_INFO: &str = concat!(
    "Eloquawk talks to Gmail with an OAuth client you create in the Google Cloud console. ",
    "Enable the Gmail API, create a desktop client, then paste its id and secret in Settings. ",
    "docs/google-gmail.md walks through it.",
);
const SIGN_IN_INFO: &str = concat!(
    "Your browser will open Google's consent page, which asks to let Eloquawk read, ",
    "organise and send your mail. You can add more accounts later in Settings.",
);
const SIGN_IN_BROWSER_INFO: &str =
    "Approve access on Google's page. If no browser opened, copy the link and open it yourself.";

/// Whether the main window shows the panes or an auth card.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    Mail,
    Auth,
}

impl Screen {
    pub fn of(store: &Store) -> Self {
        match store.auth {
            AuthState::Starting | AuthState::Locked | AuthState::NeedsClient => Screen::Auth,
            AuthState::Ready | AuthState::Demo if store.accounts.is_empty() => Screen::Auth,
            AuthState::Ready | AuthState::Demo => Screen::Mail,
        }
    }
}

impl MainView {
    fn send_click(&self, cx: &Context<Self>, cmd: fn() -> Command) -> OnClick {
        Box::new(
            cx.listener(move |this, _: &ClickEvent, _, cx| this.store.read(cx).send_command(cmd())),
        )
    }

    /// The card for the Auth screen.
    pub(super) fn auth_card(&self, store: &Store, cx: &Context<Self>) -> AnyElement {
        let p = palette(cx);
        let content = match store.auth {
            AuthState::Locked => card(p, "Unlock your keyring")
                .child(body_text(p, KEYRING_READ_ERROR))
                .child(div().flex().child(button(
                    p,
                    "retry-keyring",
                    "Retry",
                    ButtonStyle::Primary,
                    Some(self.send_click(cx, || Command::Refresh)),
                ))),
            AuthState::Starting => {
                card(p, "Opening your keyring…").child(body_text(p, KEYRING_STARTING_INFO))
            }
            AuthState::Demo => card(p, "Loading the demo mailbox…"),
            AuthState::NeedsClient => {
                card(p, "Set up a Google client").child(body_text(p, NEEDS_CLIENT_INFO))
            }
            AuthState::Ready => match store.signing_in.clone() {
                None => card(p, "Sign in to Google")
                    .child(body_text(p, SIGN_IN_INFO))
                    .child(div().flex().child(button(
                        p,
                        "sign-in",
                        "Sign in with Google",
                        ButtonStyle::Primary,
                        Some(self.send_click(cx, || Command::AddAccount)),
                    ))),
                Some(url) => card(p, "Finish signing in in your browser")
                    .child(body_text(p, SIGN_IN_BROWSER_INFO))
                    .child(self.sign_in_actions(p, url, cx)),
            },
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .p_8()
            .child(div().w(px(460.0)).child(content))
            .into_any_element()
    }

    /// "Copy sign-in link" and "Cancel", while a browser sign-in is pending.
    fn sign_in_actions(&self, p: &Palette, url: String, cx: &Context<Self>) -> Div {
        div()
            .flex()
            .gap_2()
            .child(button(
                p,
                "copy-link",
                "Copy sign-in link",
                ButtonStyle::Secondary,
                Some(Box::new(move |_, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(url.clone()))
                })),
            ))
            .child(button(
                p,
                "cancel-sign-in",
                "Cancel",
                ButtonStyle::Secondary,
                Some(self.send_click(cx, || Command::CancelSignIn)),
            ))
    }
}
