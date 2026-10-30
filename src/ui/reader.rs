//! The reading pane (SPEC 8.2): a [`ReaderView`] entity that the main view embeds with
//! `.cached(..)`, so the frequent store notifications that do not concern the open thread do
//! not re-render it. It observes the store and notifies only when the open thread, its detail
//! or its summary changes.
//!
//! The scroll area's children are the subject block (child 0) and then exactly one child per
//! message card, so card i is child i + 1.
//!
//! Each expanded message's body is a [`BodyText`] entity (and an unfolded quote a second one),
//! kept in a map by message id and dropped when the thread closes. Link clicks from a body
//! come here: http and https open in the browser, mailto goes through the whitelisted parser
//! of `mail/mailto.rs`, and nothing else is ever opened. While the link guard is armed for a
//! message (SPEC 16.12.1), a web link asks first through a toast.
//!
//! Child 0 also carries the AI triage (SPEC 16.12): the banner, the local caution and the
//! failure or pause caption, so card i stays child i + 1.

use std::collections::{BTreeSet, HashMap, HashSet};

use gpui::prelude::*;
use gpui::{
    AnyElement, ClickEvent, ClipboardItem, Context, Div, Entity, EventEmitter, FocusHandle,
    Focusable, FontWeight, ScrollHandle, SharedString, Subscription, Window, div, point, px, rgb,
};
use jiff::Zoned;

use super::body_text::{BodyEvent, BodyText};
use super::format;
use super::theme::{Palette, palette};
use super::triage::{self, GuardDecision};
use super::widgets::{ButtonStyle, OnClick, button, caption, ghost_button, label_chip};
use crate::ai::jev::Level;
use crate::mail::address::display_name;
use crate::mail::links::LinkTarget;
use crate::mail::mailto;
use crate::mail::quote::split_quote;
use crate::model::{
    Address, Attachment, BodySource, LabelDelta, LabelKind, Message, ThreadDetail, ThreadKey,
    ThreadSummary, ViewKind, labels,
};
use crate::protocol::Command;
use crate::store::{DetailState, MutationKind, Store};

/// Gives every message not in `seen` its default expansion (unread or last) and records it,
/// leaving the expansion of messages already seen as the user toggled it.
fn expand_new(seen: &mut HashSet<String>, expanded: &mut HashSet<String>, messages: &[Message]) {
    let last = messages.len().saturating_sub(1);
    for (i, m) in messages.iter().enumerate() {
        if seen.insert(m.id.clone()) && (i == last || m.label_ids.contains(labels::UNREAD)) {
            expanded.insert(m.id.clone());
        }
    }
}

/// Bodies longer than this are cut in the reader (SPEC 8.2).
pub const MAX_BODY: usize = 200 * 1024;

/// Cuts `body` to at most `max` bytes on a char boundary; true when it was cut.
pub fn cut_body(body: &str, max: usize) -> (&str, bool) {
    if body.len() <= max {
        return (body, false);
    }
    let mut end = max;
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    (&body[..end], true)
}

/// What an expanded card shows of a message body.
#[derive(Debug, PartialEq)]
pub struct BodyParts {
    /// The body above any folded quote (the snippet when there is no body).
    pub main: String,
    /// The trailing quote that folds behind "• • •".
    pub quote: Option<String>,
    /// "Converted from HTML", "Message truncated".
    pub captions: Vec<&'static str>,
}

impl BodyParts {
    pub fn of(m: &Message) -> Self {
        let (body, cut) = cut_body(&m.body, MAX_BODY);
        let (shown, quote) = split_quote(body);
        let mut captions = Vec::new();
        if m.body_source == BodySource::Html {
            captions.push("Converted from HTML");
        }
        if cut || m.body_truncated {
            captions.push("Message truncated");
        }
        let main = if m.body_source == BodySource::Empty && shown.trim().is_empty() {
            m.snippet.clone()
        } else {
            // Trailing blank lines would only pad the card and the copied text.
            shown.trim_end().to_string()
        };
        Self {
            main,
            quote: quote.map(str::to_string),
            captions,
        }
    }

    /// The text the card's Copy button copies: what is visible.
    pub fn visible(&self, unfolded: bool) -> String {
        match (&self.quote, unfolded) {
            (Some(q), true) => format!("{}\n\n{q}", self.main),
            _ => self.main.clone(),
        }
    }
}

/// The map key of a message's quote body.
fn quote_key(id: &str) -> String {
    format!("{id}#quote")
}

/// A user label's id, name and colours, for the subject's chips.
type ChipLabel = (String, String, Option<(u32, u32)>);

/// What the reader last rendered from the store; a change means a re-render.
#[derive(Clone, PartialEq)]
struct Fingerprint {
    open: Option<ThreadKey>,
    detail: DetailSig,
    summary: Option<ThreadSummary>,
    user_labels: Vec<ChipLabel>,
    view: Option<ViewKind>,
    /// A mutation on the open thread is waiting for the worker.
    pending: bool,
    clock_24h: bool,
    /// The open thread's AI revision and the global one (SPEC 16.10).
    ai: (u64, u64),
}

#[derive(Clone, PartialEq)]
enum DetailSig {
    None,
    Loading,
    Failed(String),
    /// Message ids, their labels and body lengths.
    Loaded(Vec<(String, BTreeSet<String>, usize)>),
}

impl Fingerprint {
    fn of(store: &Store) -> Self {
        let open = store.open.clone();
        let detail = match open.as_ref().and_then(|k| store.details.get(k)) {
            None => DetailSig::None,
            Some(DetailState::Loading) => DetailSig::Loading,
            Some(DetailState::Failed(e)) => DetailSig::Failed(e.clone()),
            Some(DetailState::Loaded(d)) => DetailSig::Loaded(
                d.messages
                    .iter()
                    .map(|m| (m.id.clone(), m.label_ids.clone(), m.body.len()))
                    .collect(),
            ),
        };
        let user_labels = open
            .as_ref()
            .and_then(|k| store.labels.get(&k.account))
            .map(|ls| {
                ls.iter()
                    .filter(|l| l.kind == LabelKind::User)
                    .map(|l| (l.id.clone(), l.name.clone(), l.color))
                    .collect()
            })
            .unwrap_or_default();
        let ai = (
            open.as_ref().map_or(0, |k| store.ai_thread_rev(k)),
            store.ai.ai_rev,
        );
        Self {
            ai,
            summary: open.as_ref().and_then(|k| store.threads.get(k).cloned()),
            pending: open.as_ref().is_some_and(|k| store.is_pending(k)),
            open,
            detail,
            user_labels,
            view: store.view.as_ref().map(|v| v.kind.clone()),
            clock_24h: store.prefs.clock_24h,
        }
    }
}

/// What the reader asks of the main view.
pub enum ReaderEvent {
    /// The "Labels…" button: open the label picker for this thread.
    Labels(ThreadKey),
}

pub struct ReaderView {
    store: Entity<Store>,
    focus: FocusHandle,
    scroll: ScrollHandle,
    last: Fingerprint,
    /// Expanded message ids of the open thread.
    expanded: HashSet<String>,
    /// Message ids whose folded quote is shown.
    unfolded: HashSet<String>,
    /// Message ids of the open thread already given their default expansion, so a message
    /// that arrives later (refresh, new mail, a sent reply) gets its default too while the
    /// user's toggles on the others are kept.
    seen: HashSet<String>,
    /// Body entities by message id (quotes under [`quote_key`]), with their event
    /// subscriptions; dropped when the open thread changes.
    bodies: HashMap<String, (Entity<BodyText>, Subscription)>,
    /// The message card `n` / `p` moved to, by index; `o` / enter toggles it.
    focused_card: Option<usize>,
    _subscription: Subscription,
}

impl EventEmitter<ReaderEvent> for ReaderView {}

impl ReaderView {
    pub fn new(store: Entity<Store>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let subscription = cx.observe_in(&store, window, |this, store, window, cx| {
            let next = Fingerprint::of(store.read(cx));
            if next != this.last {
                if next.open != this.last.open {
                    this.scroll.set_offset(point(px(0.), px(0.)));
                    this.expanded.clear();
                    this.unfolded.clear();
                    this.seen.clear();
                    this.focused_card = None;
                    let keys: Vec<String> = this.bodies.keys().cloned().collect();
                    for key in keys {
                        this.drop_body(&key, window, cx);
                    }
                }
                this.last = next;
                cx.notify();
            }
        });
        let last = Fingerprint::of(store.read(cx));
        Self {
            store,
            focus: cx.focus_handle(),
            scroll: ScrollHandle::new(),
            last,
            expanded: HashSet::new(),
            unfolded: HashSet::new(),
            seen: HashSet::new(),
            bodies: HashMap::new(),
            focused_card: None,
            _subscription: subscription,
        }
    }

    fn click(
        &self,
        cx: &Context<Self>,
        f: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> OnClick {
        Box::new(cx.listener(move |this, _: &ClickEvent, window, cx| f(this, window, cx)))
    }

    /// The action bar acts on the thread it shows, never on an `x` selection the list may
    /// hold: the buttons and their labels describe this thread.
    fn mutate(&self, kind: MutationKind, cx: &mut Context<Self>) {
        self.store.update(cx, |s, cx| {
            if let Some(open) = s.open.clone() {
                s.mutate_threads(vec![open], kind);
                cx.notify();
            }
        });
    }

    /// The body entity under `key`, created on first use and kept in step with `text`.
    fn body(
        &mut self,
        key: String,
        text: String,
        secondary: bool,
        cx: &mut Context<Self>,
    ) -> Entity<BodyText> {
        if let Some((body, _)) = self.bodies.get(&key) {
            let body = body.clone();
            body.update(cx, |b, cx| b.set_text(text, cx));
            return body;
        }
        let body = cx.new(|cx| BodyText::new(text, secondary, cx));
        // The message a click came from, for the link guard: the map key without "#quote".
        let message_id = key.strip_suffix("#quote").unwrap_or(&key).to_string();
        let subscription = cx.subscribe(
            &body,
            move |this, body, event: &BodyEvent, cx| match event {
                BodyEvent::Selecting => this.clear_selections_except(&body, cx),
                BodyEvent::OpenLink(target) => this.open_link(target, &message_id, cx),
            },
        );
        self.bodies.insert(key, (body.clone(), subscription));
        body
    }

    /// Drops the body entity under `key`. A focused body takes the focus with it, which would
    /// leave the Mail context without focus (and j, k, escape dead), so the focus moves to the
    /// reader first.
    fn drop_body(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((body, _)) = self.bodies.remove(key)
            && body.focus_handle(cx).contains_focused(window, cx)
        {
            window.focus(&self.focus, cx);
        }
    }

    /// A selection spans one body: starting one clears the others.
    fn clear_selections_except(&self, keep: &Entity<BodyText>, cx: &mut Context<Self>) {
        for (body, _) in self.bodies.values() {
            if body != keep {
                body.update(cx, |b, cx| b.clear_selection(cx));
            }
        }
    }

    /// Opens a clicked link in `message_id`: http and https in the browser, mailto in the
    /// composer. The schemes are checked again here, so nothing else can be opened whatever
    /// the detector produced. While the link guard is armed for the message, a web link only
    /// posts a toast whose "Open link" button opens it (SPEC 16.12.1).
    fn open_link(&self, target: &LinkTarget, message_id: &str, cx: &mut Context<Self>) {
        self.follow(target, message_id, false, cx);
    }

    /// The demo script's `link:N`: the n-th http(s) link of the open thread's target body goes
    /// through the same guard as a click. Unguarded, it prints `link: would open <host>`
    /// instead of opening a browser.
    pub fn follow_nth_link(&mut self, n: usize, cx: &mut Context<Self>) -> Result<(), String> {
        let key = self.last.open.clone().ok_or("no open thread")?;
        let (message_id, body) = {
            let store = self.store.read(cx);
            let Some(DetailState::Loaded(detail)) = store.details.get(&key) else {
                return Err("the thread is not loaded".into());
            };
            let target = store.ai_target(detail).ok_or("no target message")?;
            (target.id.clone(), BodyParts::of(target).main)
        };
        let link = crate::mail::links::find_links(&body)
            .into_iter()
            .map(|(_, t)| t)
            .filter(|t| matches!(t, LinkTarget::Web(_)))
            .nth(n)
            .ok_or_else(|| format!("no link {n}"))?;
        self.follow(&link, &message_id, true, cx);
        Ok(())
    }

    fn follow(&self, target: &LinkTarget, message_id: &str, dry: bool, cx: &mut Context<Self>) {
        let url = target.url();
        let guarded = self
            .last
            .open
            .as_ref()
            .is_some_and(|k| self.store.read(cx).link_guard(k, message_id));
        if let GuardDecision::Notice { text, url } = triage::guard_decision(target, guarded) {
            self.store.update(cx, |s, cx| {
                s.ask_open_link(text, url);
                cx.notify();
            });
            return;
        }
        match target {
            LinkTarget::Web(_) => {
                if dry {
                    let host = url::Url::parse(url)
                        .ok()
                        .and_then(|u| u.host_str().map(str::to_string))
                        .unwrap_or_default();
                    println!("link: would open {host}");
                } else {
                    spawn_open(url.to_string(), cx);
                }
            }
            LinkTarget::Mail(_) => {
                let Some(account) = self.last.open.as_ref().map(|k| k.account.clone()) else {
                    return;
                };
                let from = Address::new(None, &account);
                if let Some(prefill) = mailto::parse(url, &account, from) {
                    // Only ever a prefill: the user reviews and sends it.
                    let store = self.store.clone();
                    cx.defer(move |cx| super::windows::open_compose_window(&store, prefill, cx));
                } else {
                    eprintln!("eloquawk: ignored a link that is not a mailto URL");
                }
            }
        }
    }

    // ----- keyboard (SPEC 9) ---------------------------------------------------------------

    /// The open thread's message ids, when its detail has loaded.
    fn message_ids(&self, cx: &gpui::App) -> Vec<String> {
        let Some(key) = &self.last.open else {
            return Vec::new();
        };
        match self.store.read(cx).details.get(key) {
            Some(DetailState::Loaded(d)) => d.messages.iter().map(|m| m.id.clone()).collect(),
            _ => Vec::new(),
        }
    }

    /// `n` / `p`: focuses the next or previous card, expands it and scrolls it into view
    /// (card i is the scroll area's child i + 1). With no card focused yet, `n` starts at the
    /// first card and `p` at the last.
    pub fn step_card(&mut self, delta: isize, cx: &mut Context<Self>) {
        let ids = self.message_ids(cx);
        if ids.is_empty() {
            return;
        }
        let last = ids.len() as isize - 1;
        let ix = match self.focused_card {
            Some(i) => (i as isize + delta).clamp(0, last),
            None if delta > 0 => 0,
            None => last,
        } as usize;
        self.focused_card = Some(ix);
        self.expanded.insert(ids[ix].clone());
        self.scroll.scroll_to_item(ix + 1);
        cx.notify();
    }

    /// `o` / enter in the reader: toggles the focused card. False when no card is focused, so
    /// the key opens the cursor thread instead.
    pub fn toggle_focused_card(&mut self, cx: &mut Context<Self>) -> bool {
        let ids = self.message_ids(cx);
        let Some(id) = self.focused_card.and_then(|i| ids.get(i)) else {
            return false;
        };
        if !self.expanded.remove(id) {
            self.expanded.insert(id.clone());
        }
        cx.notify();
        true
    }

    /// `;` expands every message; `:` collapses them all.
    pub fn set_all_expanded(&mut self, expand: bool, cx: &mut Context<Self>) {
        let ids = self.message_ids(cx);
        if expand {
            self.expanded.extend(ids);
        } else {
            self.expanded.clear();
        }
        cx.notify();
    }

    /// Space / page down and shift-space / page up: scrolls by the visible height, less a
    /// line of overlap so the reading position carries over.
    pub fn scroll_page(&mut self, down: bool, cx: &mut Context<Self>) {
        if self.last.open.is_none() {
            return;
        }
        let page = (self.scroll.bounds().size.height - px(40.0)).max(px(40.0));
        let offset = self.scroll.offset();
        let max = self.scroll.max_offset().y;
        let y = if down {
            offset.y - page
        } else {
            offset.y + page
        };
        self.scroll
            .set_offset(point(offset.x, y.clamp(-max, px(0.0))));
        cx.notify();
    }

    /// The focused card, for `expect:card=`.
    pub fn focused_card(&self) -> Option<usize> {
        self.focused_card
    }

    /// How many of the open thread's messages are expanded, for `expect:expanded=`.
    pub fn expanded_count(&self, cx: &gpui::App) -> usize {
        self.message_ids(cx)
            .iter()
            .filter(|id| self.expanded.contains(*id))
            .count()
    }

    /// Whether the reader is scrolled away from the top, for `expect:scroll=`.
    pub fn scrolled(&self) -> bool {
        self.scroll.offset().y < px(0.0)
    }

    /// The demo script's `selectall:N`: expands card N, focuses its body and selects all.
    pub fn select_all_in_card(
        &mut self,
        n: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let key = self.last.open.clone().ok_or("no open thread")?;
        let messages = match self.store.read(cx).details.get(&key) {
            Some(DetailState::Loaded(detail)) => detail.messages.clone(),
            _ => return Err("the thread is not loaded".into()),
        };
        let m = messages.get(n).ok_or_else(|| format!("no card {n}"))?;
        self.initialise_messages(&messages);
        self.expanded.insert(m.id.clone());
        let body = self.body(m.id.clone(), BodyParts::of(m).main, false, cx);
        body.update(cx, |b, cx| b.select_all_and_focus(window, cx));
        cx.notify();
        Ok(())
    }

    /// Saves the open thread's attachment `n`, counting every message's attachments in
    /// order, as its Save button does (the demo's `save:N` step).
    pub fn save_attachment(&self, n: usize, cx: &gpui::App) -> Result<(), String> {
        let key = self.last.open.clone().ok_or("no open thread")?;
        let store = self.store.read(cx);
        let Some(DetailState::Loaded(detail)) = store.details.get(&key) else {
            return Err("the thread is not loaded".into());
        };
        let (message_id, attachment) = detail
            .messages
            .iter()
            .flat_map(|m| m.attachments.iter().map(move |a| (&m.id, a)))
            .nth(n)
            .ok_or_else(|| format!("no attachment {n}"))?;
        store.send_command(Command::SaveAttachment {
            account: key.account.clone(),
            message_id: message_id.clone(),
            attachment: attachment.clone(),
        });
        Ok(())
    }

    /// The selected text of whichever body has a selection (at most one has).
    pub fn selection_text(&self, cx: &gpui::App) -> String {
        self.bodies
            .values()
            .map(|(b, _)| b.read(cx).selected_text())
            .find(|t| !t.is_empty())
            .unwrap_or_default()
            .to_string()
    }

    fn initialise_messages(&mut self, messages: &[Message]) {
        expand_new(&mut self.seen, &mut self.expanded, messages);
    }

    /// By default the unread messages and the last one are expanded.
    fn initialise(&mut self, detail: &ThreadDetail) {
        self.initialise_messages(&detail.messages);
    }

    fn action_bar(&self, p: &Palette, summary: Option<&ThreadSummary>, cx: &Context<Self>) -> Div {
        let in_view = |id: &str| self.last.view == Some(ViewKind::Label(id.to_string()));
        let starred = summary.is_some_and(ThreadSummary::starred);
        let tool = |id: &'static str, label: &'static str, on: Option<OnClick>| {
            button(p, id, label, ButtonStyle::Toolbar, on)
        };
        let first = tool(
            "archive",
            if in_view(labels::SPAM) {
                "Not spam"
            } else {
                "Archive"
            },
            Some(self.click(cx, |this, _, cx| {
                let kind = this.store.read(cx).archive_kind();
                this.mutate(kind, cx)
            })),
        );
        let second = tool(
            "trash",
            if in_view(labels::TRASH) {
                "Restore"
            } else {
                "Trash"
            },
            Some(self.click(cx, |this, _, cx| {
                let kind = this.store.read(cx).trash_kind();
                this.mutate(kind, cx)
            })),
        );
        let gmail_url = summary.map(gmail_url);
        div()
            .flex_shrink_0()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(6.0))
            .px(px(20.0))
            .py(px(10.0))
            .border_b_1()
            .border_color(rgb(p.border))
            .child(first)
            .child(second)
            .child(tool(
                "mark-unread",
                "Mark unread",
                Some(self.click(cx, |this, _, cx| {
                    this.mutate(MutationKind::Labels(LabelDelta::unread()), cx)
                })),
            ))
            .child(tool(
                "star",
                if starred { "Unstar" } else { "Star" },
                Some(self.click(cx, |this, _, cx| {
                    let store = this.store.read(cx);
                    let starred = store
                        .open
                        .as_ref()
                        .and_then(|k| store.threads.get(k))
                        .is_some_and(ThreadSummary::starred);
                    let delta = if starred {
                        LabelDelta::unstar()
                    } else {
                        LabelDelta::star()
                    };
                    this.mutate(MutationKind::Labels(delta), cx)
                })),
            ))
            .child(tool(
                "labels",
                "Labels…",
                Some(self.click(cx, |this, _, cx| {
                    if let Some(open) = this.store.read(cx).open.clone() {
                        cx.emit(ReaderEvent::Labels(open));
                    }
                })),
            ))
            // The main view answers the open thread, which is the one shown here.
            .child(tool(
                "reply",
                "Reply",
                Some(self.click(cx, |_, window, cx| {
                    window.dispatch_action(Box::new(super::Reply), cx)
                })),
            ))
            .child(tool(
                "reply-all",
                "Reply all",
                Some(self.click(cx, |_, window, cx| {
                    window.dispatch_action(Box::new(super::ReplyAll), cx)
                })),
            ))
            .child(tool(
                "forward",
                "Forward",
                Some(self.click(cx, |_, window, cx| {
                    window.dispatch_action(Box::new(super::Forward), cx)
                })),
            ))
            .child(div().flex_1())
            // A change to this thread is waiting for the worker.
            .when(self.last.pending, |d| d.child(caption(p, "Saving…")))
            .child(ghost_button(
                p,
                "open-gmail",
                "Open in Gmail",
                gmail_url.map(|url| -> OnClick { open_url(url) }),
            ))
    }

    /// Child 0's AI rows (SPEC 16.12.1): the banner, the local caution when no Warning or
    /// Danger banner shows, and the failure or pause caption. Nothing while a check is in
    /// flight, and nothing for a clean result.
    fn triage_rows(&self, p: &Palette, cx: &Context<Self>) -> Vec<Div> {
        let Some(key) = self.last.open.clone() else {
            return Vec::new();
        };
        let store = self.store.read(cx);
        let Some(DetailState::Loaded(detail)) = store.details.get(&key) else {
            return Vec::new();
        };
        let target = store.ai_target(detail);
        let mut rows = Vec::new();
        let banner = store.ai_banner(&key).copied();
        if let (Some(v), Some(target)) = (banner, target) {
            let report = (!store.viewing_label(labels::SPAM)).then(|| {
                self.click(cx, |this, _, cx| {
                    let kind = this.store.read(cx).spam_kind();
                    this.mutate(kind, cx)
                })
            });
            let dismiss = self.click(cx, |this, _, cx| {
                this.store.update(cx, |s, cx| {
                    s.dismiss_triage();
                    cx.notify();
                })
            });
            rows.push(triage::banner(
                p,
                &v,
                &triage::reasons(target),
                report,
                dismiss,
            ));
        }
        let strong = banner.is_some_and(|v| v.level >= Level::Warning);
        if !strong && let Some(reasons) = store.local_caution(&key) {
            rows.push(triage::caution(p, &reasons));
        }
        let opted_in = store.ai_opted_in(&key.account);
        if banner.is_none() && opted_in && !store.ai_checking(&key) {
            if let Some(reason) = store.ai.paused.clone() {
                rows.push(triage::failure_caption(
                    p,
                    format!("Paused: {reason}"),
                    None,
                ));
            } else if let Some((_, retryable)) = store.ai_failure(&key) {
                let retry = retryable.then(|| {
                    self.click(cx, |this, _, cx| {
                        this.store.update(cx, |s, cx| {
                            s.retry_ai();
                            cx.notify();
                        })
                    })
                });
                rows.push(triage::failure_caption(
                    p,
                    "Couldn't check this message".into(),
                    retry,
                ));
            }
        }
        rows
    }

    fn subject_block(
        &self,
        p: &Palette,
        subject: &str,
        label_ids: &BTreeSet<String>,
        triage: Vec<Div>,
    ) -> Div {
        let chips = self
            .last
            .user_labels
            .iter()
            .filter(|(id, _, _)| label_ids.contains(id))
            .map(|(_, name, color)| label_chip(p, name.clone(), *color));
        div()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .px(px(20.0))
            .pt(px(18.0))
            .pb(px(10.0))
            .child(
                div()
                    .text_size(px(20.0))
                    .line_height(px(26.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(p.text))
                    .child(if subject.trim().is_empty() {
                        "(no subject)".to_string()
                    } else {
                        subject.to_string()
                    }),
            )
            .child(div().flex().flex_wrap().gap(px(6.0)).children(chips))
            .children(triage)
    }

    fn card(
        &mut self,
        p: &Palette,
        ix: usize,
        m: &Message,
        at: &CardEnv,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let expanded = self.expanded.contains(&m.id);
        let unread = m.label_ids.contains(labels::UNREAD);
        let clock = self.last.clock_24h;
        let id = m.id.clone();
        let toggle = cx.listener(move |this, _: &ClickEvent, _, cx| {
            if !this.expanded.remove(&id) {
                this.expanded.insert(id.clone());
            }
            cx.notify();
        });
        let sender = display_name(&m.from).to_string();
        let CardEnv { now, gmail_url } = at;
        let gmail_url = gmail_url.as_deref();
        let date = format::list_date(m.date, now, clock);
        let focused = self.focused_card == Some(ix);
        let shell = div()
            .mx(px(20.0))
            .mb(px(10.0))
            .rounded(px(8.0))
            .border_1()
            // The card `n` / `p` moved to.
            .border_color(rgb(if focused { p.accent } else { p.border }))
            .bg(rgb(p.panel))
            .flex()
            .flex_col();
        if !expanded {
            // A collapsed card keeps no body entities, so no hidden selection lingers.
            self.drop_body(&m.id, window, cx);
            self.drop_body(&quote_key(&m.id), window, cx);
            return shell
                .child(
                    div()
                        .id(("card-head", ix))
                        .flex()
                        .items_center()
                        .gap(px(10.0))
                        .px(px(14.0))
                        .h(px(40.0))
                        .cursor_pointer()
                        .on_click(toggle)
                        .child(
                            div()
                                .flex_shrink_0()
                                .max_w(px(200.0))
                                .truncate()
                                .text_size(px(13.0))
                                .font_weight(if unread {
                                    FontWeight::BOLD
                                } else {
                                    FontWeight::MEDIUM
                                })
                                .text_color(rgb(p.text))
                                .child(sender),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .truncate()
                                .text_size(px(13.0))
                                .text_color(rgb(p.muted))
                                .child(m.snippet.clone()),
                        )
                        .child(
                            div()
                                .flex_shrink_0()
                                .text_size(px(12.0))
                                .text_color(rgb(p.caption))
                                .child(date),
                        ),
                )
                .into_any_element();
        }
        let recipients = recipients_line(m);
        let parts = BodyParts::of(m);
        let unfolded = self.unfolded.contains(&m.id);
        let qid = m.id.clone();
        let copy_text = parts.visible(unfolded);
        let body = self.body(m.id.clone(), parts.main.clone(), false, cx);
        let quote = parts.quote.clone().map(|q| {
            let body = unfolded.then(|| self.body(quote_key(&m.id), q, true, cx));
            if !unfolded {
                // A folded quote keeps no entity (nor selection).
                self.drop_body(&quote_key(&m.id), window, cx);
            }
            body
        });
        let captions = parts.captions.clone();
        shell
            .child(
                div()
                    .id(("card-head", ix))
                    .flex()
                    .items_start()
                    .gap(px(10.0))
                    .px(px(14.0))
                    .pt(px(12.0))
                    .pb(px(8.0))
                    .cursor_pointer()
                    .on_click(toggle)
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(
                                div()
                                    .flex()
                                    .items_baseline()
                                    .gap(px(6.0))
                                    .min_w(px(0.0))
                                    .child(
                                        div()
                                            .flex_shrink_0()
                                            .text_size(px(13.0))
                                            .line_height(px(18.0))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(rgb(p.text))
                                            .child(sender),
                                    )
                                    .child(
                                        div()
                                            .min_w(px(0.0))
                                            .truncate()
                                            .text_size(px(12.0))
                                            .line_height(px(18.0))
                                            .text_color(rgb(p.muted))
                                            .child(format!("<{}>", m.from.email)),
                                    ),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(12.0))
                                    .line_height(px(16.0))
                                    .text_color(rgb(p.caption))
                                    .child(recipients),
                            ),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_size(px(12.0))
                            .line_height(px(18.0))
                            .text_color(rgb(p.caption))
                            .child(format::full_date(m.date, now, clock)),
                    )
                    .child(
                        ghost_button(
                            p,
                            ("copy", ix),
                            "Copy",
                            Some(Box::new(move |_, _, cx: &mut gpui::App| {
                                // The header toggles the card; copying must not.
                                cx.stop_propagation();
                                cx.write_to_clipboard(ClipboardItem::new_string(copy_text.clone()));
                            })),
                        )
                        .flex_shrink_0()
                        .h(px(22.0))
                        .px(px(8.0))
                        .text_size(px(12.0)),
                    ),
            )
            .child(
                div()
                    .px(px(14.0))
                    .pb(px(12.0))
                    .text_size(px(13.5))
                    .line_height(px(21.0))
                    .child(body),
            )
            .when_some(quote, |d, quote| {
                d.child(
                    div()
                        .px(px(14.0))
                        .pb(px(10.0))
                        .flex()
                        .flex_col()
                        .items_start()
                        .gap(px(6.0))
                        .child(
                            ghost_button(
                                p,
                                ("quote", ix),
                                "• • •",
                                Some(Box::new(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    if !this.unfolded.remove(&qid) {
                                        this.unfolded.insert(qid.clone());
                                    }
                                    cx.notify();
                                }))),
                            )
                            .px(px(6.0))
                            .py(px(0.0))
                            .rounded(px(4.0))
                            .bg(rgb(p.panel_raised))
                            .text_size(px(10.0)),
                        )
                        .when_some(quote, |d, body| {
                            d.child(
                                div()
                                    .w_full()
                                    .text_size(px(13.0))
                                    .line_height(px(20.0))
                                    .child(body),
                            )
                        }),
                )
            })
            .when(!captions.is_empty(), |d| {
                d.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(4.0))
                        .px(px(14.0))
                        .pb(px(10.0))
                        .child(caption(p, format!("{} ·", captions.join(" · "))))
                        .child(
                            div()
                                .id(("caption-gmail", ix))
                                .text_size(px(11.0))
                                .line_height(px(16.0))
                                .text_color(rgb(p.link))
                                .underline()
                                .when_some(gmail_url.map(str::to_string), |d, url| {
                                    d.cursor_pointer().on_click(open_url(url))
                                })
                                .child("Open in Gmail"),
                        ),
                )
            })
            .when(!m.attachments.is_empty(), |d| {
                d.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(6.0))
                        .px(px(14.0))
                        .pb(px(12.0))
                        .children(m.attachments.iter().enumerate().map(|(ai, a)| {
                            div()
                                .flex()
                                .items_center()
                                .gap(px(10.0))
                                .px(px(10.0))
                                .py(px(6.0))
                                .rounded(px(6.0))
                                .border_1()
                                .border_color(rgb(p.border))
                                .bg(rgb(p.panel_raised))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w(px(0.0))
                                        .truncate()
                                        .text_size(px(12.5))
                                        .text_color(rgb(p.text))
                                        .child(a.filename.clone()),
                                )
                                .child(
                                    div()
                                        .flex_shrink_0()
                                        .text_size(px(12.0))
                                        .text_color(rgb(p.caption))
                                        .child(format::size_human(a.size)),
                                )
                                .child(button(
                                    p,
                                    SharedString::from(format!("save-{ix}-{ai}")),
                                    "Save",
                                    ButtonStyle::Secondary,
                                    self.save_click(&m.id, a, cx),
                                ))
                        })),
                )
            })
            .into_any_element()
    }

    /// The Save button's handler: asks the worker to save `attachment` of message
    /// `message_id` in the open thread's account (SPEC 7.5). The worker answers with a toast.
    fn save_click(
        &self,
        message_id: &str,
        attachment: &Attachment,
        cx: &Context<Self>,
    ) -> Option<OnClick> {
        let account = self.last.open.as_ref()?.account.clone();
        let message_id = message_id.to_string();
        let attachment = attachment.clone();
        Some(self.click(cx, move |this, _, cx| {
            this.store.read(cx).send_command(Command::SaveAttachment {
                account: account.clone(),
                message_id: message_id.clone(),
                attachment: attachment.clone(),
            });
        }))
    }

    fn empty(&self, p: &Palette, title: &str, detail: &str) -> Div {
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(6.0))
            .child(
                div()
                    .text_size(px(15.0))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(rgb(p.secondary))
                    .child(title.to_string()),
            )
            .child(caption(p, detail.to_string()))
    }
}

/// The thread in Gmail on the web, for "Open in Gmail".
/// What every card of one render shares.
struct CardEnv {
    now: Zoned,
    /// The thread in Gmail's web UI.
    gmail_url: Option<String>,
}

fn gmail_url(s: &ThreadSummary) -> String {
    let email: String = url::form_urlencoded::byte_serialize(s.key.account.as_bytes()).collect();
    format!(
        "https://mail.google.com/mail/u/?authuser={email}#all/{}",
        s.key.id
    )
}

/// "to Ann, Bob · cc Cleo".
fn recipients_line(m: &Message) -> String {
    let names = |list: &[Address]| {
        list.iter()
            .map(|a| display_name(a).to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut parts = Vec::new();
    if !m.to.is_empty() {
        parts.push(format!("to {}", names(&m.to)));
    }
    if !m.cc.is_empty() {
        parts.push(format!("cc {}", names(&m.cc)));
    }
    if !m.bcc.is_empty() {
        parts.push(format!("bcc {}", names(&m.bcc)));
    }
    if parts.is_empty() {
        "to undisclosed recipients".into()
    } else {
        parts.join(" · ")
    }
}

/// Whether `url` is http or https, the only schemes Eloquawk hands to the browser.
fn is_web_url(url: &str) -> bool {
    let lower = url.get(..8).unwrap_or(url).to_ascii_lowercase();
    lower.starts_with("https://") || lower.starts_with("http://")
}

/// Opens an http or https URL in the browser, off the UI thread, and anything else not at
/// all: every caller (a click, the link guard's "Open link") goes through this check, so
/// nothing else can be opened whatever the detector produced. The URL is not logged.
pub(super) fn spawn_open(url: String, cx: &mut gpui::App) {
    if !is_web_url(&url) {
        return;
    }
    cx.background_spawn(async move {
        if let Err(e) = open::that(&url) {
            eprintln!("eloquawk: could not open the browser: {}", e.kind());
        }
    })
    .detach();
}

/// A click handler opening `url` in the browser.
fn open_url(url: String) -> OnClick {
    Box::new(move |_, _, cx| spawn_open(url.clone(), cx))
}

impl Focusable for ReaderView {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for ReaderView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let root = div()
            .id("reader")
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(p.bg));
        let Some(key) = self.last.open.clone() else {
            return root.child(self.empty(
                p,
                "No conversation selected",
                "Pick a conversation from the list to read it.",
            ));
        };
        let store = self.store.read(cx);
        let summary = store.threads.get(&key).cloned();
        let detail = match store.details.get(&key) {
            Some(DetailState::Loaded(d)) => Ok(Some(d.clone())),
            Some(DetailState::Failed(e)) => Err(e.clone()),
            Some(DetailState::Loading) | None => Ok(None),
        };
        let bar = self.action_bar(p, summary.as_ref(), cx);
        let now = Zoned::now();
        let mut scroll = div()
            .id("reader-scroll")
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .flex()
            .flex_col()
            .pb(px(24.0));
        match detail {
            Ok(Some(detail)) => {
                self.initialise(&detail);
                let label_ids = summary
                    .as_ref()
                    .map_or_else(|| detail.label_ids.clone(), |s| s.label_ids.clone());
                let triage = self.triage_rows(p, cx);
                scroll = scroll.child(self.subject_block(p, &detail.subject, &label_ids, triage));
                let at = CardEnv {
                    now,
                    gmail_url: summary.as_ref().map(gmail_url),
                };
                for (i, m) in detail.messages.iter().enumerate() {
                    scroll = scroll.child(self.card(p, i, m, &at, window, cx));
                }
            }
            Ok(None) => {
                let (subject, label_ids) = summary
                    .as_ref()
                    .map(|s| (s.subject.clone(), s.label_ids.clone()))
                    .unwrap_or_default();
                scroll = scroll
                    .child(self.subject_block(p, &subject, &label_ids, Vec::new()))
                    .child(div().px(px(20.0)).child(caption(p, "Loading…")));
            }
            Err(error) => {
                let retry_key = key.clone();
                scroll = scroll
                    .child(self.empty(p, "Couldn't load this conversation", &error))
                    .child(div().flex().justify_center().child(button(
                        p,
                        "retry-thread",
                        "Try again",
                        ButtonStyle::Secondary,
                        Some(self.click(cx, move |this, _, cx| {
                            let key = retry_key.clone();
                            this.store.update(cx, |s, cx| {
                                s.open_thread(key);
                                cx.notify();
                            });
                        })),
                    )));
            }
        }
        root.child(bar).child(scroll)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_web_urls_are_handed_to_the_browser() {
        for ok in ["https://a.example/x", "HTTP://a.example", "http://a"] {
            assert!(is_web_url(ok), "{ok}");
        }
        for bad in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "mailto:a@b",
            "",
            "https:/x",
        ] {
            assert!(!is_web_url(bad), "{bad}");
        }
    }

    #[test]
    fn cut_body_respects_char_boundaries() {
        assert_eq!(cut_body("short", 10), ("short", false));
        assert_eq!(cut_body("abcdef", 3), ("abc", true));
        // "é" is two bytes; a cut through it backs off.
        assert_eq!(cut_body("aé", 2), ("a", true));
        let big = "x".repeat(MAX_BODY + 5);
        let (cut, flagged) = cut_body(&big, MAX_BODY);
        assert!(flagged);
        assert_eq!(cut.len(), MAX_BODY);
    }

    fn message(id: &str, unread: bool) -> Message {
        Message {
            id: id.into(),
            thread_id: "t".into(),
            label_ids: if unread {
                [labels::UNREAD.to_string()].into()
            } else {
                BTreeSet::new()
            },
            from: Address::new(None, "a@example.com"),
            to: vec![],
            cc: vec![],
            bcc: vec![],
            reply_to: vec![],
            date: jiff::Timestamp::UNIX_EPOCH,
            subject: String::new(),
            message_id: None,
            in_reply_to: None,
            references: vec![],
            snippet: String::new(),
            body: String::new(),
            body_source: BodySource::Plain,
            body_truncated: false,
            signals: Default::default(),
            attachments: vec![],
        }
    }

    #[test]
    fn body_parts_split_trim_and_caption() {
        let mut m = message("m", false);
        m.body = "Thanks!\n\n> earlier\n".into();
        let parts = BodyParts::of(&m);
        assert_eq!(parts.main, "Thanks!");
        assert_eq!(parts.quote.as_deref(), Some("> earlier"));
        assert!(parts.captions.is_empty());
        assert_eq!(parts.visible(false), "Thanks!");
        assert_eq!(parts.visible(true), "Thanks!\n\n> earlier");

        m.body = "x".repeat(MAX_BODY + 10);
        m.body_source = BodySource::Html;
        let parts = BodyParts::of(&m);
        assert_eq!(parts.main.len(), MAX_BODY);
        assert_eq!(parts.captions, ["Converted from HTML", "Message truncated"]);

        m.body = String::new();
        m.body_source = BodySource::Empty;
        m.snippet = "Only a snippet".into();
        assert_eq!(BodyParts::of(&m).main, "Only a snippet");
    }

    #[test]
    fn recipients_are_summarised() {
        let mut m = Message {
            id: "m".into(),
            thread_id: "t".into(),
            label_ids: BTreeSet::new(),
            from: Address::new(Some("Ann"), "ann@example.com"),
            to: vec![
                Address::new(Some("Bob"), "bob@example.com"),
                Address::new(None, "cleo@example.com"),
            ],
            cc: vec![Address::new(Some("Dee"), "dee@example.com")],
            bcc: vec![],
            reply_to: vec![],
            date: jiff::Timestamp::UNIX_EPOCH,
            subject: String::new(),
            message_id: None,
            in_reply_to: None,
            references: vec![],
            snippet: String::new(),
            body: String::new(),
            body_source: BodySource::Plain,
            body_truncated: false,
            signals: Default::default(),
            attachments: vec![],
        };
        assert_eq!(recipients_line(&m), "to Bob, cleo@example.com · cc Dee");
        m.to.clear();
        m.cc.clear();
        assert_eq!(recipients_line(&m), "to undisclosed recipients");
    }

    #[test]
    fn new_messages_get_their_default_expansion_and_toggles_are_kept() {
        let msg = message;
        let (mut seen, mut expanded) = (HashSet::new(), HashSet::new());
        let mut messages = vec![msg("a", false), msg("b", true), msg("c", false)];
        expand_new(&mut seen, &mut expanded, &messages);
        assert_eq!(expanded, HashSet::from(["b".to_string(), "c".to_string()]));

        // The user collapses the unread one; a re-render changes nothing.
        expanded.remove("b");
        expand_new(&mut seen, &mut expanded, &messages);
        assert_eq!(expanded, HashSet::from(["c".to_string()]));

        // A reply arrives: it is last, so it opens; an earlier new unread opens too.
        messages.push(msg("d", true));
        messages.push(msg("e", false));
        expand_new(&mut seen, &mut expanded, &messages);
        assert_eq!(
            expanded,
            HashSet::from(["c".to_string(), "d".to_string(), "e".to_string()])
        );
    }
}
