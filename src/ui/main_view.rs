//! `MainView`, the main window's root (SPEC 8.2): the header with search, the problem banner,
//! the sidebar, thread list and reader panes (or an auth card), the toast and the shortcut
//! overlay. It owns the Mail key context and handles every Mail action of SPEC 9 (keys whose
//! feature lands later post "Not available yet"), and answers the demo script's `expect:`
//! checks.

use std::time::{Duration, Instant};

use gpui::prelude::*;
use gpui::{
    AnyElement, ClickEvent, Context, Div, Entity, FocusHandle, Focusable, FontWeight,
    ScrollStrategy, StyleRefinement, Subscription, Task, UniformListScrollHandle, Window, div, px,
    rgb,
};

use super::auth::Screen;
use super::reader::ReaderView;
use super::text_input::TextInput;
use super::theme::{self, CONTROL_H, HEADER_H, ICON_SLOT, palette, tint};
use super::widgets::{ButtonStyle, button, ghost_button};
use super::{
    Archive, BackToList, CloseWindow, CollapseAll, Compose, ExpandAll, FocusSearch, Forward,
    GoAllMail, GoDrafts, GoInbox, GoSent, GoStarred, MarkRead, MarkUnread, NextAccount,
    NextMessage, NextThread, OpenLabelPicker, OpenSettings, OpenThread, PrevAccount, PrevMessage,
    PrevThread, Quit, Refresh, Reply, ReplyAll, ReportSpam, ScrollReaderDown, ScrollReaderUp,
    ToggleHelp, ToggleSelect, ToggleStar, Trash, Undo,
};
use crate::demo_script::{Cond, FocusTarget};
use crate::model::{LabelDelta, ThreadKey, ViewKind, labels};
use crate::protocol::Command;
use crate::store::{MutationKind, NoticeAction, Row, Store};

/// The notice for keys whose feature lands in a later milestone.
pub const NOT_YET: &str = "Not available yet";

/// How long a toast stays up.
const TOAST_FOR: Duration = Duration::from_secs(5);

pub struct MainView {
    pub(super) store: Entity<Store>,
    reader: Entity<ReaderView>,
    search: Entity<TextInput>,
    /// The Mail key context's focus: the panes container.
    focus: FocusHandle,
    pub(super) list_scroll: UniformListScrollHandle,
    /// The cursor the list was last scrolled to.
    scrolled_to: Option<ThreadKey>,
    /// The screen last rendered; entering Mail focuses the list.
    screen: Screen,
    /// The keyboard shortcut overlay is showing.
    pub(super) help_open: bool,
    /// The notice the toast timer is running for, and the timer (one, never per frame).
    toast_for: Option<Instant>,
    _toast_timer: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl MainView {
    pub fn new(store: Entity<Store>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let reader = cx.new(|cx| ReaderView::new(store.clone(), window, cx));
        // Search is wired in M7; the field is already here so the layout is final.
        let search = cx.new(|cx| TextInput::new("Search mail", false, 0, cx));
        let subscriptions = vec![
            cx.observe(&store, |this, store, cx| {
                this.schedule_toast_expiry(&store, cx);
                cx.notify();
            }),
            cx.observe_window_appearance(window, |this, window, cx| {
                let saved = this.store.read(cx).prefs.theme;
                theme::apply(saved, window, cx);
            }),
        ];
        theme::apply(store.read(cx).prefs.theme, window, cx);
        Self {
            store,
            reader,
            search,
            focus: cx.focus_handle(),
            list_scroll: UniformListScrollHandle::new(),
            scrolled_to: None,
            screen: Screen::Auth,
            help_open: false,
            toast_for: None,
            _toast_timer: None,
            _subscriptions: subscriptions,
        }
    }

    /// Focuses the Mail context (the list), as after opening the window or switching views.
    pub fn focus_mail(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
    }

    /// Starts the single expiry timer when a new notice is posted.
    fn schedule_toast_expiry(&mut self, store: &Entity<Store>, cx: &mut Context<Self>) {
        let posted = store.read(cx).notice.as_ref().map(|n| n.posted_at);
        if posted == self.toast_for {
            return;
        }
        self.toast_for = posted;
        self._toast_timer = posted.map(|at| {
            let store = store.clone();
            cx.spawn(async move |_, cx| {
                cx.background_executor()
                    .timer(TOAST_FOR.saturating_sub(at.elapsed()))
                    .await;
                store.update(cx, |s, cx| {
                    if s.notice.as_ref().is_some_and(|n| n.posted_at == at) {
                        s.clear_notice();
                        cx.notify();
                    }
                });
            })
        });
    }

    // ----- navigation actions -------------------------------------------------------------

    fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        self.store.update(cx, |s, cx| {
            s.move_cursor(delta);
            if s.open.is_some() && s.open != s.cursor {
                s.open_cursor();
            }
            cx.notify();
        });
    }

    fn next_thread(&mut self, _: &NextThread, _: &mut Window, cx: &mut Context<Self>) {
        self.step(1, cx);
    }

    fn prev_thread(&mut self, _: &PrevThread, _: &mut Window, cx: &mut Context<Self>) {
        self.step(-1, cx);
    }

    /// Opens the cursor thread; when the reader already shows it and a card is focused,
    /// toggles that card instead.
    fn open_thread(&mut self, _: &OpenThread, _: &mut Window, cx: &mut Context<Self>) {
        let reading_cursor = {
            let s = self.store.read(cx);
            s.open.is_some() && s.open == s.cursor
        };
        if reading_cursor
            && self
                .reader
                .update(cx, |reader, cx| reader.toggle_focused_card(cx))
        {
            return;
        }
        self.store.update(cx, |s, cx| {
            s.open_cursor();
            cx.notify();
        });
    }

    /// Escape and `u`: close the shortcut overlay if it shows, else the reader, else clear the
    /// selection.
    fn back_to_list(&mut self, _: &BackToList, window: &mut Window, cx: &mut Context<Self>) {
        if self.help_open {
            self.set_help(false, window, cx);
            return;
        }
        self.store.update(cx, |s, cx| {
            if s.open.is_some() {
                s.close_thread();
            } else {
                s.clear_selection();
            }
            cx.notify();
        });
        self.focus_mail(window, cx);
    }

    /// Shows or hides the shortcut overlay; the list keeps the focus either way.
    pub(super) fn set_help(&mut self, open: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.help_open = open;
        self.focus_mail(window, cx);
        cx.notify();
    }

    fn toggle_help(&mut self, _: &ToggleHelp, window: &mut Window, cx: &mut Context<Self>) {
        self.set_help(!self.help_open, window, cx);
    }

    fn next_message(&mut self, _: &NextMessage, _: &mut Window, cx: &mut Context<Self>) {
        self.reader.update(cx, |r, cx| r.step_card(1, cx));
    }

    fn prev_message(&mut self, _: &PrevMessage, _: &mut Window, cx: &mut Context<Self>) {
        self.reader.update(cx, |r, cx| r.step_card(-1, cx));
    }

    fn scroll_down(&mut self, _: &ScrollReaderDown, _: &mut Window, cx: &mut Context<Self>) {
        self.reader.update(cx, |r, cx| r.scroll_page(true, cx));
    }

    fn scroll_up(&mut self, _: &ScrollReaderUp, _: &mut Window, cx: &mut Context<Self>) {
        self.reader.update(cx, |r, cx| r.scroll_page(false, cx));
    }

    fn expand_all(&mut self, _: &ExpandAll, _: &mut Window, cx: &mut Context<Self>) {
        self.reader.update(cx, |r, cx| r.set_all_expanded(true, cx));
    }

    fn collapse_all(&mut self, _: &CollapseAll, _: &mut Window, cx: &mut Context<Self>) {
        self.reader
            .update(cx, |r, cx| r.set_all_expanded(false, cx));
    }

    fn toggle_select(&mut self, _: &ToggleSelect, _: &mut Window, cx: &mut Context<Self>) {
        self.store.update(cx, |s, cx| {
            s.toggle_select_cursor();
            cx.notify();
        });
    }

    // ----- mutations ----------------------------------------------------------------------

    /// Applies the mutation `kind` picks for the current view to the targets (the selection,
    /// else the open thread, else the cursor row).
    fn mutate(&mut self, kind: impl FnOnce(&Store) -> MutationKind, cx: &mut Context<Self>) {
        self.store.update(cx, |s, cx| {
            let kind = kind(s);
            s.mutate(kind);
            cx.notify();
        });
    }

    fn archive(&mut self, _: &Archive, _: &mut Window, cx: &mut Context<Self>) {
        self.mutate(Store::archive_kind, cx);
    }

    fn trash(&mut self, _: &Trash, _: &mut Window, cx: &mut Context<Self>) {
        self.mutate(Store::trash_kind, cx);
    }

    fn report_spam(&mut self, _: &ReportSpam, _: &mut Window, cx: &mut Context<Self>) {
        self.mutate(Store::spam_kind, cx);
    }

    fn toggle_star(&mut self, _: &ToggleStar, _: &mut Window, cx: &mut Context<Self>) {
        self.mutate(Store::star_kind, cx);
    }

    fn mark_read(&mut self, _: &MarkRead, _: &mut Window, cx: &mut Context<Self>) {
        self.mutate(|_| MutationKind::Labels(LabelDelta::read()), cx);
    }

    fn mark_unread(&mut self, _: &MarkUnread, _: &mut Window, cx: &mut Context<Self>) {
        self.mutate(|_| MutationKind::Labels(LabelDelta::unread()), cx);
    }

    fn undo(&mut self, _: &Undo, _: &mut Window, cx: &mut Context<Self>) {
        self.store.update(cx, |s, cx| {
            if s.undo() {
                cx.notify();
            }
        });
    }

    // ----- views and accounts -------------------------------------------------------------

    fn go(&mut self, kind: ViewKind, window: &mut Window, cx: &mut Context<Self>) {
        self.store.update(cx, |s, cx| {
            s.go_to(kind);
            cx.notify();
        });
        self.focus_mail(window, cx);
    }

    fn go_label(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.go(ViewKind::Label(id.to_string()), window, cx);
    }

    fn switch_account(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        self.store.update(cx, |s, cx| {
            s.switch_account(delta);
            cx.notify();
        });
        self.focus_mail(window, cx);
    }

    /// The demo script's `view:<label id>`: shows that label of the current account
    /// (`ALL_MAIL` for All Mail).
    pub fn show_view(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let kind = if id == "ALL_MAIL" {
            ViewKind::AllMail
        } else {
            ViewKind::Label(id.to_string())
        };
        self.go(kind, window, cx);
    }

    /// Keys bound from M6 whose feature lands later (`l`, `/`, `c`, `r`, `a`, `f`, ctrl-n,
    /// ctrl-,): the owning milestone replaces the handler.
    fn not_yet(&mut self, cx: &mut Context<Self>) {
        self.store.update(cx, |s, cx| {
            s.inform(NOT_YET);
            cx.notify();
        });
    }

    /// The demo script's `open:N`: cursor on row N, then open it.
    pub fn open_row(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.store.update(cx, |s, cx| {
            s.set_cursor_index(ix);
            s.open_cursor();
            cx.notify();
        });
    }

    /// The demo script's `selectall:N`: selects all of card N's body in the open thread.
    pub fn select_all_in_card(
        &mut self,
        n: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.reader
            .update(cx, |reader, cx| reader.select_all_in_card(n, window, cx))
    }

    // ----- demo script checks -------------------------------------------------------------

    fn focus_target(&self, window: &Window, cx: &gpui::App) -> Option<FocusTarget> {
        if self.search.focus_handle(cx).is_focused(window) {
            Some(FocusTarget::Search)
        } else if self.reader.focus_handle(cx).contains_focused(window, cx) {
            Some(FocusTarget::Reader)
        } else if self.focus.is_focused(window) {
            Some(FocusTarget::List)
        } else {
            None
        }
    }

    /// Checks an `expect:` condition; `Err` carries the actual value.
    pub fn check(&self, cond: &Cond, window: &Window, cx: &gpui::App) -> Result<(), String> {
        let store = self.store.read(cx);
        let row_of = |key: &Option<ThreadKey>| -> Option<usize> {
            let key = key.as_ref()?;
            store.visible_rows().iter().position(|r| r.key() == key)
        };
        let show = |ix: Option<usize>| ix.map_or("none".to_string(), |i| i.to_string());
        let (ok, actual) = match cond {
            Cond::Cursor(n) => {
                let ix = store.cursor_index();
                (ix == Some(*n), show(ix))
            }
            Cond::Open(want) => {
                let ix = row_of(&store.open);
                let ok = match want {
                    None => store.open.is_none(),
                    Some(n) => ix == Some(*n),
                };
                let actual = match (&store.open, ix) {
                    (None, _) => "none".to_string(),
                    (Some(_), Some(i)) => i.to_string(),
                    (Some(_), None) => "a thread outside the list".to_string(),
                };
                (ok, actual)
            }
            Cond::View(want) => {
                let actual = match store.view.as_ref().map(|v| &v.kind) {
                    Some(ViewKind::Label(id)) => id.clone(),
                    Some(ViewKind::AllMail) => "ALL_MAIL".into(),
                    Some(ViewKind::Search(_)) => "search".into(),
                    None => "none".into(),
                };
                (actual == *want, actual)
            }
            Cond::Focus(want) => {
                let actual = self.focus_target(window, cx);
                (
                    actual == Some(*want),
                    actual.map_or("none", FocusTarget::name).to_string(),
                )
            }
            Cond::Search(want) => {
                let actual = self.search.read(cx).text().to_string();
                (actual == *want, actual)
            }
            Cond::Selection(want) => {
                let actual = self.reader.read(cx).selection_text(cx);
                (actual == *want, actual)
            }
            Cond::Card(want) => {
                let ix = self.reader.read(cx).focused_card();
                (ix == *want, show(ix))
            }
            Cond::Expanded(want) => {
                let n = self.reader.read(cx).expanded_count(cx);
                (n == *want, n.to_string())
            }
            Cond::Scrolled(want) => {
                let scrolled = self.reader.read(cx).scrolled();
                (
                    scrolled == *want,
                    if scrolled { "down" } else { "top" }.into(),
                )
            }
            Cond::Selected(want) => {
                let n = store.selected.len();
                (n == *want, n.to_string())
            }
            Cond::Rows(want) => {
                let n = store.visible_rows().len();
                (n == *want, n.to_string())
            }
            Cond::Unread(want) | Cond::Starred(want) => {
                let row = store
                    .cursor_index()
                    .and_then(|i| store.visible_rows().get(i).cloned());
                let flag = match row {
                    Some(Row::Thread(t)) => Some(if matches!(cond, Cond::Unread(_)) {
                        t.unread()
                    } else {
                        t.starred()
                    }),
                    _ => None,
                };
                let actual = match flag {
                    Some(true) => "yes",
                    Some(false) => "no",
                    None => "no cursor thread",
                };
                (flag == Some(*want), actual.into())
            }
            Cond::Notice(want) => {
                let actual = store.notice.as_ref().map(|n| n.text.clone());
                (actual == *want, actual.unwrap_or_else(|| "none".into()))
            }
            Cond::Help(want) => (
                self.help_open == *want,
                if self.help_open { "open" } else { "closed" }.into(),
            ),
            Cond::Account(want) => {
                let actual = store.view.as_ref().map(|v| v.account.clone());
                (
                    actual.as_deref() == Some(want.as_str()),
                    actual.unwrap_or_else(|| "none".into()),
                )
            }
            Cond::Syncs(want) => (store.syncs == *want, store.syncs.to_string()),
        };
        if ok { Ok(()) } else { Err(actual) }
    }

    // ----- chrome -------------------------------------------------------------------------

    fn header(&self, store: &Store, cx: &Context<Self>) -> Div {
        let p = palette(cx);
        div()
            .h(px(HEADER_H))
            .w_full()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px(12.0))
            .px(px(14.0))
            .bg(rgb(p.header))
            .border_b_1()
            .border_color(rgb(p.border))
            // The app icon fills this slot in M14.
            .child(div().size(px(ICON_SLOT)).flex_shrink_0())
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .max_w(px(680.0))
                    .child(self.search.clone()),
            )
            .child(div().flex_1())
            .child(
                div()
                    .size(px(8.0))
                    .flex_shrink_0()
                    .rounded_full()
                    .when(store.busy, |d| d.bg(rgb(p.amber))),
            )
            .child(
                ghost_button(
                    p,
                    "refresh",
                    "Refresh",
                    (!store.busy).then(|| -> super::widgets::OnClick {
                        Box::new(cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.store.read(cx).send_command(Command::Refresh)
                        }))
                    }),
                )
                .h(px(CONTROL_H)),
            )
            // Settings arrives in M9.
            .child(ghost_button(p, "settings", "Settings", None).h(px(CONTROL_H)))
    }

    fn banner(&self, store: &Store, cx: &Context<Self>) -> Option<Div> {
        let p = palette(cx);
        let problem = store.problem.clone()?;
        Some(
            div()
                .flex_shrink_0()
                .flex()
                .items_center()
                .gap_2()
                .px_5()
                .py_2()
                .bg(rgb(tint(p.amber, p.bg, 0.12)))
                .border_b_1()
                .border_color(rgb(tint(p.amber, p.bg, 0.35)))
                .child(
                    div()
                        .size(px(6.0))
                        .flex_shrink_0()
                        .rounded_full()
                        .bg(rgb(p.amber)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .text_size(px(12.0))
                        .text_color(rgb(p.text))
                        .child(problem),
                )
                .child(ghost_button(
                    p,
                    "dismiss-problem",
                    "✕",
                    Some(Box::new(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.store.read(cx).send_command(Command::DismissProblems)
                    }))),
                )),
        )
    }

    fn toast(&self, store: &Store, cx: &Context<Self>) -> Option<Div> {
        let p = palette(cx);
        let notice = store.notice.as_ref()?;
        if notice.posted_at.elapsed() > TOAST_FOR {
            return None;
        }
        let undo = (notice.action == Some(NoticeAction::Undo)).then(|| {
            button(
                p,
                "toast-undo",
                "Undo",
                ButtonStyle::Secondary,
                Some(Box::new(cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.store.update(cx, |s, cx| {
                        s.undo();
                        cx.notify();
                    });
                }))),
            )
        });
        Some(
            div().absolute().bottom(px(18.0)).right(px(18.0)).child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.0))
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(rgb(p.selected_border))
                    .bg(rgb(p.panel_raised))
                    .shadow_md()
                    .pl_4()
                    .pr(px(if undo.is_some() { 6.0 } else { 16.0 }))
                    .py(px(6.0))
                    .min_h(px(40.0))
                    .child(
                        div()
                            .text_size(px(12.5))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(rgb(p.text))
                            .child(notice.text.clone()),
                    )
                    .children(undo),
            ),
        )
    }

    fn panes(&self, store: &Store, cx: &Context<Self>) -> AnyElement {
        div()
            .size_full()
            .flex()
            .child(self.sidebar(store, cx))
            .child(self.thread_list(store, cx))
            .child(
                div().flex_1().min_w(px(0.0)).h_full().child(
                    self.reader
                        .clone()
                        .cached(StyleRefinement::default().size_full()),
                ),
            )
            .into_any_element()
    }
}

impl Focusable for MainView {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for MainView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let screen = Screen::of(self.store.read(cx));
        if screen != self.screen {
            self.screen = screen;
            if screen == Screen::Mail {
                cx.defer_in(window, |this, window, cx| this.focus_mail(window, cx));
            }
        }
        let store_entity = self.store.clone();
        let store = store_entity.read(cx);
        // Keep the cursor row in view whenever the cursor moves, however it moved.
        if store.cursor != self.scrolled_to {
            if let Some(ix) = store.cursor_index() {
                self.list_scroll.scroll_to_item(ix, ScrollStrategy::Nearest);
            }
            self.scrolled_to = store.cursor.clone();
        }
        let header = self.header(store, cx);
        let banner = self.banner(store, cx);
        let toast = self.toast(store, cx);
        let help = (self.help_open && screen == Screen::Mail).then(|| self.help_overlay(cx));
        let main = match screen {
            Screen::Auth => self.auth_card(store, cx),
            Screen::Mail => self.panes(store, cx),
        };
        div()
            .key_context("Eloquawk")
            .on_action(cx.listener(|this, _: &Refresh, _, cx| {
                this.store.read(cx).send_command(Command::Refresh)
            }))
            .on_action(cx.listener(|_, _: &CloseWindow, window, _| window.remove_window()))
            // Later milestones: labels and search (M7), compose (M8), settings (M9).
            .on_action(cx.listener(|this, _: &OpenLabelPicker, _, cx| this.not_yet(cx)))
            .on_action(cx.listener(|this, _: &FocusSearch, _, cx| this.not_yet(cx)))
            .on_action(cx.listener(|this, _: &Compose, _, cx| this.not_yet(cx)))
            .on_action(cx.listener(|this, _: &Reply, _, cx| this.not_yet(cx)))
            .on_action(cx.listener(|this, _: &ReplyAll, _, cx| this.not_yet(cx)))
            .on_action(cx.listener(|this, _: &Forward, _, cx| this.not_yet(cx)))
            .on_action(cx.listener(|this, _: &OpenSettings, _, cx| this.not_yet(cx)))
            .on_action(cx.listener(|_, _: &Quit, _, cx| cx.quit()))
            .when_some(theme::ui_font(cx), |d, font| d.font_family(font))
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(p.bg))
            .text_color(rgb(p.text))
            .child(header)
            .children(banner)
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_hidden()
                    // The Mail context exists only with the panes: no Mail key fires on the
                    // auth screen.
                    .when(screen == Screen::Mail, |d| {
                        d.key_context("Mail")
                            .track_focus(&self.focus)
                            .on_action(cx.listener(Self::next_thread))
                            .on_action(cx.listener(Self::prev_thread))
                            .on_action(cx.listener(Self::open_thread))
                            .on_action(cx.listener(Self::back_to_list))
                            .on_action(cx.listener(Self::next_message))
                            .on_action(cx.listener(Self::prev_message))
                            .on_action(cx.listener(Self::scroll_down))
                            .on_action(cx.listener(Self::scroll_up))
                            .on_action(cx.listener(Self::toggle_select))
                            .on_action(cx.listener(Self::archive))
                            .on_action(cx.listener(Self::trash))
                            .on_action(cx.listener(Self::toggle_star))
                            .on_action(cx.listener(Self::mark_read))
                            .on_action(cx.listener(Self::mark_unread))
                            .on_action(cx.listener(Self::report_spam))
                            .on_action(cx.listener(Self::undo))
                            .on_action(cx.listener(Self::expand_all))
                            .on_action(cx.listener(Self::collapse_all))
                            .on_action(cx.listener(Self::toggle_help))
                            .on_action(cx.listener(|this, _: &GoInbox, window, cx| {
                                this.go_label(labels::INBOX, window, cx)
                            }))
                            .on_action(cx.listener(|this, _: &GoStarred, window, cx| {
                                this.go_label(labels::STARRED, window, cx)
                            }))
                            .on_action(cx.listener(|this, _: &GoSent, window, cx| {
                                this.go_label(labels::SENT, window, cx)
                            }))
                            .on_action(cx.listener(|this, _: &GoDrafts, window, cx| {
                                this.go_label(labels::DRAFT, window, cx)
                            }))
                            .on_action(cx.listener(|this, _: &GoAllMail, window, cx| {
                                this.go(ViewKind::AllMail, window, cx)
                            }))
                            .on_action(cx.listener(|this, _: &NextAccount, window, cx| {
                                this.switch_account(1, window, cx)
                            }))
                            .on_action(cx.listener(|this, _: &PrevAccount, window, cx| {
                                this.switch_account(-1, window, cx)
                            }))
                    })
                    .child(main),
            )
            .children(help)
            .children(toast)
    }
}
