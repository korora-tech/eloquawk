//! The reading pane (SPEC 8.2): a [`ReaderView`] entity that the main view embeds with
//! `.cached(..)`, so the frequent store notifications that do not concern the open thread do
//! not re-render it. It observes the store and notifies only when the open thread, its detail
//! or its summary changes.
//!
//! The scroll area's children are the subject block (child 0) and then exactly one child per
//! message card, so card i is child i + 1. Bodies are plain wrapped text until M5 makes them
//! selectable.

use std::collections::{BTreeSet, HashSet};

use gpui::prelude::*;
use gpui::{
    AnyElement, ClickEvent, Context, Div, Entity, FocusHandle, Focusable, FontWeight, ScrollHandle,
    SharedString, Subscription, Window, div, point, px, rgb,
};
use jiff::Zoned;

use super::format;
use super::theme::{Palette, palette};
use super::widgets::{ButtonStyle, OnClick, button, caption, ghost_button, label_chip};
use crate::mail::address::display_name;
use crate::model::{
    Address, BodySource, LabelDelta, LabelKind, Message, ThreadDetail, ThreadKey, ThreadSummary,
    ViewKind, labels,
};
use crate::store::{DetailState, MutationKind, Store};

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

/// Splits a body into what is shown and a trailing quote that folds behind "• • •": a final
/// block of `>` lines (blank lines allowed inside), with an "On … wrote:" line just above it
/// included. Nothing folds when the quote would be all there is.
pub fn split_quote(body: &str) -> (&str, Option<&str>) {
    // Byte offsets of each line start, with the line.
    let mut lines: Vec<(usize, &str)> = Vec::new();
    let mut at = 0;
    for line in body.split_inclusive('\n') {
        lines.push((at, line));
        at += line.len();
    }
    let is_blank = |l: &str| l.trim().is_empty();
    let is_quote = |l: &str| l.trim_start().starts_with('>');
    let mut i = lines.len();
    while i > 0 && is_blank(lines[i - 1].1) {
        i -= 1;
    }
    let mut start = None;
    while i > 0 && (is_quote(lines[i - 1].1) || is_blank(lines[i - 1].1)) {
        i -= 1;
        if is_quote(lines[i].1) {
            start = Some(i);
        }
    }
    let Some(mut start) = start else {
        return (body, None);
    };
    // An attribution line ("On Mon, Zoë wrote:"), possibly after blank lines.
    let mut j = start;
    while j > 0 && is_blank(lines[j - 1].1) {
        j -= 1;
    }
    if j > 0 {
        let l = lines[j - 1].1.trim();
        if l.starts_with("On ") && l.ends_with("wrote:") {
            start = j - 1;
        }
    }
    let cut = lines[start].0;
    let shown = body[..cut].trim_end();
    if shown.is_empty() {
        return (body, None);
    }
    (shown, Some(body[cut..].trim_end()))
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
        Self {
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

pub struct ReaderView {
    store: Entity<Store>,
    focus: FocusHandle,
    scroll: ScrollHandle,
    last: Fingerprint,
    /// Expanded message ids of the open thread.
    expanded: HashSet<String>,
    /// Message ids whose folded quote is shown.
    unfolded: HashSet<String>,
    /// The thread `expanded` was initialised for (once its detail loaded).
    initialised: Option<ThreadKey>,
    _subscription: Subscription,
}

impl ReaderView {
    pub fn new(store: Entity<Store>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.observe(&store, |this, store, cx| {
            let next = Fingerprint::of(store.read(cx));
            if next != this.last {
                if next.open != this.last.open {
                    this.scroll.set_offset(point(px(0.), px(0.)));
                    this.expanded.clear();
                    this.unfolded.clear();
                    this.initialised = None;
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
            initialised: None,
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

    fn mutate(&self, kind: MutationKind, cx: &mut Context<Self>) {
        self.store.update(cx, |s, cx| {
            s.mutate(kind);
            cx.notify();
        });
    }

    /// By default the unread messages and the last one are expanded.
    fn initialise(&mut self, detail: &ThreadDetail) {
        if self.initialised.as_ref() == Some(&detail.key) {
            return;
        }
        self.initialised = Some(detail.key.clone());
        self.expanded = detail
            .messages
            .iter()
            .filter(|m| m.label_ids.contains(labels::UNREAD))
            .map(|m| m.id.clone())
            .chain(detail.messages.last().map(|m| m.id.clone()))
            .collect();
    }

    fn action_bar(&self, p: &Palette, summary: Option<&ThreadSummary>, cx: &Context<Self>) -> Div {
        let in_view = |id: &str| self.last.view == Some(ViewKind::Label(id.to_string()));
        let starred = summary.is_some_and(ThreadSummary::starred);
        let tool = |id: &'static str, label: &'static str, on: Option<OnClick>| {
            button(p, id, label, ButtonStyle::Toolbar, on)
        };
        let first = if in_view(labels::SPAM) {
            tool(
                "not-spam",
                "Not spam",
                Some(self.click(cx, |this, _, cx| {
                    this.mutate(MutationKind::Labels(LabelDelta::not_spam()), cx)
                })),
            )
        } else {
            tool(
                "archive",
                "Archive",
                Some(self.click(cx, |this, _, cx| {
                    this.mutate(MutationKind::Labels(LabelDelta::archive()), cx)
                })),
            )
        };
        let second = if in_view(labels::TRASH) {
            tool(
                "restore",
                "Restore",
                Some(self.click(cx, |this, _, cx| {
                    let restore = LabelDelta {
                        add: vec![labels::INBOX.into()],
                        remove: vec![],
                    };
                    this.mutate(MutationKind::Untrash { restore }, cx)
                })),
            )
        } else {
            tool(
                "trash",
                "Trash",
                Some(self.click(cx, |this, _, cx| this.mutate(MutationKind::Trash, cx))),
            )
        };
        let gmail_url = summary.map(|s| {
            let email: String =
                url::form_urlencoded::byte_serialize(s.key.account.as_bytes()).collect();
            format!(
                "https://mail.google.com/mail/u/?authuser={email}#all/{}",
                s.key.id
            )
        });
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
                Some(self.click(cx, move |this, _, cx| {
                    let delta = if starred {
                        LabelDelta::unstar()
                    } else {
                        LabelDelta::star()
                    };
                    this.mutate(MutationKind::Labels(delta), cx)
                })),
            ))
            // Labels arrives in M7; Reply, Reply all and Forward in M8.
            .child(tool("labels", "Labels…", None))
            .child(tool("reply", "Reply", None))
            .child(tool("reply-all", "Reply all", None))
            .child(tool("forward", "Forward", None))
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

    fn subject_block(&self, p: &Palette, subject: &str, label_ids: &BTreeSet<String>) -> Div {
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
    }

    fn card(
        &self,
        p: &Palette,
        ix: usize,
        m: &Message,
        now: &Zoned,
        cx: &Context<Self>,
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
        let date = format::list_date(m.date, now, clock);
        let shell = div()
            .mx(px(20.0))
            .mb(px(10.0))
            .rounded(px(8.0))
            .border_1()
            .border_color(rgb(p.border))
            .bg(rgb(p.panel))
            .flex()
            .flex_col();
        if !expanded {
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
        let (body, cut) = cut_body(&m.body, MAX_BODY);
        let (shown, quote) = split_quote(body);
        let unfolded = self.unfolded.contains(&m.id);
        let qid = m.id.clone();
        let mut captions: Vec<&str> = Vec::new();
        if m.body_source == BodySource::Html {
            captions.push("Converted from HTML");
        }
        if cut || m.body_truncated {
            captions.push("Message truncated");
        }
        let body_text = if m.body_source == BodySource::Empty && shown.trim().is_empty() {
            m.snippet.clone()
        } else {
            shown.to_string()
        };
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
                    ),
            )
            .child(
                div()
                    .px(px(14.0))
                    .pb(px(12.0))
                    .text_size(px(13.5))
                    .line_height(px(21.0))
                    .text_color(rgb(p.text))
                    .child(body_text),
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
                        .when(unfolded, |d| {
                            d.child(
                                div()
                                    .text_size(px(13.0))
                                    .line_height(px(20.0))
                                    .text_color(rgb(p.secondary))
                                    .child(quote.to_string()),
                            )
                        }),
                )
            })
            .when(!captions.is_empty(), |d| {
                d.child(div().px(px(14.0)).pb(px(10.0)).child(caption(
                    p,
                    format!("{} · Open in Gmail", captions.join(" · ")),
                )))
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
                                // Saving arrives in M12.
                                .child(button(
                                    p,
                                    SharedString::from(format!("save-{ix}-{ai}")),
                                    "Save",
                                    ButtonStyle::Secondary,
                                    None,
                                ))
                        })),
                )
            })
            .into_any_element()
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

/// Opens an https URL in the browser, off the UI thread. The URL is not logged.
fn open_url(url: String) -> OnClick {
    Box::new(move |_, _, cx| {
        let url = url.clone();
        cx.background_spawn(async move {
            if let Err(e) = open::that(&url) {
                eprintln!("eloquawk: could not open the browser: {}", e.kind());
            }
        })
        .detach();
    })
}

impl Focusable for ReaderView {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for ReaderView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
                scroll = scroll.child(self.subject_block(p, &detail.subject, &label_ids));
                for (i, m) in detail.messages.iter().enumerate() {
                    scroll = scroll.child(self.card(p, i, m, &now, cx));
                }
            }
            Ok(None) => {
                let (subject, label_ids) = summary
                    .as_ref()
                    .map(|s| (s.subject.clone(), s.label_ids.clone()))
                    .unwrap_or_default();
                scroll = scroll
                    .child(self.subject_block(p, &subject, &label_ids))
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

    #[test]
    fn trailing_quote_folds() {
        let body = "Sounds good.\n\n> earlier\n> more\n";
        assert_eq!(
            split_quote(body),
            ("Sounds good.", Some("> earlier\n> more"))
        );
    }

    #[test]
    fn attribution_line_folds_with_the_quote() {
        let body = "Yes!\n\nOn Mon, 3 Oct 2026, Zoë wrote:\n> Lunch?\n>\n> > nested\n\n";
        assert_eq!(
            split_quote(body),
            (
                "Yes!",
                Some("On Mon, 3 Oct 2026, Zoë wrote:\n> Lunch?\n>\n> > nested")
            )
        );
    }

    #[test]
    fn inline_quotes_and_quote_only_bodies_do_not_fold() {
        let inline = "> question\nanswer\n";
        assert_eq!(split_quote(inline), (inline, None));
        let only = "> all quote\n> here";
        assert_eq!(split_quote(only), (only, None));
        assert_eq!(split_quote("plain"), ("plain", None));
        assert_eq!(split_quote(""), ("", None));
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
            attachments: vec![],
        };
        assert_eq!(recipients_line(&m), "to Bob, cleo@example.com · cc Dee");
        m.to.clear();
        m.cc.clear();
        assert_eq!(recipients_line(&m), "to undisclosed recipients");
    }
}
