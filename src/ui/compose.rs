//! The compose window (SPEC 8.5): a [`ComposeView`] per window with From, To, Cc, Bcc,
//! Subject, the body [`TextArea`], attachment chips and Send.
//!
//! The window is opened prefilled (new, reply, reply-all, forward or a `mailto:` link) by
//! [`super::windows::open_compose_window`]. Address fields parse with `mail/address.rs`; any
//! entry lettre would reject turns its field's border red, and Send stays disabled until the
//! fields hold at least one valid address and no invalid one. Sending hands an [`Outgoing`] to
//! the store and waits for `SendFinished`: success closes the window (the main window shows
//! "Message sent"), failure shows the error inline, and a failure that may still have sent
//! keeps Send disabled until "Send anyway" is ticked. Closing or quitting with edits asks
//! "Discard this message?" first.
//!
//! The key context is `Compose` on the root: ctrl-enter sends, escape and ctrl-w close, and
//! ctrl-q quits, each after the discard check.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use gpui::prelude::*;
use gpui::{
    AnyWindowHandle, App, ClickEvent, Context, Div, Entity, FocusHandle, Focusable, FontWeight,
    PathPromptOptions, SharedString, Subscription, Window, div, px, rgb,
};

use super::text_area::TextArea;
use super::text_input::{Submit, TextInput};
use super::theme::{self, Palette, palette, tint};
use super::widgets::{ButtonStyle, OnClick, button, checkbox, ghost_button};
use super::{CloseCompose, DiscardCompose, Quit, Send as SendMessage};
use crate::demo_script::{ComposeField, Cond};
use crate::mail::address::{format_list, parse_list};
use crate::mail::compose::{TOO_LARGE_TO_ATTACH, fits, mime_for, valid_email};
use crate::mail::reply;
use crate::model::{
    AccountId, Address, AttachmentSource, ComposeKind, Outgoing, OutgoingAttachment, ThreadDetail,
};
use crate::protocol::OpId;
use crate::store::{SendState, Store};
use crate::ui::format::{full_date, size_human};
use jiff::Zoned;

/// An empty new message from `account`.
pub fn blank(account: &AccountId) -> Outgoing {
    Outgoing {
        account: account.clone(),
        kind: ComposeKind::New,
        from: Address::new(None, account),
        to: Vec::new(),
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: String::new(),
        body: String::new(),
        thread_id: None,
        in_reply_to: None,
        references: Vec::new(),
        attachments: Vec::new(),
    }
}

/// The prefill for replying to or forwarding `detail` from its own account, with dates in
/// the local time zone and the user's clock.
pub fn reply_prefill(
    detail: &ThreadDetail,
    kind: ComposeKind,
    clock_24h: bool,
) -> Option<Outgoing> {
    let me = Address::new(None, &detail.key.account);
    let now = Zoned::now();
    reply::prefill(detail, kind, &me, &|t| full_date(t, &now, clock_24h))
}

/// The title of a compose window: "New message", or the reply's or forward's subject.
pub fn title_for(prefill: &Outgoing) -> String {
    let subject = prefill.subject.trim();
    match prefill.kind {
        ComposeKind::New => "New message".to_string(),
        _ if subject.is_empty() => "New message".to_string(),
        _ => subject.to_string(),
    }
}

/// Where the send stands.
#[derive(Clone, Debug, PartialEq)]
enum Sending {
    Editing,
    InFlight(OpId),
    Failed { message: String, maybe_sent: bool },
}

/// What to do once the user agrees to discard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Leave {
    Close,
    Quit,
}

/// The field texts as opened, to tell whether anything was edited.
#[derive(Clone, PartialEq)]
struct Snapshot {
    account: AccountId,
    to: String,
    cc: String,
    bcc: String,
    subject: String,
    body: String,
    attachments: usize,
}

/// Numbers compose windows in the order they open, so the demo script can find the newest.
static OPENED: AtomicU64 = AtomicU64::new(0);

/// How an address field parses: how many valid entries, and whether any entry is invalid.
fn check_field(text: &str) -> (usize, bool) {
    let list = parse_list(text);
    let bad = list.iter().any(|a| !valid_email(&a.email));
    (list.len(), bad)
}

pub struct ComposeView {
    store: Entity<Store>,
    root: FocusHandle,
    title: String,
    kind: ComposeKind,
    accounts: Vec<AccountId>,
    account: AccountId,
    /// The account the thread belongs to; switching From away drops the thread id.
    thread_account: AccountId,
    thread_id: Option<String>,
    in_reply_to: Option<String>,
    references: Vec<String>,
    to: Entity<TextInput>,
    cc: Entity<TextInput>,
    bcc: Entity<TextInput>,
    subject: Entity<TextInput>,
    body: Entity<TextArea>,
    show_cc: bool,
    attachments: Vec<OutgoingAttachment>,
    /// An inline error under the attachments (file chooser, size budget).
    attach_error: Option<String>,
    invalid: [bool; 3],
    sending: Sending,
    send_anyway: bool,
    confirm: Option<Leave>,
    /// The user agreed to discard this message while quitting; quit no longer asks about it.
    discarded: bool,
    /// When this window opened, relative to the other compose windows.
    opened: u64,
    baseline: Snapshot,
    _subscriptions: Vec<Subscription>,
}

impl ComposeView {
    pub fn new(
        store: Entity<Store>,
        prefill: Outgoing,
        title: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let field = |text: String, placeholder: &str, tab: isize, cx: &mut Context<Self>| {
            let input = cx.new(|cx| TextInput::new(placeholder.to_string(), false, tab, cx));
            input.update(cx, |i, cx| i.set_text(text, cx));
            input
        };
        let to = field(format_list(&prefill.to), "Recipients", 1, cx);
        let cc = field(format_list(&prefill.cc), "", 2, cx);
        let bcc = field(format_list(&prefill.bcc), "", 3, cx);
        let subject = field(prefill.subject.clone(), "Subject", 4, cx);
        let body = cx.new(|cx| TextArea::new(&prefill.body, "", 5, cx));
        let accounts = store.read(cx).accounts.clone();
        let mut subscriptions = vec![cx.observe_in(&store, window, |this, store, window, cx| {
            this.on_store(&store, window, cx)
        })];
        for input in [&to, &cc, &bcc, &subject] {
            subscriptions.push(cx.observe(input, |this, _, cx| this.validate(cx)));
        }
        subscriptions.push(cx.observe(&body, |_, _, cx| cx.notify()));
        let show_cc = !prefill.cc.is_empty() || !prefill.bcc.is_empty();
        // The window manager's close button gets the same discard check as escape.
        let weak = cx.entity().downgrade();
        window.on_window_should_close(cx, move |window, cx| {
            weak.update(cx, |this, cx| this.should_close(window, cx))
                .unwrap_or(true)
        });
        let mut this = Self {
            store,
            root: cx.focus_handle(),
            title,
            kind: prefill.kind,
            accounts,
            account: prefill.account.clone(),
            thread_account: prefill.account.clone(),
            thread_id: prefill.thread_id,
            in_reply_to: prefill.in_reply_to,
            references: prefill.references,
            to,
            cc,
            bcc,
            subject,
            body,
            show_cc,
            attachments: prefill.attachments,
            attach_error: None,
            invalid: [false; 3],
            sending: Sending::Editing,
            send_anyway: false,
            confirm: None,
            discarded: false,
            opened: OPENED.fetch_add(1, Ordering::Relaxed),
            baseline: Snapshot {
                account: String::new(),
                to: String::new(),
                cc: String::new(),
                bcc: String::new(),
                subject: String::new(),
                body: String::new(),
                attachments: 0,
            },
            _subscriptions: subscriptions,
        };
        this.baseline = this.snapshot(cx);
        this.validate(cx);
        this
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    /// Focuses To for a new message or a forward, and the body for a reply.
    pub fn focus_initial(&self, window: &mut Window, cx: &mut App) {
        let handle = match self.kind {
            ComposeKind::Reply | ComposeKind::ReplyAll => self.body.focus_handle(cx),
            ComposeKind::New | ComposeKind::Forward => self.to.focus_handle(cx),
        };
        window.focus(&handle, cx);
    }

    fn snapshot(&self, cx: &App) -> Snapshot {
        Snapshot {
            account: self.account.clone(),
            to: self.to.read(cx).text().to_string(),
            cc: self.cc.read(cx).text().to_string(),
            bcc: self.bcc.read(cx).text().to_string(),
            subject: self.subject.read(cx).text().to_string(),
            body: self.body.read(cx).text().to_string(),
            attachments: self.attachments.len(),
        }
    }

    fn edited(&self, cx: &App) -> bool {
        self.snapshot(cx) != self.baseline
    }

    /// Closing or quitting should ask first: something was edited, the user has not agreed
    /// to discard it, and it is not already with the worker.
    fn needs_asking(&self, cx: &App) -> bool {
        !self.discarded && !matches!(self.sending, Sending::InFlight(_)) && self.edited(cx)
    }

    /// The window manager asks to close the window: ask about edits first.
    fn should_close(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.needs_asking(cx) {
            if self.confirm.is_none() {
                self.confirm = Some(Leave::Close);
                cx.notify();
            }
            return false;
        }
        self.abandon_send(cx);
        true
    }

    /// Hands a send still in flight over to the store, which reports its outcome as a notice
    /// once this window is gone.
    fn abandon_send(&self, cx: &mut Context<Self>) {
        if let Sending::InFlight(op) = self.sending {
            self.store.update(cx, |s, _| s.abandon_send(op));
        }
    }

    /// Re-checks the address fields after any edit.
    fn validate(&mut self, cx: &mut Context<Self>) {
        let fields = [self.to.clone(), self.cc.clone(), self.bcc.clone()];
        for (i, f) in fields.iter().enumerate() {
            let (_, bad) = check_field(f.read(cx).text());
            self.invalid[i] = bad;
            f.update(cx, |input, cx| input.set_invalid(bad, cx));
        }
        cx.notify();
    }

    fn recipients_ok(&self, cx: &App) -> bool {
        let mut valid = 0;
        for f in [&self.to, &self.cc, &self.bcc] {
            let (n, bad) = check_field(f.read(cx).text());
            if bad {
                return false;
            }
            valid += n;
        }
        valid > 0
    }

    fn can_send(&self, cx: &App) -> bool {
        let state_ok = match &self.sending {
            Sending::Editing => true,
            Sending::InFlight(_) => false,
            Sending::Failed { maybe_sent, .. } => !maybe_sent || self.send_anyway,
        };
        state_ok && self.confirm.is_none() && self.recipients_ok(cx)
    }

    fn outgoing(&self, cx: &App) -> Outgoing {
        Outgoing {
            account: self.account.clone(),
            kind: self.kind,
            from: Address::new(None, &self.account),
            to: parse_list(self.to.read(cx).text()),
            cc: parse_list(self.cc.read(cx).text()),
            bcc: parse_list(self.bcc.read(cx).text()),
            subject: self.subject.read(cx).text().to_string(),
            body: self.body.read(cx).text().to_string(),
            thread_id: self
                .thread_id
                .clone()
                .filter(|_| self.account == self.thread_account),
            in_reply_to: self.in_reply_to.clone(),
            references: self.references.clone(),
            attachments: self.attachments.clone(),
        }
    }

    fn send(&mut self, _: &SendMessage, _: &mut Window, cx: &mut Context<Self>) {
        if !self.can_send(cx) {
            return;
        }
        let out = self.outgoing(cx);
        let op = self.store.update(cx, |s, cx| {
            let op = s.send(out);
            cx.notify();
            op
        });
        self.sending = Sending::InFlight(op);
        self.send_anyway = false;
        cx.notify();
    }

    fn on_store(&mut self, store: &Entity<Store>, window: &mut Window, cx: &mut Context<Self>) {
        let s = store.read(cx);
        self.accounts = s.accounts.clone();
        let Sending::InFlight(op) = self.sending else {
            return;
        };
        let failed = match s.send_state(op) {
            Some(SendState::Sent) => None,
            Some(SendState::Failed(e)) => Some(e.clone()),
            Some(SendState::Sending) | None => return,
        };
        // This window has the outcome now; the store need not keep it.
        self.store.update(cx, |s, _| s.forget_send(op));
        match failed {
            None => window.remove_window(),
            Some(e) => {
                self.sending = Sending::Failed {
                    message: e.message,
                    maybe_sent: e.maybe_sent,
                };
                cx.notify();
            }
        }
    }

    /// Escape and ctrl-w: close, asking first when anything was edited. A second escape
    /// while asking keeps the message.
    fn close(&mut self, _: &CloseCompose, window: &mut Window, cx: &mut Context<Self>) {
        self.leave(Leave::Close, window, cx);
    }

    /// ctrl-q: quit, with the same check for this window first and then for the others.
    fn quit(&mut self, _: &Quit, window: &mut Window, cx: &mut Context<Self>) {
        self.leave(Leave::Quit, window, cx);
    }

    fn leave(&mut self, how: Leave, window: &mut Window, cx: &mut Context<Self>) {
        if self.confirm.is_some() {
            self.confirm = None;
            cx.notify();
            return;
        }
        // A message already handed to the worker is not lost by closing its window.
        if self.needs_asking(cx) {
            self.confirm = Some(how);
            cx.notify();
            return;
        }
        self.finish(how, window, cx);
    }

    fn finish(&mut self, how: Leave, window: &mut Window, cx: &mut Context<Self>) {
        match how {
            Leave::Close => {
                self.abandon_send(cx);
                window.remove_window();
            }
            // Other compose windows may hold edits too: they ask in turn before the app quits.
            Leave::Quit => {
                self.discarded = true;
                cx.defer(request_quit);
            }
        }
    }

    fn discard(&mut self, _: &DiscardCompose, window: &mut Window, cx: &mut Context<Self>) {
        let how = self.confirm.take().unwrap_or(Leave::Close);
        self.finish(how, window, cx);
    }

    fn keep(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm = None;
        self.focus_initial(window, cx);
        cx.notify();
    }

    fn set_account(&mut self, account: AccountId, cx: &mut Context<Self>) {
        self.account = account;
        cx.notify();
    }

    // ----- attachments ------------------------------------------------------------------

    /// Adds local files, rejecting any that would take the total past 25 MB.
    pub fn attach_paths(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        self.attach_error = None;
        for path in paths {
            let meta = match std::fs::metadata(&path) {
                Ok(m) if m.is_file() => m,
                Ok(_) => {
                    self.attach_error = Some("Only files can be attached".into());
                    continue;
                }
                Err(e) => {
                    eprintln!(
                        "eloquawk: could not read an attachment's size: {}",
                        e.kind()
                    );
                    self.attach_error = Some("Couldn't read that file".into());
                    continue;
                }
            };
            if !fits(&self.attachments, meta.len()) {
                self.attach_error = Some(TOO_LARGE_TO_ATTACH.into());
                continue;
            }
            let filename = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "attachment".into());
            self.attachments.push(OutgoingAttachment {
                mime_type: mime_for(&filename).to_string(),
                filename,
                size: meta.len(),
                source: AttachmentSource::Local(path),
            });
        }
        cx.notify();
    }

    fn choose_files(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Attach".into()),
        });
        cx.spawn(async move |this, cx| {
            let result = rx.await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(Ok(Some(paths))) => this.attach_paths(paths, cx),
                Ok(Ok(None)) => {}
                Ok(Err(e)) => {
                    eprintln!("eloquawk: the file chooser failed: {e}");
                    this.attach_error = Some("Couldn't open the file chooser".into());
                    cx.notify();
                }
                Err(_) => {
                    this.attach_error = Some("Couldn't open the file chooser".into());
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn remove_attachment(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix < self.attachments.len() {
            self.attachments.remove(ix);
            self.attach_error = None;
            cx.notify();
        }
    }

    // ----- demo script ------------------------------------------------------------------

    fn focused_field(&self, window: &Window, cx: &App) -> Option<ComposeField> {
        let fields = [
            (&self.to.focus_handle(cx), ComposeField::To),
            (&self.cc.focus_handle(cx), ComposeField::Cc),
            (&self.bcc.focus_handle(cx), ComposeField::Bcc),
            (&self.subject.focus_handle(cx), ComposeField::Subject),
            (&self.body.focus_handle(cx), ComposeField::Body),
        ];
        fields
            .into_iter()
            .find(|(h, _)| h.is_focused(window))
            .map(|(_, f)| f)
    }

    /// Checks the compose conditions of an `expect:` step.
    pub fn check(&self, cond: &Cond, window: &Window, cx: &App) -> Option<Result<(), String>> {
        let (ok, actual) = match cond {
            Cond::Compose(want) => {
                let actual = self.focused_field(window, cx);
                (
                    actual == *want || *want == Some(ComposeField::Open),
                    actual.map_or("none", ComposeField::name).to_string(),
                )
            }
            Cond::ComposeError(want) => {
                let actual = self.error_text();
                (actual == *want, actual.unwrap_or_else(|| "none".into()))
            }
            Cond::Attached(n) => (
                self.attachments.len() == *n,
                self.attachments.len().to_string(),
            ),
            Cond::Discard(want) => (
                self.confirm.is_some() == *want,
                if self.confirm.is_some() {
                    "asking"
                } else {
                    "no"
                }
                .into(),
            ),
            _ => return None,
        };
        Some(if ok { Ok(()) } else { Err(actual) })
    }

    /// The inline error shown, if any: a failed send, else an attachment problem.
    fn error_text(&self) -> Option<String> {
        match &self.sending {
            Sending::Failed { message, .. } => Some(message.clone()),
            _ => self.attach_error.clone(),
        }
    }

    // ----- rendering --------------------------------------------------------------------

    fn row(&self, p: &Palette, label: &str, field: Div) -> Div {
        div()
            .flex()
            .items_center()
            .gap(px(10.0))
            .child(
                div()
                    .w(px(58.0))
                    .flex_shrink_0()
                    .text_size(px(12.5))
                    .text_color(rgb(p.secondary))
                    .child(label.to_string()),
            )
            .child(field.flex_1().min_w(px(0.0)))
    }

    fn sender_row(&self, p: &Palette, cx: &Context<Self>) -> Option<Div> {
        if self.accounts.len() < 2 {
            return None;
        }
        let segments = self.accounts.iter().enumerate().map(|(i, a)| {
            let selected = *a == self.account;
            let account = a.clone();
            div()
                .id(("from", i))
                .px(px(10.0))
                .h(px(26.0))
                .flex()
                .items_center()
                .rounded(px(5.0))
                .text_size(px(12.5))
                .when(selected, |d| {
                    d.bg(rgb(tint(p.accent, p.bg, 0.16)))
                        .text_color(rgb(p.accent_text))
                        .font_weight(FontWeight::MEDIUM)
                })
                .when(!selected, |d| {
                    d.text_color(rgb(p.secondary))
                        .hover(|s| s.bg(rgb(p.hover)).cursor_pointer())
                })
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.set_account(account.clone(), cx)
                }))
                .child(a.clone())
        });
        Some(
            self.row(
                p,
                "From",
                div()
                    .flex()
                    .gap(px(4.0))
                    .p(px(2.0))
                    .rounded(px(7.0))
                    .border_1()
                    .border_color(rgb(p.border))
                    .children(segments),
            ),
        )
    }

    fn chips(&self, p: &Palette, cx: &Context<Self>) -> Option<Div> {
        if self.attachments.is_empty() {
            return None;
        }
        let chips = self.attachments.iter().enumerate().map(|(i, a)| {
            div()
                .flex()
                .items_center()
                .gap(px(6.0))
                .pl(px(10.0))
                .pr(px(2.0))
                .h(px(28.0))
                .rounded(px(6.0))
                .border_1()
                .border_color(rgb(p.border))
                .bg(rgb(p.panel_raised))
                .text_size(px(12.0))
                .child(
                    div()
                        .max_w(px(220.0))
                        .truncate()
                        .text_color(rgb(p.text))
                        .child(a.filename.clone()),
                )
                .child(div().text_color(rgb(p.muted)).child(size_human(a.size)))
                .child(ghost_button(
                    p,
                    ("remove-attachment", i),
                    "✕",
                    Some(Box::new(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.remove_attachment(i, cx)
                    }))),
                ))
        });
        Some(div().flex().flex_wrap().gap(px(6.0)).children(chips))
    }

    fn footer(&self, p: &Palette, cx: &Context<Self>) -> Div {
        let in_flight = matches!(self.sending, Sending::InFlight(_));
        let send: Option<OnClick> = self.can_send(cx).then(|| -> OnClick {
            Box::new(
                cx.listener(|this, _: &ClickEvent, window, cx| this.send(&SendMessage, window, cx)),
            )
        });
        let attach: Option<OnClick> = (!in_flight && self.confirm.is_none()).then(|| -> OnClick {
            Box::new(cx.listener(|this, _: &ClickEvent, _, cx| this.choose_files(cx)))
        });
        let mut bar = div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(button(
                p,
                "send",
                if in_flight { "Sending…" } else { "Send" },
                ButtonStyle::Primary,
                send,
            ))
            .child(button(
                p,
                "attach",
                "Attach…",
                ButtonStyle::Secondary,
                attach,
            ))
            .child(div().flex_1());
        if self.confirm.is_some() {
            bar = bar
                .child(
                    div()
                        .text_size(px(12.5))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(rgb(p.text))
                        .child("Discard this message?"),
                )
                .child(button(
                    p,
                    "discard",
                    "Discard",
                    ButtonStyle::Secondary,
                    Some(Box::new(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.discard(&DiscardCompose, window, cx)
                    }))),
                ))
                .child(button(
                    p,
                    "keep",
                    "Keep",
                    ButtonStyle::Primary,
                    Some(Box::new(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.keep(window, cx)
                    }))),
                ));
        } else if let Sending::Failed {
            maybe_sent: true, ..
        } = self.sending
        {
            let checked = self.send_anyway;
            bar = bar
                .child(checkbox(
                    p,
                    "send-anyway",
                    checked,
                    Box::new(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.send_anyway = !this.send_anyway;
                        cx.notify();
                    })),
                ))
                .child(
                    div()
                        .text_size(px(12.5))
                        .text_color(rgb(p.text))
                        .child("Send anyway"),
                );
        }
        bar
    }
}

impl Focusable for ComposeView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.root.clone()
    }
}

impl Render for ComposeView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let error = self.error_text();
        let show_cc = self.show_cc;
        div()
            .key_context("Compose")
            .track_focus(&self.root)
            .on_action(cx.listener(Self::send))
            .on_action(cx.listener(Self::close))
            .on_action(cx.listener(Self::quit))
            .on_action(cx.listener(Self::discard))
            // Enter in a one-line field moves on to the next field.
            .on_action(cx.listener(|_, _: &Submit, window, cx| window.focus_next(cx)))
            .when_some(theme::ui_font(cx), |d, font| d.font_family(font))
            .size_full()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .p(px(16.0))
            .bg(rgb(p.bg))
            .text_color(rgb(p.text))
            .children(self.sender_row(p, cx))
            .child(self.row(p, "To", div().child(self.to.clone())).child(
                div().w(px(64.0)).flex_shrink_0().when(!show_cc, |d| {
                    d.child(ghost_button(
                        p,
                        "show-cc",
                        "Cc Bcc",
                        Some(Box::new(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.show_cc = true;
                            let h = this.cc.focus_handle(cx);
                            window.focus(&h, cx);
                            cx.notify();
                        }))),
                    ))
                }),
            ))
            .when(show_cc, |d| {
                d.child(self.row(p, "Cc", div().child(self.cc.clone())))
                    .child(self.row(p, "Bcc", div().child(self.bcc.clone())))
            })
            .child(self.row(p, "Subject", div().child(self.subject.clone())))
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .mt(px(4.0))
                    .px(px(12.0))
                    .py(px(10.0))
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(p.input_border))
                    .bg(rgb(p.panel))
                    .child(self.body.clone()),
            )
            .children(self.chips(p, cx))
            .when_some(error, |d, e| {
                d.child(
                    div()
                        .text_size(px(12.5))
                        .text_color(rgb(p.red))
                        .child(SharedString::from(e)),
                )
            })
            .child(self.footer(p, cx))
    }
}

/// The compose window behind `handle`, if it is one.
pub fn compose_view(handle: AnyWindowHandle, cx: &App) -> Option<Entity<ComposeView>> {
    let window = handle.downcast::<ComposeView>()?;
    window.entity(cx).ok()
}

/// Quits, unless a compose window holds edits: the first such window comes forward and asks
/// "Discard this message?", and agreeing there carries on with the quit.
pub fn request_quit(cx: &mut App) {
    for handle in cx.windows() {
        let Some(view) = compose_view(handle, cx) else {
            continue;
        };
        if !view.read(cx).needs_asking(cx) {
            continue;
        }
        let asked = handle.update(cx, |_, window, cx| {
            view.update(cx, |this, cx| {
                this.confirm = Some(Leave::Quit);
                cx.notify();
            });
            window.activate_window();
        });
        if asked.is_ok() {
            return;
        }
    }
    cx.quit();
}

/// The compose window opened last, if any: the demo script's target when no window has the
/// compositor's focus (an idle or locked session gives none to new windows).
pub fn newest_compose_window(cx: &App) -> Option<AnyWindowHandle> {
    cx.windows()
        .into_iter()
        .filter_map(|w| compose_view(w, cx).map(|v| (v.read(cx).opened, w)))
        .max_by_key(|(opened, _)| *opened)
        .map(|(_, w)| w)
}

/// How many compose windows are open.
pub fn open_count(cx: &App) -> usize {
    cx.windows()
        .into_iter()
        .filter(|w| w.downcast::<ComposeView>().is_some())
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_fields_count_valid_and_flag_invalid() {
        assert_eq!(check_field(""), (0, false));
        assert_eq!(
            check_field("Zoë <zoe@example.org>, b@example.org"),
            (2, false)
        );
        assert_eq!(check_field("zoe@example.org, not an address"), (2, true));
        assert_eq!(check_field("\"Last, First\" <lf@example.org>"), (1, false));
        assert_eq!(check_field("a@"), (1, true));
    }

    #[test]
    fn titles_follow_the_kind() {
        let mut out = crate::mail::mailto::parse(
            "mailto:a@example.org?subject=Hello",
            &"you@example.com".to_string(),
            Address::new(None, "you@example.com"),
        )
        .unwrap();
        assert_eq!(title_for(&out), "New message");
        out.kind = ComposeKind::Reply;
        out.subject = "Re: Lunch".into();
        assert_eq!(title_for(&out), "Re: Lunch");
        out.kind = ComposeKind::Forward;
        out.subject = "  ".into();
        assert_eq!(title_for(&out), "New message");
    }
}
