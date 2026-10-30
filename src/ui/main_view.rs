//! `MainView`, the main window's root (SPEC 8.2): the header with search, the problem banner,
//! the sidebar, thread list and reader panes (or an auth card, or Settings), the toast, the
//! shortcut overlay and the label picker. It owns the Mail key context and handles every Mail
//! action of SPEC 9, runs searches from the header's `Search` context, opens Settings and
//! compose windows (new, reply, reply-all, forward), and answers the demo script's `expect:`
//! checks.

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::prelude::*;
use gpui::{
    AnyElement, App, Bounds, ClickEvent, Context, Div, Entity, FocusHandle, Focusable, FontWeight,
    Pixels, ScrollStrategy, StyleRefinement, Subscription, Task, UniformListScrollHandle, Window,
    canvas, div, px, rgb,
};

use super::auth::Screen;
use super::compose;
use super::label_picker::{LabelPicker, PickerEvent};
use super::reader::{ReaderEvent, ReaderView};
use super::settings::{SettingsEvent, SettingsView};
use super::text_input::{Submit, TextInput};
use super::theme::{self, CONTROL_H, HEADER_H, ICON_SLOT, LIST_W, SIDEBAR_W, palette, tint};
use super::widgets::{ButtonStyle, button, ghost_button};
use super::{
    ApplySuggestion1, ApplySuggestion2, ApplySuggestion3, Archive, BackToList, CancelSearch,
    CloseSearch, CloseWindow, CollapseAll, Compose, DismissTriage, ExpandAll, FocusSearch, Forward,
    GoAllMail, GoDrafts, GoInbox, GoSent, GoStarred, MarkRead, MarkUnread, NextAccount,
    NextMessage, NextThread, OpenLabelPicker, OpenSettings, OpenThread, PrevAccount, PrevMessage,
    PrevThread, Quit, Refresh, Reply, ReplyAll, ReportSpam, ScrollReaderDown, ScrollReaderUp,
    ToggleHelp, ToggleSelect, ToggleStar, Trash, Undo,
};
use crate::ai::jev::{Kind, Level};
use crate::branding;
use crate::demo_script::{Cond, FocusTarget};
use crate::model::{ComposeKind, LabelDelta, ThreadKey, ViewKind, labels};
use crate::protocol::Command;
use crate::store::{DetailState, MutationKind, NoticeAction, Row, Store};

/// How long a toast stays up.
const TOAST_FOR: Duration = Duration::from_secs(5);

/// Where the label picker sits within the reader pane: below its action bar, inset from the
/// pane's left edge.
const PICKER_TOP: f32 = 52.0;
const PICKER_LEFT: f32 = 20.0;

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
    /// The label picker, while it shows, with its event subscription.
    picker: Option<(Entity<LabelPicker>, Subscription)>,
    /// The Settings screen, while it shows, with its event subscription.
    settings: Option<(Entity<SettingsView>, Subscription)>,
    /// The reader pane's bounds as last laid out (window coordinates, so below the header and
    /// any banner), which the picker hangs from.
    reader_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// The search query the field was last filled from, so a change of view refills or
    /// clears it once instead of overwriting what the user types.
    shown_search: Option<String>,
    /// A reply or forward waiting for its thread's detail to load.
    pending_compose: Option<(ThreadKey, ComposeKind)>,
    /// The notice the toast timer is running for, and the timer (one, never per frame).
    toast_for: Option<Instant>,
    _toast_timer: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl MainView {
    pub fn new(store: Entity<Store>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let reader = cx.new(|cx| ReaderView::new(store.clone(), window, cx));
        let search = cx.new(|cx| TextInput::new("Search mail", false, 0, cx));
        let subscriptions = vec![
            cx.observe(&store, |this, store, cx| {
                this.schedule_toast_expiry(&store, cx);
                this.sync_search_field(&store, cx);
                this.fulfil_pending_compose(cx);
                cx.notify();
            }),
            cx.subscribe_in(&reader, window, |this, _, event, window, cx| match event {
                ReaderEvent::Labels(key) => this.open_picker(vec![key.clone()], window, cx),
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
            picker: None,
            settings: None,
            reader_bounds: Rc::new(Cell::new(None)),
            shown_search: None,
            pending_compose: None,
            toast_for: None,
            _toast_timer: None,
            _subscriptions: subscriptions,
        }
    }

    /// Focuses the Mail context (the list), as after opening the window or switching views.
    pub fn focus_mail(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
    }

    /// The screen to show: Settings while it is open, otherwise what the store calls for.
    pub(super) fn current_screen(&self, cx: &gpui::App) -> Screen {
        if self.settings.is_some() {
            Screen::Settings
        } else {
            Screen::of(self.store.read(cx))
        }
    }

    /// Shows the Settings screen (or focuses it when it already shows).
    pub fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.help_open = false;
        self.picker = None;
        let view = match &self.settings {
            Some((view, _)) => view.clone(),
            None => {
                let view = cx.new(|cx| SettingsView::new(self.store.clone(), window, cx));
                let sub =
                    cx.subscribe_in(&view, window, |this, _, event, window, cx| match event {
                        SettingsEvent::Close => this.close_settings(window, cx),
                    });
                self.settings = Some((view.clone(), sub));
                view
            }
        };
        window.focus(&view.focus_handle(cx), cx);
        cx.notify();
    }

    fn close_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((view, _)) = self.settings.take() {
            view.update(cx, |v, cx| v.closing(cx));
            self.focus_mail(window, cx);
            cx.notify();
        }
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

    // ----- search ---------------------------------------------------------------------------

    /// Fills the search field with the query of a search view when one is shown (also when a
    /// script or the store started it), and clears it once the view leaves search.
    fn sync_search_field(&mut self, store: &Entity<Store>, cx: &mut Context<Self>) {
        let query = store.read(cx).search_query().map(str::to_string);
        if query == self.shown_search {
            return;
        }
        let text = query.clone().unwrap_or_default();
        self.search.update(cx, |field, cx| field.set_text(text, cx));
        self.shown_search = query;
    }

    /// `/`: focuses the search field.
    fn focus_search(&mut self, _: &FocusSearch, window: &mut Window, cx: &mut Context<Self>) {
        let handle = self.search.focus_handle(cx);
        window.focus(&handle, cx);
    }

    /// Enter in the search field: shows the results for the current account (an empty field
    /// leaves a search) and focuses the list.
    fn submit_search(&mut self, _: &Submit, window: &mut Window, cx: &mut Context<Self>) {
        let query = self.search.read(cx).text().to_string();
        self.store.update(cx, |s, cx| {
            s.search(&query);
            cx.notify();
        });
        self.focus_mail(window, cx);
    }

    /// Escape in the search field: clears it and focuses the list.
    fn cancel_search(&mut self, _: &CancelSearch, window: &mut Window, cx: &mut Context<Self>) {
        self.search.update(cx, |field, cx| field.set_text("", cx));
        self.focus_mail(window, cx);
    }

    /// The search results' ✕: back to the view the search replaced.
    fn close_search(&mut self, _: &CloseSearch, window: &mut Window, cx: &mut Context<Self>) {
        self.store.update(cx, |s, cx| {
            if s.end_search() {
                cx.notify();
            }
        });
        self.focus_mail(window, cx);
    }

    /// The demo script's `search:<query>`: fills the field and submits it.
    pub fn search_for(&mut self, query: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.search
            .update(cx, |field, cx| field.set_text(query.to_string(), cx));
        self.submit_search(&Submit, window, cx);
    }

    // ----- label picker -------------------------------------------------------------------

    /// `l`: the picker for the targets (the selection, else the open thread, else the cursor
    /// row).
    fn open_label_picker(
        &mut self,
        _: &OpenLabelPicker,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let targets = self.store.read(cx).targets();
        if targets.is_empty() {
            self.store.update(cx, |s, cx| {
                s.inform("No conversation to label");
                cx.notify();
            });
            return;
        }
        self.open_picker(targets, window, cx);
    }

    /// Shows the picker for these threads, with the focus on its filter.
    fn open_picker(
        &mut self,
        targets: Vec<ThreadKey>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = self.store.clone();
        let picker = cx.new(|cx| LabelPicker::new(store, targets, cx));
        let sub = cx.subscribe_in(&picker, window, |this, _, event, window, cx| match event {
            PickerEvent::Close => this.close_picker(window, cx),
        });
        let focus = picker.read(cx).filter_focus(cx);
        self.picker = Some((picker, sub));
        window.focus(&focus, cx);
        cx.notify();
    }

    /// Closes the picker and gives the focus back to the list.
    pub(super) fn close_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.picker.take().is_some() {
            self.focus_mail(window, cx);
            cx.notify();
        }
    }

    /// The picker under the reader's action bar, over a scrim that closes it on click. It
    /// hangs from the reader pane as last laid out, so it follows the banner and the pane
    /// widths; before the first layout it falls back to where the pane starts without them.
    fn picker_overlay(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let (picker, _) = self.picker.as_ref()?;
        let (top, left) = match self.reader_bounds.get() {
            Some(b) => (b.top() + px(PICKER_TOP), b.left() + px(PICKER_LEFT)),
            None => (
                px(HEADER_H + PICKER_TOP),
                px(SIDEBAR_W + LIST_W + PICKER_LEFT),
            ),
        };
        Some(
            div()
                .id("picker-scrim")
                // Keeps the panes underneath from getting the click, hover and scroll.
                .occlude()
                .absolute()
                .inset_0()
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                    this.close_picker(window, cx);
                }))
                .child(div().absolute().top(top).left(left).child(picker.clone()))
                .into_any_element(),
        )
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

    /// `1`/`2`/`3`: the open thread's nth suggested label, on that thread only. Nothing (and
    /// no notice) when there is no such suggestion or the reader is closed (SPEC 16.12.3).
    fn apply_suggestion(&mut self, n: usize, cx: &mut Context<Self>) {
        self.store.update(cx, |s, cx| {
            if s.apply_suggestion(n).is_some() {
                cx.notify();
            }
        });
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

    // ----- compose -------------------------------------------------------------------------

    /// `c`, ctrl-n and the sidebar's Compose: a new message from the current account.
    pub(super) fn compose_new(&mut self, cx: &mut Context<Self>) {
        let Some(account) = self.store.read(cx).current_account() else {
            self.store.update(cx, |s, cx| {
                s.inform("Add an account to write mail");
                cx.notify();
            });
            return;
        };
        super::windows::open_compose_window(&self.store, compose::blank(&account), cx);
    }

    /// `r`, `a`, `f` and the reader's buttons: answers the open thread, else the cursor row.
    /// Without its detail yet, the thread opens and the window follows once it loads.
    fn compose_reply(&mut self, kind: ComposeKind, cx: &mut Context<Self>) {
        let key = {
            let s = self.store.read(cx);
            s.open.clone().or_else(|| s.cursor.clone())
        };
        let Some(key) = key else {
            let text = if kind == ComposeKind::Forward {
                "No conversation to forward"
            } else {
                "No conversation to reply to"
            };
            self.store.update(cx, |s, cx| {
                s.inform(text);
                cx.notify();
            });
            return;
        };
        self.pending_compose = Some((key.clone(), kind));
        let loaded = matches!(
            self.store.read(cx).details.get(&key),
            Some(DetailState::Loaded(_))
        );
        if !loaded || self.store.read(cx).open.as_ref() != Some(&key) {
            self.store.update(cx, |s, cx| {
                s.open_thread(key);
                cx.notify();
            });
        }
        self.fulfil_pending_compose(cx);
    }

    /// Opens the waiting reply or forward once its detail has loaded (or gives up when it
    /// failed to).
    fn fulfil_pending_compose(&mut self, cx: &mut Context<Self>) {
        let Some((key, kind)) = self.pending_compose.clone() else {
            return;
        };
        // The user moved on (another thread, another view) before the detail arrived: a
        // window for the earlier thread popping up later would be a surprise.
        if self.store.read(cx).open.as_ref() != Some(&key) {
            self.pending_compose = None;
            return;
        }
        let prefill = {
            let s = self.store.read(cx);
            match s.details.get(&key) {
                Some(DetailState::Loaded(d)) => {
                    Some(compose::reply_prefill(d, kind, s.prefs.clock_24h))
                }
                Some(DetailState::Failed(_)) => Some(None),
                Some(DetailState::Loading) => None,
                None => Some(None),
            }
        };
        let Some(prefill) = prefill else {
            return;
        };
        self.pending_compose = None;
        match prefill {
            Some(prefill) => {
                // Not from inside the store's notification.
                let store = self.store.clone();
                cx.defer(move |cx| super::windows::open_compose_window(&store, prefill, cx));
            }
            None => self.store.update(cx, |s, cx| {
                s.inform("Couldn't load the conversation");
                cx.notify();
            }),
        }
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

    /// The demo script's `settings:<card>`: opens Settings and scrolls it to the card.
    pub fn open_settings_at(
        &mut self,
        card: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.open_settings(window, cx);
        let view = self
            .settings
            .as_ref()
            .map(|(v, _)| v.clone())
            .ok_or("no settings")?;
        if view.update(cx, |v, cx| v.scroll_to_card(card, cx)) {
            Ok(())
        } else {
            Err(format!("no card {card}"))
        }
    }

    /// The demo script's `link:N`: follows the open target's n-th web link, guard and all.
    pub fn follow_link(&mut self, n: usize, cx: &mut Context<Self>) -> Result<(), String> {
        self.reader
            .update(cx, |reader, cx| reader.follow_nth_link(n, cx))
    }

    /// Saves the open thread's attachment `n` (the demo's `save:N` step).
    pub fn save_attachment(&self, n: usize, cx: &gpui::App) -> Result<(), String> {
        self.reader.read(cx).save_attachment(n, cx)
    }

    // ----- demo script checks -------------------------------------------------------------

    fn focus_target(&self, window: &Window, cx: &gpui::App) -> Option<FocusTarget> {
        if self.search.focus_handle(cx).is_focused(window) {
            Some(FocusTarget::Search)
        } else if self
            .picker
            .as_ref()
            .is_some_and(|(p, _)| p.read(cx).filter_focus(cx).is_focused(window))
        {
            Some(FocusTarget::Picker)
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
            Cond::Picker(want) => {
                let open = self.picker.is_some();
                (open == *want, if open { "open" } else { "closed" }.into())
            }
            Cond::Filter(want) => {
                let actual = self
                    .picker
                    .as_ref()
                    .map(|(p, _)| p.read(cx).filter_text(cx));
                (
                    actual.as_deref() == Some(want.as_str()),
                    actual.unwrap_or_else(|| "no picker".into()),
                )
            }
            Cond::Label { name, on } => {
                let thread = store.cursor.as_ref().and_then(|k| store.threads.get(k));
                let id = thread.and_then(|t| {
                    store
                        .labels
                        .get(&t.key.account)?
                        .iter()
                        .find(|l| l.name == *name)
                        .map(|l| l.id.clone())
                });
                match (thread, id) {
                    (Some(t), Some(id)) => {
                        let has = t.in_label(&id);
                        (has == *on, if has { "yes" } else { "no" }.into())
                    }
                    (None, _) => (false, "no cursor thread".into()),
                    (_, None) => (false, "no such label".into()),
                }
            }
            Cond::Badge { name, count } => {
                let id = store.view.as_ref().and_then(|v| {
                    store
                        .labels
                        .get(&v.account)?
                        .iter()
                        .find(|l| l.name == *name)
                        .map(|l| (v.account.clone(), l.id.clone()))
                });
                match id {
                    Some((account, id)) => {
                        let n = store.unread_badge(&account, &id);
                        (n == *count, n.map_or("none".into(), |n| n.to_string()))
                    }
                    None => (false, "no such label".into()),
                }
            }
            Cond::Messages(want) => {
                let n = store
                    .open
                    .as_ref()
                    .and_then(|k| match store.details.get(k) {
                        Some(DetailState::Loaded(d)) => Some(d.messages.len()),
                        _ => None,
                    });
                (
                    n == Some(*want),
                    n.map_or("no open thread".into(), |n| n.to_string()),
                )
            }
            Cond::Composers(want) => {
                let n = compose::open_count(cx);
                (n == *want, n.to_string())
            }
            Cond::Ai(want) => {
                let actual = store.open.as_ref().map_or("none", |key| {
                    let banner = store.ai_banner(key).map(|v| (v.level, v.kind));
                    match banner {
                        Some((Level::Danger, Kind::Scam)) => "danger-scam",
                        Some((Level::Danger, _)) => "danger-phishing",
                        Some((Level::Warning, _)) => "warning",
                        _ if store.local_caution(key).is_some() => "caution",
                        Some((Level::Spam, _)) => "spam",
                        _ if store.ai.paused.is_some() => "paused",
                        _ if store.ai_checking(key) => "checking",
                        _ if store.ai_failure(key).is_some() => "failed",
                        _ => "none",
                    }
                });
                (want == actual, actual.to_string())
            }
            Cond::Suggest(want) => {
                let actual: Vec<String> = store
                    .open
                    .as_ref()
                    .map(|key| store.suggestions(key).into_iter().map(|s| s.name).collect())
                    .unwrap_or_default();
                (*want == actual, actual.join(";"))
            }
            Cond::Screen(want) => {
                let actual = match self.current_screen(cx) {
                    Screen::Mail => "mail",
                    Screen::Settings => "settings",
                    Screen::Auth => "auth",
                };
                (want == actual, actual.to_string())
            }
            // Checked against the focused compose window; here none is focused.
            Cond::Compose(_) | Cond::ComposeError(_) | Cond::Attached(_) | Cond::Discard(_) => {
                (false, "the main window is focused".into())
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
            .child(branding::icon(ICON_SLOT))
            .child(
                div()
                    .key_context("Search")
                    .on_action(cx.listener(Self::submit_search))
                    .on_action(cx.listener(Self::cancel_search))
                    .flex_1()
                    .min_w(px(0.0))
                    .max_w(px(680.0))
                    .child(self.search.clone()),
            )
            .when_some(store.search_query(), |d, q| {
                d.child(
                    div()
                        .flex()
                        .flex_shrink_0()
                        .min_w(px(0.0))
                        .max_w(px(320.0))
                        .items_center()
                        .gap(px(4.0))
                        .pl(px(10.0))
                        .pr(px(2.0))
                        .h(px(CONTROL_H))
                        .rounded(px(6.0))
                        .bg(rgb(tint(p.accent, p.header, 0.14)))
                        .child(
                            div()
                                .min_w(px(0.0))
                                .truncate()
                                .text_size(px(12.5))
                                .text_color(rgb(p.text))
                                .child(format!("Search results for \u{201c}{q}\u{201d}")),
                        )
                        .child(ghost_button(
                            p,
                            "close-search",
                            "✕",
                            Some(Box::new(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.close_search(&CloseSearch, window, cx)
                            }))),
                        )),
                )
            })
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
                        Box::new(
                            cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.store.read(cx).refresh()
                            }),
                        )
                    }),
                )
                .h(px(CONTROL_H)),
            )
            .child(
                ghost_button(
                    p,
                    "settings",
                    "Settings",
                    Some(Box::new(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.open_settings(window, cx)
                    }))),
                )
                .h(px(CONTROL_H)),
            )
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
        let undo = match &notice.action {
            Some(NoticeAction::Undo) => Some(button(
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
            )),
            Some(NoticeAction::OpenLink(url)) => {
                let url = url.clone();
                Some(button(
                    p,
                    "toast-open-link",
                    "Open link",
                    ButtonStyle::Secondary,
                    // The same opener as an unguarded click; the URL is never logged.
                    Some(Box::new(move |_: &ClickEvent, _, cx: &mut App| {
                        super::reader::spawn_open(url.clone(), cx)
                    })),
                ))
            }
            Some(NoticeAction::OpenFolder(path)) => {
                let path = path.clone();
                Some(button(
                    p,
                    "toast-open-folder",
                    "Open folder",
                    ButtonStyle::Secondary,
                    // The file manager opens on the folder with the saved file selected
                    // (the portal's OpenDirectory, or xdg-open on the folder).
                    Some(Box::new(move |_: &ClickEvent, _, cx: &mut App| {
                        cx.reveal_path(&path)
                    })),
                ))
            }
            None => None,
        };
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
                div()
                    .relative()
                    .flex_1()
                    .min_w(px(0.0))
                    .h_full()
                    .child({
                        // Records the pane's bounds for the label picker; paints nothing.
                        let cell = self.reader_bounds.clone();
                        canvas(move |bounds, _, _| cell.set(Some(bounds)), |_, _, _, _| {})
                            .absolute()
                            .size_full()
                    })
                    .child(
                        self.reader
                            .clone()
                            .cached(StyleRefinement::default().size_full()),
                    ),
            )
            .into_any_element()
    }
}

/// Wraps a Mail action handler so it does nothing while the shortcut overlay shows: the list
/// keeps the focus under the overlay, and without this `e`, `#` or `j` would act on threads the
/// user cannot see. `?` and escape/`u` are not wrapped; they close the overlay.
fn unless_help<A>(
    f: impl Fn(&mut MainView, &A, &mut Window, &mut Context<MainView>) + 'static,
) -> impl Fn(&mut MainView, &A, &mut Window, &mut Context<MainView>) + 'static {
    move |this, action, window, cx| {
        if !this.help_open {
            f(this, action, window, cx);
        }
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
        let screen = self.current_screen(cx);
        if screen != self.screen {
            self.screen = screen;
            if screen == Screen::Mail {
                cx.defer_in(window, |this, window, cx| this.focus_mail(window, cx));
            }
        }
        // The picker only exists with the panes: leaving Mail (a sign-out, an auth failure)
        // drops it, so it neither keeps the focus unseen nor comes back later.
        if screen != Screen::Mail {
            self.picker = None;
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
        let picker = if screen == Screen::Mail {
            self.picker_overlay(cx)
        } else {
            None
        };
        let main = match (screen, &self.settings) {
            (Screen::Settings, Some((view, _))) => view.clone().into_any_element(),
            (Screen::Mail, _) => self.panes(store, cx),
            _ => self.auth_card(store, cx),
        };
        div()
            .key_context("Eloquawk")
            .on_action(cx.listener(|this, _: &Refresh, _, cx| this.store.read(cx).refresh()))
            .on_action(cx.listener(|_, _: &CloseWindow, window, _| window.remove_window()))
            // On the root so the reader's buttons and scripts reach them from any focus.
            .on_action(cx.listener(unless_help(Self::open_label_picker)))
            .on_action(cx.listener(unless_help(Self::focus_search)))
            .on_action(cx.listener(Self::close_search))
            .on_action(cx.listener(unless_help(|this, _: &Compose, _, cx| this.compose_new(cx))))
            .on_action(cx.listener(unless_help(|this, _: &Reply, _, cx| {
                this.compose_reply(ComposeKind::Reply, cx)
            })))
            .on_action(cx.listener(unless_help(|this, _: &ReplyAll, _, cx| {
                this.compose_reply(ComposeKind::ReplyAll, cx)
            })))
            .on_action(cx.listener(unless_help(|this, _: &Forward, _, cx| {
                this.compose_reply(ComposeKind::Forward, cx)
            })))
            .on_action(
                cx.listener(|this, _: &OpenSettings, window, cx| this.open_settings(window, cx)),
            )
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
                            .on_action(cx.listener(unless_help(Self::next_thread)))
                            .on_action(cx.listener(unless_help(Self::prev_thread)))
                            .on_action(cx.listener(unless_help(Self::open_thread)))
                            .on_action(cx.listener(Self::back_to_list))
                            .on_action(cx.listener(unless_help(Self::next_message)))
                            .on_action(cx.listener(unless_help(Self::prev_message)))
                            .on_action(cx.listener(unless_help(Self::scroll_down)))
                            .on_action(cx.listener(unless_help(Self::scroll_up)))
                            .on_action(cx.listener(unless_help(Self::toggle_select)))
                            .on_action(cx.listener(unless_help(Self::archive)))
                            .on_action(cx.listener(unless_help(Self::trash)))
                            .on_action(cx.listener(unless_help(Self::toggle_star)))
                            .on_action(cx.listener(unless_help(Self::mark_read)))
                            .on_action(cx.listener(unless_help(Self::mark_unread)))
                            .on_action(cx.listener(unless_help(Self::report_spam)))
                            .on_action(cx.listener(unless_help(
                                |this, _: &DismissTriage, _, cx| {
                                    this.store.update(cx, |s, cx| {
                                        s.dismiss_triage();
                                        cx.notify();
                                    })
                                },
                            )))
                            .on_action(cx.listener(unless_help(
                                |this, _: &ApplySuggestion1, _, cx| this.apply_suggestion(0, cx),
                            )))
                            .on_action(cx.listener(unless_help(
                                |this, _: &ApplySuggestion2, _, cx| this.apply_suggestion(1, cx),
                            )))
                            .on_action(cx.listener(unless_help(
                                |this, _: &ApplySuggestion3, _, cx| this.apply_suggestion(2, cx),
                            )))
                            .on_action(cx.listener(unless_help(Self::undo)))
                            .on_action(cx.listener(unless_help(Self::expand_all)))
                            .on_action(cx.listener(unless_help(Self::collapse_all)))
                            .on_action(cx.listener(Self::toggle_help))
                            .on_action(cx.listener(unless_help(|this, _: &GoInbox, window, cx| {
                                this.go_label(labels::INBOX, window, cx)
                            })))
                            .on_action(cx.listener(unless_help(
                                |this, _: &GoStarred, window, cx| {
                                    this.go_label(labels::STARRED, window, cx)
                                },
                            )))
                            .on_action(cx.listener(unless_help(|this, _: &GoSent, window, cx| {
                                this.go_label(labels::SENT, window, cx)
                            })))
                            .on_action(cx.listener(unless_help(
                                |this, _: &GoDrafts, window, cx| {
                                    this.go_label(labels::DRAFT, window, cx)
                                },
                            )))
                            .on_action(cx.listener(unless_help(
                                |this, _: &GoAllMail, window, cx| {
                                    this.go(ViewKind::AllMail, window, cx)
                                },
                            )))
                            .on_action(cx.listener(unless_help(
                                |this, _: &NextAccount, window, cx| {
                                    this.switch_account(1, window, cx)
                                },
                            )))
                            .on_action(cx.listener(unless_help(
                                |this, _: &PrevAccount, window, cx| {
                                    this.switch_account(-1, window, cx)
                                },
                            )))
                    })
                    .child(main),
            )
            .children(picker)
            .children(help)
            .children(toast)
    }
}
