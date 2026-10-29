//! `BodyText`: a read-only, selectable and copyable message body with clickable links
//! (SPEC 8.2), built on [`super::text_layout`].
//!
//! The reader keeps one entity per expanded message (and one per unfolded quote). Mouse
//! down, drag and up select; a double-click selects a word; ctrl-c copies the selection and
//! ctrl-a selects the whole body, both bound in the `ReadOnlyText` key context, which Mail keys
//! do not exclude. Escape first drops a selection; with none it falls through to the Mail
//! binding. A click (a press and release without a drag) on a link emits
//! [`BodyEvent::OpenLink`]; the reader decides what opening means. Only the targets that
//! `mail/links.rs` produces (http, https and mailto) can ever be emitted.

use std::ops::Range;

use gpui::prelude::*;
use gpui::{
    App, ClipboardItem, Context, EventEmitter, FocusHandle, Focusable, KeyBinding, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, SharedString, Window, actions,
    div, px, rgb,
};

use super::text_layout::{SelectableText, TextLayout, normalise, word_at};
use super::theme::palette;
use crate::mail::links::{LinkTarget, find_links};

actions!(
    body_text,
    [
        /// Copies the selection to the clipboard.
        Copy,
        /// Selects the whole body.
        SelectAll,
        /// Drops the selection; with nothing selected the key falls through to the Mail
        /// binding (escape closes the thread).
        ClearSelection,
    ]
);

pub fn bind_keys(cx: &mut App) {
    let ctx = Some("ReadOnlyText");
    cx.bind_keys([
        KeyBinding::new("ctrl-c", Copy, ctx),
        KeyBinding::new("ctrl-a", SelectAll, ctx),
        KeyBinding::new("escape", ClearSelection, ctx),
    ]);
}

/// A press moving further than this is a drag, not a click.
const DRAG_SLOP: f32 = 3.0;

pub enum BodyEvent {
    /// The user started selecting here; other bodies drop their selection (a selection never
    /// spans two bodies).
    Selecting,
    /// A link was clicked.
    OpenLink(LinkTarget),
}

/// A press in progress.
struct Press {
    at: Point<Pixels>,
    dragged: bool,
    /// The link under the press, if any.
    link: Option<usize>,
}

pub struct BodyText {
    focus: FocusHandle,
    text: SharedString,
    links: Vec<(Range<usize>, LinkTarget)>,
    selection: Range<usize>,
    anchor: usize,
    press: Option<Press>,
    /// Quoted text renders in the secondary colour.
    secondary: bool,
    layout: TextLayout,
}

impl EventEmitter<BodyEvent> for BodyText {}

impl BodyText {
    pub fn new(text: String, secondary: bool, cx: &mut Context<Self>) -> Self {
        Self {
            focus: cx.focus_handle(),
            links: find_links(&text),
            text: text.into(),
            selection: 0..0,
            anchor: 0,
            press: None,
            secondary,
            layout: TextLayout::default(),
        }
    }

    /// Replaces the text (a refreshed message), dropping the selection when it changed.
    pub fn set_text(&mut self, text: String, cx: &mut Context<Self>) {
        if *self.text != *text {
            self.links = find_links(&text);
            self.text = text.into();
            self.selection = 0..0;
            self.anchor = 0;
            cx.notify();
        }
    }

    pub fn selected_text(&self) -> &str {
        self.text.get(self.selection.clone()).unwrap_or_default()
    }

    pub fn clear_selection(&mut self, cx: &mut Context<Self>) {
        if !self.selection.is_empty() {
            self.selection = 0..0;
            cx.notify();
        }
    }

    /// Selects the whole body and focuses it, as ctrl-a does.
    pub fn select_all_and_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
        self.select_all(&SelectAll, window, cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(BodyEvent::Selecting);
        self.anchor = 0;
        self.selection = 0..self.text.len();
        cx.notify();
    }

    fn escape(&mut self, _: &ClearSelection, _: &mut Window, cx: &mut Context<Self>) {
        if self.selection.is_empty() {
            cx.propagate();
        } else {
            self.clear_selection(cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        let selected = self.selected_text();
        if !selected.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(selected.to_string()));
        }
    }

    fn link_at(&self, ix: usize) -> Option<usize> {
        self.links.iter().position(|(r, _)| r.contains(&ix))
    }

    fn on_mouse_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
        cx.emit(BodyEvent::Selecting);
        let Some(ix) = self.layout.closest_index(ev.position) else {
            return;
        };
        let link = self
            .layout
            .exact_index(ev.position)
            .and_then(|i| self.link_at(i));
        if ev.click_count == 2 {
            self.selection = word_at(&self.text, ix);
            self.anchor = self.selection.start;
            self.press = None;
        } else if ev.modifiers.shift {
            self.selection = normalise(self.anchor, ix);
            self.press = Some(Press {
                at: ev.position,
                dragged: true,
                link: None,
            });
        } else {
            self.anchor = ix;
            self.selection = ix..ix;
            self.press = Some(Press {
                at: ev.position,
                dragged: false,
                link,
            });
        }
        cx.notify();
    }

    fn on_mouse_move(&mut self, ev: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(press) = self.press.as_mut() else {
            return;
        };
        if !ev.dragging() {
            return;
        }
        let moved = ev.position - press.at;
        if !press.dragged && moved.x.abs().max(moved.y.abs()) < px(DRAG_SLOP) {
            return;
        }
        press.dragged = true;
        if let Some(ix) = self.layout.closest_index(ev.position) {
            let next = normalise(self.anchor, ix);
            if next != self.selection {
                self.selection = next;
                cx.notify();
            }
        }
    }

    fn on_mouse_up(&mut self, ev: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(press) = self.press.take() else {
            return;
        };
        if press.dragged {
            return;
        }
        // A click: open the link it started and ended on.
        let released_on = self
            .layout
            .exact_index(ev.position)
            .and_then(|i| self.link_at(i));
        if let Some(link) = press.link
            && released_on == Some(link)
        {
            cx.emit(BodyEvent::OpenLink(self.links[link].1.clone()));
        }
    }
}

impl Focusable for BodyText {
    fn focus_handle(&self, _: &App) -> gpui::FocusHandle {
        self.focus.clone()
    }
}

impl Render for BodyText {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        div()
            .key_context("ReadOnlyText")
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::escape))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .w_full()
            .text_color(rgb(if self.secondary { p.secondary } else { p.text }))
            .child(SelectableText {
                layout: self.layout.clone(),
                text: self.text.clone(),
                links: self.links.iter().map(|(r, _)| r.clone()).collect(),
                selection: self.selection.clone(),
                selection_color: rgb(p.selection).into(),
                link_color: rgb(p.link).into(),
                on_mouse_move: Some(Box::new(cx.listener(Self::on_mouse_move))),
            })
    }
}
