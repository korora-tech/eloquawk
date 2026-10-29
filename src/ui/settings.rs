//! The Settings screen (SPEC 8.4): the Google OAuth client, the accounts and the
//! preferences. It replaces the panes in the main window; escape or ✕ closes it.
//!
//! The client secret is write-only from the UI's point of view: it goes to the worker, which
//! stores it in the keyring; the field is cleared afterwards and never pre-filled. Preference
//! changes update `store.prefs` and are saved only when the prefs are not ephemeral (demo).

use gpui::prelude::*;
use gpui::{
    App, ClickEvent, ClipboardItem, Context, Div, Entity, EventEmitter, FocusHandle, Focusable,
    FontWeight, ScrollHandle, SharedString, Subscription, Window, div, px, rgb,
};

use super::CloseSettings;
use super::text_input::{Submit, TextInput};
use super::theme::{self, Palette, ThemeOverride, palette};
use super::widgets::{
    ButtonStyle, OnClick, body_text, button, caption, card, checkbox_row, ghost_button, segment,
    segments,
};
use crate::config::ThemePref;
use crate::google::oauth;
use crate::protocol::{AuthState, Command};
use crate::store::{Mode, Store};

/// Enabling the Gmail API in the user's Google Cloud project.
const GMAIL_API_URL: &str = "https://console.cloud.google.com/apis/library/gmail.googleapis.com";
/// The setup guide (SPEC 13, M15).
const GUIDE_URL: &str = "https://github.com/korora-tech/eloquawk/blob/main/docs/google-gmail.md";

const CLIENT_INTRO_BUILTIN: &str = concat!(
    "Eloquawk signs in with its own OAuth client, so there is nothing to set up. To use a ",
    "client you own instead, create a Desktop client in a Google Cloud project with the Gmail ",
    "API enabled and save it here. Its ID and secret are stored only in your system keyring. ",
    "Switching clients signs out every account.",
);
const CLIENT_INTRO_OWN: &str = concat!(
    "Eloquawk talks to Gmail through an OAuth client you create: enable the Gmail API in a ",
    "Google Cloud project, create a Desktop client, then paste its ID and secret here. They ",
    "are stored only in your system keyring.",
);
const STATUS_DEMO: &str = "Demo mode: nothing is saved";
const STATUS_LOCKED: &str = "Keyring locked: unlock it and press Ctrl+R";
const STATUS_SAVED: &str = "Your client is stored in the keyring";
const STATUS_BUILTIN: &str = "Using the built-in client";
const STATUS_NONE: &str = "No client set up yet";
const SIGN_IN_PENDING: &str =
    "Waiting for you to choose an account and approve access in the browser…";

/// The refresh interval choices, in seconds.
const REFRESH_CHOICES: &[(u64, &str)] = &[(60, "1 min"), (300, "5 min"), (900, "15 min")];

pub enum SettingsEvent {
    Close,
}

pub struct SettingsView {
    store: Entity<Store>,
    client_id: Entity<TextInput>,
    client_secret: Entity<TextInput>,
    /// The stored client id last copied into the field, so edits are not overwritten.
    shown_client: Option<Option<String>>,
    /// Whether this build has a built-in client.
    builtin: bool,
    focus: FocusHandle,
    scroll: ScrollHandle,
    _subscription: Subscription,
}

impl EventEmitter<SettingsEvent> for SettingsView {}

impl SettingsView {
    pub fn new(store: Entity<Store>, cx: &mut Context<Self>) -> Self {
        let client_id =
            cx.new(|cx| TextInput::new("1234567890-abc.apps.googleusercontent.com", false, 1, cx));
        let client_secret = cx.new(|cx| TextInput::new("GOCSPX-…", true, 2, cx));
        let subscription = cx.observe(&store, |this, _, cx| {
            this.sync_from_store(cx);
            cx.notify();
        });
        let mut this = Self {
            store,
            client_id,
            client_secret,
            shown_client: None,
            builtin: oauth::builtin_client().is_some(),
            focus: cx.focus_handle(),
            scroll: ScrollHandle::new(),
            _subscription: subscription,
        };
        this.sync_from_store(cx);
        this
    }

    /// When the stored client changes (loaded, saved, removed), reflect it in the fields.
    fn sync_from_store(&mut self, cx: &mut Context<Self>) {
        let stored = self.store.read(cx).client_id.clone();
        if self.shown_client.as_ref() == Some(&stored) {
            return;
        }
        self.shown_client = Some(stored.clone());
        let has_client = stored.is_some();
        self.client_id
            .update(cx, |i, cx| i.set_text(stored.unwrap_or_default(), cx));
        self.client_secret.update(cx, |i, cx| {
            i.set_text("", cx);
            i.set_placeholder(
                if has_client {
                    "Stored in the keyring · leave blank to keep it"
                } else {
                    "GOCSPX-…"
                },
                cx,
            );
        });
    }

    fn save_client(&mut self, cx: &mut Context<Self>) {
        let client_id = self.client_id.read(cx).text().trim().to_string();
        let secret = self.client_secret.read(cx).text().trim().to_string();
        self.store.read(cx).send_command(Command::SaveClient {
            client_id,
            client_secret: (!secret.is_empty()).then_some(secret),
        });
        // Do not keep the secret around in the UI once it has been handed over.
        self.client_secret.update(cx, |i, cx| {
            i.set_text("", cx);
            i.set_masked(true, cx);
        });
    }

    fn click(
        &self,
        cx: &Context<Self>,
        f: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> OnClick {
        Box::new(cx.listener(move |this, _: &ClickEvent, window, cx| f(this, window, cx)))
    }

    fn send(&self, cx: &Context<Self>, cmd: fn() -> Command) -> OnClick {
        Box::new(
            cx.listener(move |this, _: &ClickEvent, _, cx| this.store.read(cx).send_command(cmd())),
        )
    }

    /// Changes a preference and saves it (unless ephemeral).
    fn pref(&self, cx: &Context<Self>, f: impl Fn(&mut crate::config::Prefs) + 'static) -> OnClick {
        self.click(cx, move |this, _, cx| {
            this.store.update(cx, |s, cx| {
                s.change_prefs(&f);
                cx.notify();
            })
        })
    }

    /// Picks a theme: saved like any preference, and it replaces a `--theme` override.
    fn set_theme(&mut self, pref: ThemePref, window: &mut Window, cx: &mut Context<Self>) {
        self.store.update(cx, |s, cx| {
            s.change_prefs(|p| p.theme = pref);
            cx.notify();
        });
        cx.set_global(ThemeOverride(None));
        theme::apply(pref, window, cx);
    }

    fn field(
        p: &Palette,
        label: &'static str,
        hint: &'static str,
        input: Entity<TextInput>,
    ) -> Div {
        div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(caption(p, label))
            .child(input)
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(rgb(p.muted))
                    .child(hint),
            )
    }

    fn client_card(&self, p: &Palette, store: &Store, cx: &Context<Self>) -> Div {
        let demo = store.mode == Mode::Demo;
        let stored = store.client_id.is_some();
        let secret_masked = self.client_secret.read(cx).is_masked();
        let status = match (stored, demo) {
            (_, true) => STATUS_DEMO,
            (false, false) if store.auth == AuthState::Locked => STATUS_LOCKED,
            (true, false) => STATUS_SAVED,
            (false, false) if self.builtin => STATUS_BUILTIN,
            (false, false) => STATUS_NONE,
        };
        let ready = stored || self.builtin;
        card(p, "Google OAuth client")
            .child(body_text(
                p,
                if self.builtin {
                    CLIENT_INTRO_BUILTIN
                } else {
                    CLIENT_INTRO_OWN
                },
            ))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .child(button(
                        p,
                        "open-gmail-api",
                        "Enable the Gmail API",
                        ButtonStyle::Secondary,
                        Some(Box::new(|_, _, cx| cx.open_url(GMAIL_API_URL))),
                    ))
                    .child(button(
                        p,
                        "open-guide",
                        "Setup guide",
                        ButtonStyle::Secondary,
                        Some(Box::new(|_, _, cx| cx.open_url(GUIDE_URL))),
                    )),
            )
            .child(Self::field(
                p,
                "CLIENT ID",
                "Ends in .apps.googleusercontent.com",
                self.client_id.clone(),
            ))
            .child(
                div()
                    .flex()
                    .items_end()
                    .gap_2()
                    .child(div().flex_1().child(Self::field(
                        p,
                        "CLIENT SECRET",
                        "Never shown again after saving",
                        self.client_secret.clone(),
                    )))
                    .child(div().pb(px(22.0)).child(button(
                        p,
                        "toggle-secret",
                        if secret_masked { "Show" } else { "Hide" },
                        ButtonStyle::Secondary,
                        Some(self.click(cx, |this, _, cx| {
                            this.client_secret.update(cx, |i, cx| {
                                let masked = i.is_masked();
                                i.set_masked(!masked, cx)
                            })
                        })),
                    ))),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().size(px(7.0)).rounded_full().bg(rgb(if ready {
                                p.green
                            } else {
                                p.amber
                            })))
                            .child(
                                div()
                                    .text_size(px(11.5))
                                    .text_color(rgb(p.secondary))
                                    .child(status),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .when(stored && self.builtin, |d| {
                                d.child(button(
                                    p,
                                    "use-builtin",
                                    "Use built-in client",
                                    ButtonStyle::Secondary,
                                    Some(self.send(cx, || Command::ClearClient)),
                                ))
                            })
                            .child(button(
                                p,
                                "save-client",
                                "Save to keyring",
                                ButtonStyle::Primary,
                                Some(self.click(cx, |this, _, cx| this.save_client(cx))),
                            )),
                    ),
            )
    }

    fn accounts_card(&self, p: &Palette, store: &Store, cx: &Context<Self>) -> Div {
        let intro = match store.auth {
            AuthState::Starting => "Reading the keyring…",
            AuthState::Locked => "The keyring is locked. Unlock it and press Ctrl+R.",
            AuthState::NeedsClient => "Set up an OAuth client above, then sign in.",
            AuthState::Demo => "Demo mode: these accounts are generated.",
            AuthState::Ready if store.accounts.is_empty() => "No account yet.",
            AuthState::Ready => "Removing an account signs it out and revokes Eloquawk's access.",
        };
        let can_change = matches!(store.auth, AuthState::Ready | AuthState::Demo);
        let rows = store.accounts.iter().map(|email| {
            let target = email.clone();
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap_3()
                .rounded_md()
                .border_1()
                .border_color(rgb(p.border))
                .bg(rgb(p.panel_raised))
                .px_3()
                .py_2()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .min_w(px(0.0))
                        .child(
                            div()
                                .size(px(7.0))
                                .flex_shrink_0()
                                .rounded_full()
                                .bg(rgb(p.green)),
                        )
                        .child(
                            div()
                                .truncate()
                                .text_size(px(12.5))
                                .text_color(rgb(p.text))
                                .child(email.clone()),
                        ),
                )
                .child(button(
                    p,
                    SharedString::from(format!("remove-{email}")),
                    "Remove",
                    ButtonStyle::Secondary,
                    can_change.then(|| {
                        self.click(cx, move |this, _, cx| {
                            this.store
                                .read(cx)
                                .send_command(Command::RemoveAccount(target.clone()))
                        })
                    }),
                ))
        });
        let footer = match store.signing_in.clone() {
            Some(url) => div()
                .flex()
                .flex_col()
                .gap_2()
                .child(body_text(p, SIGN_IN_PENDING))
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .child(button(
                            p,
                            "copy-sign-in",
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
                            Some(self.send(cx, || Command::CancelSignIn)),
                        )),
                ),
            None => div().flex().child(button(
                p,
                "add-account",
                if store.accounts.is_empty() {
                    "Sign in with Google"
                } else {
                    "Add account"
                },
                ButtonStyle::Primary,
                can_change.then(|| self.send(cx, || Command::AddAccount)),
            )),
        };
        card(p, "Accounts")
            .child(body_text(p, intro))
            .child(div().flex().flex_col().gap_2().children(rows))
            .child(footer)
    }

    fn prefs_card(&self, p: &Palette, store: &Store, cx: &Context<Self>) -> Div {
        let prefs = &store.prefs;
        let theme_choices = [
            (ThemePref::System, "System"),
            (ThemePref::Light, "Light"),
            (ThemePref::Dark, "Dark"),
        ];
        let theme_row = segments(
            p,
            theme_choices.into_iter().map(|(pref, label)| {
                segment(
                    p,
                    SharedString::from(format!("theme-{label}")),
                    label,
                    prefs.theme == pref,
                    self.click(cx, move |this, window, cx| this.set_theme(pref, window, cx)),
                )
            }),
        );
        let refresh_row = segments(
            p,
            REFRESH_CHOICES.iter().map(|&(secs, label)| {
                segment(
                    p,
                    SharedString::from(format!("refresh-{secs}")),
                    label,
                    prefs.refresh_secs == secs,
                    self.pref(cx, move |p| p.refresh_secs = secs),
                )
            }),
        );
        let labelled = |label: &'static str, control: Div| {
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap_3()
                .px_2()
                .child(
                    div()
                        .text_size(px(12.5))
                        .text_color(rgb(p.text))
                        .child(label),
                )
                .child(control)
        };
        card(p, "Preferences")
            .child(labelled("Theme", theme_row))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(checkbox_row(
                        p,
                        "pref-clock",
                        "24-hour clock",
                        prefs.clock_24h,
                        self.pref(cx, |p| p.clock_24h = !p.clock_24h),
                    ))
                    .child(checkbox_row(
                        p,
                        "pref-mark-read",
                        "Mark as read when opened",
                        prefs.mark_read_on_open,
                        self.pref(cx, |p| p.mark_read_on_open = !p.mark_read_on_open),
                    ))
                    .child(checkbox_row(
                        p,
                        "pref-notify",
                        "Notify about new mail",
                        prefs.notifications,
                        self.pref(cx, |p| p.notifications = !p.notifications),
                    )),
            )
            .child(labelled("Check for new mail every", refresh_row))
    }
}

impl Focusable for SettingsView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let store = self.store.read(cx);
        let client = self.client_card(p, store, cx);
        let accounts = self.accounts_card(p, store, cx);
        let prefs = self.prefs_card(p, store, cx);
        div()
            .id("settings")
            .key_context("Settings")
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &Submit, _, cx| this.save_client(cx)))
            .on_action(cx.listener(|_, _: &CloseSettings, _, cx| cx.emit(SettingsEvent::Close)))
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .flex()
            .justify_center()
            .child(
                div()
                    .w_full()
                    .max_w(px(720.0))
                    .flex()
                    .flex_col()
                    .gap_4()
                    .p_6()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_size(px(18.0))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(rgb(p.text))
                                    .child("Settings"),
                            )
                            .child(ghost_button(
                                p,
                                "close-settings",
                                "✕",
                                Some(Box::new(cx.listener(|_, _: &ClickEvent, _, cx| {
                                    cx.emit(SettingsEvent::Close)
                                }))),
                            )),
                    )
                    .child(client)
                    .child(accounts)
                    .child(prefs),
            )
    }
}
