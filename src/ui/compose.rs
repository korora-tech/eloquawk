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
    PathPromptOptions, SharedString, Subscription, Task, Window, div, px, rgb,
};

use super::text_area::TextArea;
use super::text_input::{Submit, TextInput};
use super::theme::{self, Palette, palette, tint};
use super::widgets::{ButtonStyle, OnClick, button, checkbox, ghost_button};
use super::{CloseCompose, DiscardCompose, Quit, Send as SendMessage};
use crate::demo_script::{ComposeField, Cond};
use crate::mail::address::{format_list, parse_list};
use crate::mail::compose::{TOO_LARGE_TO_ATTACH, fits, mime_for, valid_email};
use crate::mail::{reply, signature};
use crate::model::{
    AccountId, Address, AttachmentSource, ComposeKind, Outgoing, OutgoingAttachment, ThreadDetail,
};
use crate::protocol::OpId;
use crate::store::{DraftOp, SendState, Store};
use crate::ui::format::{full_date, size_human};
use jiff::ZonedDateTime;
use std::time::Duration;

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
    let now = ZonedDateTime::now();
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

/// One address a message can be sent from: an account's own, or one of its send-as aliases.
#[derive(Clone, Debug, PartialEq)]
struct Identity {
    account: AccountId,
    address: Address,
    /// Plain-text signature; empty when there is none.
    signature: String,
}

/// Every identity of every signed-in account, each account's default first. An account whose
/// send-as list has not arrived yet is just its own address with no signature.
fn identities(store: &Store) -> Vec<Identity> {
    let mut all = Vec::new();
    for account in &store.accounts {
        match store.send_as.get(account).filter(|l| !l.is_empty()) {
            Some(list) => all.extend(list.iter().map(|a| Identity {
                account: account.clone(),
                address: a.address(),
                signature: a.signature.clone(),
            })),
            None => all.push(Identity {
                account: account.clone(),
                address: Address::new(None, account),
                signature: String::new(),
            }),
        }
    }
    all
}

/// The identity a prefill sends from: its `from` address when the account has it, else the
/// account's default.
fn initial_identity(all: &[Identity], prefill: &Outgoing) -> Option<Identity> {
    let mine = || all.iter().filter(|i| i.account == prefill.account);
    mine()
        .find(|i| {
            i.address.email.eq_ignore_ascii_case(&prefill.from.email)
                && prefill.from.email != prefill.account
        })
        .or_else(|| mine().next())
        .cloned()
}

/// Where the send stands.
#[derive(Clone, Debug, PartialEq)]
enum Sending {
    Editing,
    /// Held for the undo window: Undo brings the message back, closing the window does not.
    Pending(OpId),
    InFlight(OpId),
    Failed {
        message: String,
        maybe_sent: bool,
    },
}

/// What to do once the user agrees to discard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Leave {
    Close,
    Quit,
}

/// The field texts as opened, to tell whether anything was edited.
#[derive(Clone, Default, PartialEq)]
struct Snapshot {
    account: AccountId,
    from: String,
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
    /// The address in From: the account's own or one of its aliases.
    from: Address,
    /// The signature currently in the body for `from`, so changing From can swap it.
    signature: String,
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
    /// Gmail's draft for this message, once one has been saved or opened.
    draft_id: Option<String>,
    /// A draft save in flight.
    saving: Option<OpId>,
    /// What the draft on Gmail holds, to tell whether there is anything new to save.
    saved: Snapshot,
    /// The snapshot the save in flight carries.
    saved_pending: Option<Snapshot>,
    /// A draft save failed: shown inline until the next one works.
    draft_error: Option<String>,
    /// The user asked to save and close: close once the draft has caught up.
    close_when_saved: Option<Leave>,
    /// The account `draft_id` belongs to: changing From to another account needs a new draft.
    draft_account: AccountId,
    /// The autosave timer; replaced by every edit.
    autosave_timer: Option<Task<()>>,
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
        draft_id: Option<String>,
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
        let identity = initial_identity(&identities(store.read(cx)), &prefill);
        let (from, signature) = identity.map_or_else(
            || (prefill.from.clone(), String::new()),
            |i| (i.address, i.signature),
        );
        // An opened draft already holds whatever signature it had.
        let text = if draft_id.is_some() {
            prefill.body.clone()
        } else {
            signature::insert(&prefill.body, &signature, prefill.kind)
        };
        let body = cx.new(|cx| TextArea::new(&text, "", 5, cx));
        let accounts = store.read(cx).accounts.clone();
        let mut subscriptions = vec![cx.observe_in(&store, window, |this, store, window, cx| {
            this.on_store(&store, window, cx)
        })];
        for input in [&to, &cc, &bcc, &subject] {
            subscriptions.push(cx.observe(input, |this, _, cx| {
                this.validate(cx);
                this.schedule_autosave(cx);
            }));
        }
        subscriptions.push(cx.observe(&body, |this, _, cx| {
            this.schedule_autosave(cx);
            cx.notify();
        }));
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
            from,
            signature,
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
            draft_id,
            saving: None,
            saved: Snapshot::default(),
            saved_pending: None,
            draft_error: None,
            close_when_saved: None,
            draft_account: prefill.account.clone(),
            autosave_timer: None,
            send_anyway: false,
            confirm: None,
            discarded: false,
            opened: OPENED.fetch_add(1, Ordering::Relaxed),
            baseline: Snapshot {
                account: String::new(),
                from: String::new(),
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
        this.saved = this.baseline.clone();
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
            from: self.from.email.clone(),
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
        !self.discarded
            && !matches!(self.sending, Sending::Pending(_) | Sending::InFlight(_))
            && self.edited(cx)
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
        if let Sending::Pending(op) | Sending::InFlight(op) = self.sending {
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
            Sending::Pending(_) | Sending::InFlight(_) => false,
            Sending::Failed { maybe_sent, .. } => !maybe_sent || self.send_anyway,
        };
        state_ok && self.confirm.is_none() && self.recipients_ok(cx)
    }

    fn outgoing(&self, cx: &App) -> Outgoing {
        Outgoing {
            account: self.account.clone(),
            kind: self.kind,
            from: self.from.clone(),
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
        let (op, wait) = self.store.update(cx, |s, store_cx| {
            let (op, wait) = s.send_undoable(out);
            if let Some(wait) = wait {
                // The store's task, so the message still leaves if this window is closed.
                store_cx
                    .spawn(async move |this, cx| {
                        cx.background_executor().timer(wait).await;
                        let _ = this.update(cx, |s, cx| {
                            s.dispatch_scheduled(op);
                            cx.notify();
                        });
                    })
                    .detach();
            }
            store_cx.notify();
            (op, wait)
        });
        self.sending = if wait.is_some() {
            Sending::Pending(op)
        } else {
            Sending::InFlight(op)
        };
        if let Some(id) = self.draft_id.clone() {
            let account = self.draft_account.clone();
            self.store
                .update(cx, |s, _| s.note_send_draft(op, &account, id));
        }
        self.autosave_timer = None;
        self.send_anyway = false;
        cx.notify();
    }

    /// Undo while the message is held: it comes back as it was, ready to edit and send again.
    fn undo_send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Sending::Pending(op) = self.sending else {
            return;
        };
        let taken = self.store.update(cx, |s, cx| {
            let taken = s.cancel_send(op);
            cx.notify();
            taken
        });
        // Too late: the window ended a moment ago and the message is on its way.
        if taken.is_some() {
            self.sending = Sending::Editing;
            self.schedule_autosave(cx);
            window.focus(&self.body.focus_handle(cx), cx);
        }
        cx.notify();
    }

    // ----- drafts -----------------------------------------------------------------------

    /// How long after the last edit the draft is saved to Gmail.
    const AUTOSAVE_AFTER: Duration = Duration::from_secs(4);

    /// Restarts the autosave timer after an edit. Nothing is saved while a send is held or on
    /// its way, and a message nobody has touched is never saved.
    fn schedule_autosave(&mut self, cx: &mut Context<Self>) {
        if matches!(self.sending, Sending::Pending(_) | Sending::InFlight(_)) {
            return;
        }
        self.autosave_timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Self::AUTOSAVE_AFTER).await;
            let _ = this.update(cx, |this, cx| this.autosave(cx));
        }));
    }

    /// Saves the message as Gmail's draft when it differs from what Gmail has. One save at a
    /// time: an edit during a save is picked up when it lands.
    fn autosave(&mut self, cx: &mut Context<Self>) {
        if self.saving.is_some()
            || matches!(self.sending, Sending::Pending(_) | Sending::InFlight(_))
            || self.discarded
        {
            return;
        }
        let now = self.snapshot(cx);
        if now == self.saved || now == self.baseline && self.draft_id.is_none() {
            return;
        }
        // A draft belongs to one account: after From moved to another, the old one goes.
        if self.draft_account != self.account
            && let Some(id) = self.draft_id.take()
        {
            let account = self.draft_account.clone();
            self.store.read(cx).delete_draft(&account, &id);
        }
        let out = self.outgoing(cx);
        let draft_id = self.draft_id.clone();
        self.saving = Some(self.store.update(cx, |s, cx| {
            let op = s.save_draft(out, draft_id);
            cx.notify();
            op
        }));
        self.saved_pending = Some(now);
    }

    /// Reads the outcome of the draft save in flight.
    fn on_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(op) = self.saving else {
            return;
        };
        let outcome = match self.store.read(cx).draft_op(op) {
            Some(DraftOp::Saved(id)) => Ok(id.clone()),
            Some(DraftOp::Failed(message)) => Err(message.clone()),
            _ => return,
        };
        self.store.update(cx, |s, _| s.forget_draft_op(op));
        self.saving = None;
        let snapshot = self.saved_pending.take();
        match outcome {
            Ok(id) => {
                self.draft_id = Some(id);
                self.draft_account = self.account.clone();
                if let Some(snapshot) = snapshot {
                    self.draft_error = None;
                    // The account may have changed mid-save: the draft is then stale.
                    self.saved = snapshot;
                }
                // A send started meanwhile must not leave this draft behind.
                if let Sending::Pending(send) | Sending::InFlight(send) = self.sending
                    && let Some(id) = self.draft_id.clone()
                {
                    let account = self.draft_account.clone();
                    self.store
                        .update(cx, |s, _| s.note_send_draft(send, &account, id));
                }
                if let Some(how) = self.close_when_saved {
                    if self.snapshot(cx) == self.saved {
                        self.on_close_saved(how, window, cx);
                        return;
                    }
                    self.autosave(cx);
                } else if self.snapshot(cx) != self.saved {
                    self.schedule_autosave(cx);
                }
            }
            Err(message) => {
                self.draft_error = Some(message);
                self.close_when_saved = None;
            }
        }
        cx.notify();
    }

    /// Save draft and leave: the draft is already on Gmail, so nothing is discarded.
    fn save_and_leave(&mut self, how: Leave, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm = None;
        self.close_when_saved = Some(how);
        self.autosave_timer = None;
        if self.saving.is_none() && self.snapshot(cx) == self.saved {
            self.on_close_saved(how, window, cx);
        } else {
            self.autosave(cx);
        }
        cx.notify();
    }

    fn on_close_saved(&mut self, how: Leave, window: &mut Window, cx: &mut Context<Self>) {
        self.discarded = true;
        self.close_when_saved = None;
        self.finish(how, window, cx);
    }

    /// Deletes Gmail's copy when the message is discarded.
    fn drop_draft(&mut self, cx: &mut Context<Self>) {
        self.autosave_timer = None;
        if let Some(id) = self.draft_id.take() {
            let account = self.draft_account.clone();
            self.store.read(cx).delete_draft(&account, &id);
        }
    }

    fn on_store(&mut self, store: &Entity<Store>, window: &mut Window, cx: &mut Context<Self>) {
        self.accounts = store.read(cx).accounts.clone();
        self.on_draft(window, cx);
        let s = store.read(cx);
        let (Sending::Pending(op) | Sending::InFlight(op)) = self.sending else {
            return;
        };
        let failed = match s.send_state(op) {
            Some(SendState::Sent) => None,
            Some(SendState::Failed(e)) => Some(e.clone()),
            Some(SendState::Scheduled) => return,
            // The undo window ended: from here it is an ordinary send.
            Some(SendState::Sending) => {
                if matches!(self.sending, Sending::Pending(_)) {
                    self.sending = Sending::InFlight(op);
                    cx.notify();
                }
                return;
            }
            None => return,
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
                let store = self.store.clone();
                cx.defer(move |cx| request_quit(&store, cx));
            }
        }
    }

    fn discard(&mut self, _: &DiscardCompose, window: &mut Window, cx: &mut Context<Self>) {
        let how = self.confirm.take().unwrap_or(Leave::Close);
        self.drop_draft(cx);
        self.finish(how, window, cx);
    }

    fn keep(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm = None;
        self.focus_initial(window, cx);
        cx.notify();
    }

    /// Switches From to `identity`, swapping the signature block when the user has not
    /// touched it. Choosing another From is not itself an edit worth a discard prompt.
    fn set_identity(&mut self, identity: Identity, cx: &mut Context<Self>) {
        let was_edited = self.edited(cx);
        let body = self.body.read(cx).text().to_string();
        let swapped = signature::swap(&body, &self.signature, &identity.signature, self.kind);
        if swapped != body {
            self.body.update(cx, |b, cx| b.set_text(&swapped, cx));
        }
        self.account = identity.account;
        self.from = identity.address;
        self.signature = identity.signature;
        if !was_edited {
            self.baseline = self.snapshot(cx);
        }
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
            _ => self.attach_error.clone().or_else(|| {
                self.draft_error
                    .as_ref()
                    .map(|m| format!("Couldn't save the draft: {m}"))
            }),
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
        let all = identities(self.store.read(cx));
        if all.len() < 2 {
            return None;
        }
        let segments = all.into_iter().enumerate().map(|(i, identity)| {
            let selected =
                identity.account == self.account && identity.address.email == self.from.email;
            let label = identity.address.email.clone();
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
                    this.set_identity(identity.clone(), cx)
                }))
                .child(label)
        });
        Some(
            self.row(
                p,
                "From",
                div()
                    .flex()
                    .flex_wrap()
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
        let pending = matches!(self.sending, Sending::Pending(_));
        let in_flight = pending || matches!(self.sending, Sending::InFlight(_));
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
        if pending {
            bar = bar
                .child(
                    div()
                        .text_size(px(12.5))
                        .text_color(rgb(p.secondary))
                        .child("Sending soon"),
                )
                .child(button(
                    p,
                    "undo-send",
                    "Undo",
                    ButtonStyle::Primary,
                    Some(Box::new(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.undo_send(window, cx)
                    }))),
                ));
        } else if self.confirm.is_some() {
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
                    "save-draft",
                    "Save draft",
                    ButtonStyle::Secondary,
                    Some(Box::new(cx.listener(|this, _: &ClickEvent, window, cx| {
                        let how = this.confirm.unwrap_or(Leave::Close);
                        this.save_and_leave(how, window, cx)
                    }))),
                ))
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
pub fn request_quit(store: &Entity<Store>, cx: &mut App) {
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
    quit_when_settled(store, cx);
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

/// How long quitting waits for a message that is held or on its way.
const QUIT_GRACE: std::time::Duration = std::time::Duration::from_secs(20);

/// Quits once no message is held for undo or on its way to Gmail. Held messages go now, since
/// the user asked to leave; the wait is bounded so a dead network cannot keep the app open.
pub fn quit_when_settled(store: &Entity<Store>, cx: &mut App) {
    store.update(cx, |s, _| s.flush_sends());
    if !store.read(cx).sends_unsettled() {
        cx.quit();
        return;
    }
    let store = store.clone();
    cx.spawn(async move |cx| {
        let started = std::time::Instant::now();
        while started.elapsed() < QUIT_GRACE && store.read_with(cx, |s, _| s.sends_unsettled()) {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(100))
                .await;
        }
        cx.update(|cx| cx.quit());
    })
    .detach();
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
