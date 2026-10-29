//! `MainView`, the main window's root (SPEC 8.2): the header with search, the problem banner,
//! the sidebar, thread list and reader panes (or an auth card), and the toast. It owns the
//! Mail key context and its navigation actions, and answers the demo script's `expect:`
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
use super::{BackToList, CloseWindow, NextThread, OpenThread, PrevThread, Quit, Refresh};
use crate::demo_script::{Cond, FocusTarget};
use crate::model::{ThreadKey, ViewKind};
use crate::protocol::Command;
use crate::store::{NoticeAction, Store};

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

    fn open_thread(&mut self, _: &OpenThread, _: &mut Window, cx: &mut Context<Self>) {
        self.store.update(cx, |s, cx| {
            s.open_cursor();
            cx.notify();
        });
    }

    fn back_to_list(&mut self, _: &BackToList, window: &mut Window, cx: &mut Context<Self>) {
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
                    })
                    .child(main),
            )
            .children(toast)
    }
}
