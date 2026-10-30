//! The Settings screen (SPEC 8.4): the Google OAuth client, the accounts, the preferences and
//! the message checks (SPEC 16.12.4). It replaces the panes in the main window; escape or ✕
//! closes it.
//!
//! The OpenRouter key is write-only too: it goes to the AI worker, which alone touches the
//! keyring. Its field is masked and cleared on Save, on Remove and when Settings closes, and
//! the UI only ever shows the key's hint.
//!
//! The client secret is write-only from the UI's point of view: it goes to the worker, which
//! stores it in the keyring; the field is cleared afterwards and never pre-filled. Preference
//! changes update `store.prefs` and are saved only when the prefs are not ephemeral (demo).

use gpui::prelude::*;
use gpui::{
    App, ClickEvent, ClipboardItem, Context, Div, ElementId, Entity, EventEmitter, FocusHandle,
    Focusable, FontWeight, ScrollHandle, SharedString, Stateful, Subscription, Window, div, px,
    rgb,
};

use super::CloseSettings;
use super::text_input::{Submit, TextInput};
use super::theme::{self, Palette, ThemeOverride, palette};
use super::widgets::{
    ButtonStyle, OnClick, body_text, button, caption, card, checkbox_row, ghost_button, segment,
    segments,
};
use crate::ai::openrouter::{ApiKey, KEY_INVALID};
use crate::ai::{AiCommand, DISCLOSURE_VERSION, KeyState};
use crate::config::{SPEND_LIMIT_CENTS_MAX, SPEND_LIMIT_CENTS_MIN, ThemePref};
use crate::google::oauth;
use crate::protocol::{AuthState, Command};
use crate::store::{Mode, Store};

/// Enabling the Gmail API in the user's Google Cloud project.
const GMAIL_API_URL: &str = "https://console.cloud.google.com/apis/library/gmail.googleapis.com";
/// The Gmail setup guide, `docs/google-gmail.md` on GitHub (SPEC 13, M15).
pub(super) const GUIDE_URL: &str =
    "https://github.com/korora-tech/eloquawk/blob/main/docs/google-gmail.md";

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

/// Where OpenRouter keys are made.
const KEYS_URL: &str = "https://openrouter.ai/settings/keys";
const OPENROUTER_PRIVACY: &str = "https://openrouter.ai/privacy";
const TYPESAFE_PRIVACY: &str = "https://typesafe.ai/legal/privacy-policy";
const AI_INTRO: &str = concat!(
    "Uses TypeSafe's Jev model through OpenRouter to flag phishing, scams and spam. Off until ",
    "you add a key and turn it on for an account. Your OpenRouter account's logging and ",
    "Broadcast settings apply to these requests, so a dedicated key with those off is best.",
);
const AI_DEMO: &str = "Demo mode uses a built-in offline checker. Nothing is sent.";
const BACKGROUND_CAPTION: &str = concat!(
    "Sends each new inbox message in the accounts you've turned on to OpenRouter as it ",
    "arrives, including ones you never open, and uses credit for each. Warnings then show in ",
    "the list before you open a message.",
);
const SPEND_ERROR: &str = "Enter an amount from $0.10 to $100";
const STALE_CONSENT: &str = "What Eloquawk sends has changed. Review and turn on again.";
/// The disclosure shown before an account is turned on (SPEC 16.2, `DISCLOSURE_VERSION` 1).
const DISCLOSURE: [&str; 4] = [
    concat!(
        "When you open a message in this account, Eloquawk sends part of it to OpenRouter, ",
        "which passes it to TypeSafe's Jev model to check for phishing, scams and spam. If you ",
        "also turn on background checks, new inbox mail in this account is sent as it arrives, ",
        "including messages you never open.",
    ),
    concat!(
        "It sends: the sender's name and address, Reply-To and Return-Path, the subject, the ",
        "sender-authentication results, the website names that links point to, the types of ",
        "any attachments, and the first 6,000 characters of the message without quoted ",
        "replies. Before sending, Eloquawk replaces web links with just their website name, ",
        "your address and the other recipients' addresses with placeholders, and one-time ",
        "codes it recognises (English wording only) with \"[code]\". Names, phone numbers and ",
        "anything else in the text are sent as written.",
    ),
    concat!(
        "It does not send the recipient list, full link addresses, attachments or their names, ",
        "or other messages in the thread.",
    ),
    concat!(
        "Requests ask for zero data retention and no training. OpenRouter also applies your ",
        "OpenRouter account's own settings: if logging or Broadcast is on for this key, ",
        "OpenRouter may store or forward these messages, so consider a dedicated key with ",
        "those off. OpenRouter screens all requests for child sexual abuse material. Each ",
        "check uses a small amount of your OpenRouter credit, and checks stop for the session ",
        "when they reach the spend limit you set.",
    ),
];

/// A dollar amount typed for the spend limit, in cents: digits with an optional `.` and up to
/// two decimals, an optional leading `$`, within $0.10 to $100.00.
pub fn parse_dollars(s: &str) -> Option<u32> {
    let s = s.trim();
    let s = s.strip_prefix('$').unwrap_or(s).trim();
    let (whole, frac) = match s.split_once('.') {
        Some((w, f)) => (w, f),
        None => (s, ""),
    };
    if whole.is_empty()
        || whole.len() > 6
        || frac.len() > 2
        || !whole.chars().all(|c| c.is_ascii_digit())
        || !frac.chars().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let mut cents: u32 = whole.parse::<u32>().ok()? * 100;
    if !frac.is_empty() {
        let f: u32 = frac.parse().ok()?;
        cents += if frac.len() == 1 { f * 10 } else { f };
    }
    (SPEND_LIMIT_CENTS_MIN..=SPEND_LIMIT_CENTS_MAX)
        .contains(&cents)
        .then_some(cents)
}

/// "1.00" for 100 cents.
fn dollars(cents: u32) -> String {
    format!("{}.{:02}", cents / 100, cents % 100)
}

/// A checkbox row that is off and cannot be clicked, with the reason under it.
fn disabled_row(p: &Palette, id: impl Into<ElementId>, label: String) -> Stateful<Div> {
    div()
        .id(id)
        .w_full()
        .flex()
        .items_center()
        .gap_2()
        .px_2()
        .py(px(5.0))
        .child(
            div()
                .size(px(14.0))
                .flex_shrink_0()
                .rounded(px(3.0))
                .border_1()
                .border_color(rgb(p.border)),
        )
        .child(
            div()
                .flex_1()
                .text_size(px(12.5))
                .line_height(px(18.0))
                .text_color(rgb(p.disabled))
                .child(label),
        )
}

/// The refresh interval choices, in seconds.
const REFRESH_CHOICES: &[(u64, &str)] = &[(60, "1 min"), (300, "5 min"), (900, "15 min")];

pub enum SettingsEvent {
    Close,
}

pub struct SettingsView {
    store: Entity<Store>,
    client_id: Entity<TextInput>,
    client_secret: Entity<TextInput>,
    /// The OpenRouter key being typed; masked, never pre-filled.
    ai_key: Entity<TextInput>,
    /// The session spend limit in dollars.
    spend: Entity<TextInput>,
    /// The spend limit last copied into its field, so typing is not overwritten.
    shown_spend: Option<u32>,
    key_error: Option<&'static str>,
    spend_error: bool,
    /// The account whose disclosure panel is open.
    disclosing: Option<String>,
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
    pub fn new(store: Entity<Store>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let client_id =
            cx.new(|cx| TextInput::new("1234567890-abc.apps.googleusercontent.com", false, 1, cx));
        let client_secret = cx.new(|cx| TextInput::new("GOCSPX-…", true, 2, cx));
        let ai_key = cx.new(|cx| TextInput::new("sk-or-…", true, 3, cx));
        let spend = cx.new(|cx| TextInput::new("1.00", false, 4, cx));
        let subscription = cx.observe(&store, |this, _, cx| {
            this.sync_from_store(cx);
            cx.notify();
        });
        // TextInput emits no blur event: leaving the spend field saves it.
        let spend_focus = spend.focus_handle(cx);
        cx.on_blur(&spend_focus, window, |this, _, cx| this.save_spend(cx))
            .detach();
        let mut this = Self {
            store,
            client_id,
            client_secret,
            ai_key,
            spend,
            shown_spend: None,
            key_error: None,
            spend_error: false,
            disclosing: None,
            shown_client: None,
            builtin: oauth::builtin_client().is_some(),
            focus: cx.focus_handle(),
            scroll: ScrollHandle::new(),
            _subscription: subscription,
        };
        this.sync_from_store(cx);
        this
    }

    /// When the stored client changes (loaded, saved, removed), reflect it in the fields; the
    /// same for the key's hint and the spend limit.
    fn sync_from_store(&mut self, cx: &mut Context<Self>) {
        let (key, limit) = {
            let s = self.store.read(cx);
            (s.ai.key.clone(), s.prefs.ai.spend_limit_cents)
        };
        let placeholder = match key {
            KeyState::Saved { hint } | KeyState::Rejected { hint } => hint,
            _ => "sk-or-…".to_string(),
        };
        self.ai_key
            .update(cx, |i, cx| i.set_placeholder(placeholder, cx));
        if self.shown_spend != Some(limit) {
            self.shown_spend = Some(limit);
            self.spend
                .update(cx, |i, cx| i.set_text(dollars(limit), cx));
        }
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

    /// Forgets a typed key: the field never holds one longer than needed.
    fn clear_key_field(&mut self, cx: &mut Context<Self>) {
        self.ai_key.update(cx, |i, cx| {
            i.set_text("", cx);
            i.set_masked(true, cx);
        });
    }

    /// Settings is closing: drop the typed key.
    pub fn closing(&mut self, cx: &mut Context<Self>) {
        self.clear_key_field(cx);
    }

    fn save_key(&mut self, cx: &mut Context<Self>) {
        let typed = self.ai_key.read(cx).text().to_string();
        if typed.trim().is_empty() {
            return;
        }
        match ApiKey::parse(&typed) {
            Ok(key) => {
                self.key_error = None;
                self.store.read(cx).send_ai(AiCommand::SaveKey(key));
                self.clear_key_field(cx);
            }
            Err(_) => self.key_error = Some(KEY_INVALID),
        }
        cx.notify();
    }

    fn test_key(&mut self, cx: &mut Context<Self>) {
        let typed = self.ai_key.read(cx).text().to_string();
        let key = if typed.trim().is_empty() {
            None
        } else {
            match ApiKey::parse(&typed) {
                Ok(k) => Some(k),
                Err(_) => {
                    self.key_error = Some(KEY_INVALID);
                    cx.notify();
                    return;
                }
            }
        };
        self.key_error = None;
        self.store.update(cx, |s, cx| {
            s.test_ai_key(key);
            cx.notify();
        });
    }

    fn save_spend(&mut self, cx: &mut Context<Self>) {
        let typed = self.spend.read(cx).text().to_string();
        match parse_dollars(&typed) {
            Some(cents) => {
                self.spend_error = false;
                self.store.update(cx, |s, cx| {
                    s.change_prefs(|p| p.ai.spend_limit_cents = cents);
                    cx.notify();
                });
                self.spend
                    .update(cx, |i, cx| i.set_text(dollars(cents), cx));
            }
            None => self.spend_error = true,
        }
        cx.notify();
    }

    /// Enter: saves whichever field it was pressed in.
    fn submit(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.ai_key.focus_handle(cx).is_focused(window) {
            self.save_key(cx);
        } else if self.spend.focus_handle(cx).is_focused(window) {
            self.save_spend(cx);
        } else {
            self.save_client(cx);
        }
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
                                    Some(self.click(cx, |this, _, cx| {
                                        this.store.update(cx, |s, cx| {
                                            s.clear_client();
                                            cx.notify();
                                        })
                                    })),
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
                            this.store.update(cx, |s, cx| {
                                s.remove_account(&target);
                                cx.notify();
                            })
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

impl SettingsView {
    fn ai_card(&self, p: &Palette, store: &Store, cx: &Context<Self>) -> Div {
        let demo = store.mode == Mode::Demo;
        let key = &store.ai.key;
        let mut c = card(p, "Message checks (OpenRouter)")
            .child(body_text(p, AI_INTRO))
            .child(div().flex().child(button(
                p,
                "ai-keys",
                "Get a key",
                ButtonStyle::Secondary,
                Some(Box::new(|_, _, cx| cx.open_url(KEYS_URL))),
            )));
        if demo {
            c = c.child(body_text(p, AI_DEMO));
        } else {
            let saved = matches!(key, KeyState::Saved { .. } | KeyState::Rejected { .. });
            let typed = !self.ai_key.read(cx).text().trim().is_empty();
            let status = match key {
                KeyState::Saved { hint } => format!("Saved ({hint})"),
                KeyState::Missing => "No key saved".into(),
                KeyState::Locked => "The keyring is locked".into(),
                KeyState::Rejected { .. } => "OpenRouter rejected this key".into(),
                KeyState::Unknown => "Checking the keyring…".into(),
                KeyState::Demo => String::new(),
            };
            let ok = matches!(key, KeyState::Saved { .. });
            c = c
                .child(
                    div()
                        .flex()
                        .items_end()
                        .gap_2()
                        .child(div().flex_1().child(Self::field(
                            p,
                            "API KEY",
                            "Stored only in your system keyring",
                            self.ai_key.clone(),
                        )))
                        .child(
                            div()
                                .pb(px(22.0))
                                .flex()
                                .gap_2()
                                .child(button(
                                    p,
                                    "ai-save-key",
                                    "Save",
                                    ButtonStyle::Primary,
                                    typed.then(|| self.click(cx, |this, _, cx| this.save_key(cx))),
                                ))
                                .child(button(
                                    p,
                                    "ai-test-key",
                                    "Test",
                                    ButtonStyle::Secondary,
                                    (typed || saved)
                                        .then(|| self.click(cx, |this, _, cx| this.test_key(cx))),
                                ))
                                .when(saved, |d| {
                                    d.child(button(
                                        p,
                                        "ai-remove-key",
                                        "Remove",
                                        ButtonStyle::Secondary,
                                        Some(self.click(cx, |this, _, cx| {
                                            this.store.read(cx).send_ai(AiCommand::ClearKey);
                                            this.clear_key_field(cx);
                                        })),
                                    ))
                                }),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(div().size(px(7.0)).rounded_full().bg(rgb(if ok {
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
                .when_some(self.key_error, |d, e| {
                    d.child(
                        div()
                            .text_size(px(11.5))
                            .text_color(rgb(p.red_text))
                            .child(e),
                    )
                })
                .when(store.ai.testing, |d| d.child(caption(p, "Testing…")))
                .when_some(store.ai.test.clone(), |d, r| {
                    let (text, colour) = match r {
                        Ok(t) => (t, p.secondary),
                        Err(e) => (e, p.red_text),
                    };
                    d.child(
                        div()
                            .text_size(px(12.0))
                            .text_color(rgb(colour))
                            .child(text),
                    )
                });
        }
        // Per-account consent.
        let toggles_on = demo
            || matches!(
                key,
                KeyState::Saved { .. } | KeyState::Rejected { .. } | KeyState::Demo
            );
        let disabled_why = match key {
            KeyState::Missing => "Add a key first",
            KeyState::Locked => "The keyring is locked",
            _ => "Checking the keyring…",
        };
        let mut accounts = div().flex().flex_col();
        for email in &store.accounts {
            let label = format!("Check messages in {email}");
            let stored = store.prefs.ai.accounts.get(email).copied();
            let on = stored == Some(DISCLOSURE_VERSION);
            let id = SharedString::from(format!("ai-account-{email}"));
            if !toggles_on {
                accounts = accounts
                    .child(disabled_row(p, id, label))
                    .child(div().pl(px(30.0)).child(caption(p, disabled_why)));
                continue;
            }
            let target = email.clone();
            accounts = accounts.child(checkbox_row(
                p,
                id,
                label,
                on,
                self.click(cx, move |this, _, cx| {
                    if on {
                        this.disclosing = None;
                        this.store.update(cx, |s, cx| {
                            s.set_ai_consent(&target, false);
                            cx.notify();
                        });
                    } else {
                        this.disclosing = Some(target.clone());
                        cx.notify();
                    }
                }),
            ));
            if stored.is_some() && !on {
                accounts = accounts.child(div().pl(px(30.0)).child(caption(p, STALE_CONSENT)));
            }
            if self.disclosing.as_deref() == Some(email.as_str()) {
                accounts = accounts.child(self.disclosure(p, email, cx));
            }
        }
        c = c.child(accounts);
        // Background checks and the spend limit.
        let prefs = &store.prefs.ai;
        c = c
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(checkbox_row(
                        p,
                        "ai-background",
                        "Check new inbox mail in the background",
                        prefs.background,
                        self.pref(cx, |p| p.ai.background = !p.ai.background),
                    ))
                    .child(div().pl(px(30.0)).child(caption(p, BACKGROUND_CAPTION))),
            )
            .child(
                div()
                    .flex()
                    .items_end()
                    .gap_2()
                    .child(div().w(px(200.0)).child(Self::field(
                        p,
                        "SPEND LIMIT",
                        "Stop checks after this much in a session ($)",
                        self.spend.clone(),
                    )))
                    .child(div().pb(px(22.0)).child(button(
                        p,
                        "ai-save-spend",
                        "Set",
                        ButtonStyle::Secondary,
                        Some(self.click(cx, |this, _, cx| this.save_spend(cx))),
                    ))),
            )
            .when(self.spend_error, |d| {
                d.child(
                    div()
                        .text_size(px(11.5))
                        .text_color(rgb(p.red_text))
                        .child(SPEND_ERROR),
                )
            });
        let (checks, cost, unknown) = store.ai.usage;
        let mut usage = format!(
            "This session: {checks} {} \u{b7} ${cost:.4} of ${}",
            if checks == 1 { "check" } else { "checks" },
            dollars(prefs.spend_limit_cents)
        );
        if unknown > 0 {
            usage.push_str(&format!(" \u{b7} cost unknown for {unknown}"));
        }
        c = c.child(body_text(p, usage));
        if let Some(reason) = store.ai.paused.clone() {
            c = c.child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .rounded_md()
                    .border_l(px(3.0))
                    .border_color(rgb(p.amber))
                    .bg(rgb(super::theme::tint(p.amber, p.panel, 0.12)))
                    .px_3()
                    .py_2()
                    .child(
                        div()
                            .text_size(px(12.5))
                            .text_color(rgb(p.text))
                            .child(format!("Checks are paused: {reason}")),
                    )
                    .child(button(
                        p,
                        "ai-resume",
                        "Try again",
                        ButtonStyle::Secondary,
                        Some(self.click(cx, |this, _, cx| {
                            this.store.read(cx).send_ai(AiCommand::ResumePaused)
                        })),
                    )),
            );
        }
        c
    }

    /// The inline disclosure under an account's toggle (SPEC 16.2).
    fn disclosure(&self, p: &Palette, email: &str, cx: &Context<Self>) -> Div {
        let target = email.to_string();
        div()
            .ml(px(30.0))
            .flex()
            .flex_col()
            .gap_2()
            .rounded_md()
            .border_1()
            .border_color(rgb(p.border))
            .bg(rgb(p.panel_raised))
            .p_3()
            .children(DISCLOSURE.iter().map(|para| body_text(p, *para)))
            .child(
                div()
                    .flex()
                    .gap_3()
                    .child(
                        div()
                            .id("ai-privacy-openrouter")
                            .text_size(px(12.0))
                            .text_color(rgb(p.link))
                            .underline()
                            .cursor_pointer()
                            .on_click(|_, _, cx| cx.open_url(OPENROUTER_PRIVACY))
                            .child("OpenRouter privacy"),
                    )
                    .child(
                        div()
                            .id("ai-privacy-typesafe")
                            .text_size(px(12.0))
                            .text_color(rgb(p.link))
                            .underline()
                            .cursor_pointer()
                            .on_click(|_, _, cx| cx.open_url(TYPESAFE_PRIVACY))
                            .child("TypeSafe privacy"),
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(button(
                        p,
                        "ai-turn-on",
                        "Turn on",
                        ButtonStyle::Primary,
                        Some(self.click(cx, move |this, _, cx| {
                            this.disclosing = None;
                            this.store.update(cx, |s, cx| {
                                s.set_ai_consent(&target, true);
                                cx.notify();
                            });
                        })),
                    ))
                    .child(button(
                        p,
                        "ai-cancel",
                        "Cancel",
                        ButtonStyle::Secondary,
                        Some(self.click(cx, |this, _, cx| {
                            this.disclosing = None;
                            cx.notify();
                        })),
                    )),
            )
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
        let ai = self.ai_card(p, store, cx);
        div()
            .id("settings")
            .key_context("Settings")
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &Submit, window, cx| this.submit(window, cx)))
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
                    .child(prefs)
                    .child(ai),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The guide the Settings card and the needs-client card open is the committed one. The
    /// Nix package's source holds no docs, so its check phase skips the file check; CI's
    /// `cargo test` runs in the checkout and makes it.
    #[test]
    fn parse_dollars_accepts_amounts_in_range() {
        for (s, cents) in [
            ("1", 100),
            ("1.00", 100),
            ("$2.5", 250),
            ("0.10", 10),
            ("100", 10_000),
        ] {
            assert_eq!(parse_dollars(s), Some(cents), "{s}");
        }
        for s in [
            "0.05", "100.01", "1.234", "-1", "", "abc", "$", ".5", "1.", "1e2",
        ] {
            if s == "1." {
                // A trailing dot is a whole number of dollars.
                assert_eq!(parse_dollars(s), Some(100));
                continue;
            }
            assert_eq!(parse_dollars(s), None, "{s}");
        }
        assert_eq!(dollars(250), "2.50");
    }

    #[test]
    fn guide_url_names_the_committed_guide() {
        let path = GUIDE_URL
            .strip_prefix("https://github.com/korora-tech/eloquawk/blob/main/")
            .expect("the guide lives on the main branch on GitHub");
        assert_eq!(path, "docs/google-gmail.md");
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        if root.join("docs").is_dir() {
            assert!(root.join(path).is_file(), "{path} is missing");
        }
    }
}
