//! Searchable command launcher for the main mail window.
use super::label_picker::step;
use super::text_input::{Submit, TextInput};
use super::theme::palette;
use super::{ClosePicker, PickerDown, PickerUp};
use gpui::prelude::*;
use gpui::{
    App, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    ScrollHandle, SharedString, Subscription, Window, div, px, rgb,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Compose,
    Search,
    Inbox,
    Starred,
    Sent,
    Drafts,
    AllMail,
    Archive,
    Trash,
    Star,
    MarkRead,
    MarkUnread,
    Labels,
    Reply,
    ReplyAll,
    Forward,
    Refresh,
    Settings,
    Sidebar,
    Help,
}

const COMMANDS: &[(Command, &str, &str)] = &[
    (Command::Compose, "Compose", "c"),
    (Command::Search, "Search mail", "/"),
    (Command::Inbox, "Go to Inbox", "g i"),
    (Command::Starred, "Go to Starred", "g s"),
    (Command::Sent, "Go to Sent", "g t"),
    (Command::Drafts, "Go to Drafts", "g d"),
    (Command::AllMail, "Go to All Mail", "g a"),
    (Command::Archive, "Archive conversation", "e"),
    (Command::Trash, "Trash conversation", "#"),
    (Command::Star, "Star or unstar", "s"),
    (Command::MarkRead, "Mark as read", "Shift I"),
    (Command::MarkUnread, "Mark as unread", "Shift U"),
    (Command::Labels, "Label conversation", "l"),
    (Command::Reply, "Reply", "r"),
    (Command::ReplyAll, "Reply all", "a"),
    (Command::Forward, "Forward", "f"),
    (Command::Refresh, "Refresh", "Ctrl R"),
    (Command::Settings, "Settings", "Ctrl ,"),
    (Command::Sidebar, "Fold or unfold the sidebar", "Ctrl B"),
    (Command::Help, "Keyboard shortcuts", "?"),
];

fn matching(query: &str) -> Vec<(Command, &'static str, &'static str)> {
    let query = query.trim().to_lowercase();
    COMMANDS
        .iter()
        .copied()
        .filter(|(_, name, _)| name.to_lowercase().contains(&query))
        .collect()
}

pub enum PaletteEvent {
    Close,
    Run(Command),
}
pub struct CommandPalette {
    filter: Entity<TextInput>,
    selected: usize,
    last_filter: String,
    scroll: ScrollHandle,
    _subscription: Subscription,
}
impl EventEmitter<PaletteEvent> for CommandPalette {}
impl CommandPalette {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| TextInput::new("Search commands", false, 0, cx));
        let subscription = cx.observe(&filter, |_, _, cx| cx.notify());
        Self {
            filter,
            selected: 0,
            last_filter: String::new(),
            scroll: ScrollHandle::new(),
            _subscription: subscription,
        }
    }
    pub fn filter_focus(&self, cx: &App) -> FocusHandle {
        self.filter.focus_handle(cx)
    }
    fn rows(&self, cx: &App) -> Vec<(Command, &'static str, &'static str)> {
        matching(self.filter.read(cx).text())
    }
    fn sync(&mut self, cx: &App) {
        let text = self.filter.read(cx).text();
        if text != self.last_filter {
            self.last_filter = text.to_string();
            self.selected = 0;
            self.scroll.scroll_to_item(0);
        }
        self.selected = self.selected.min(self.rows(cx).len().saturating_sub(1));
    }
    fn up(&mut self, _: &PickerUp, _: &mut Window, cx: &mut Context<Self>) {
        self.sync(cx);
        self.selected = step(self.selected, -1, self.rows(cx).len());
        self.scroll.scroll_to_item(self.selected);
        cx.notify();
    }
    fn down(&mut self, _: &PickerDown, _: &mut Window, cx: &mut Context<Self>) {
        self.sync(cx);
        self.selected = step(self.selected, 1, self.rows(cx).len());
        self.scroll.scroll_to_item(self.selected);
        cx.notify();
    }
    fn submit(&mut self, _: &Submit, _: &mut Window, cx: &mut Context<Self>) {
        self.sync(cx);
        if let Some((command, _, _)) = self.rows(cx).get(self.selected) {
            cx.emit(PaletteEvent::Run(*command));
        }
    }
}
impl Render for CommandPalette {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync(cx);
        let p = palette(cx);
        let rows = self.rows(cx);
        div()
            .id("command-palette")
            .key_context("CommandPalette")
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::submit))
            .on_action(cx.listener(|_, _: &ClosePicker, _, cx| cx.emit(PaletteEvent::Close)))
            .on_click(|_, _, cx| cx.stop_propagation())
            .w(px(480.0))
            .max_w_full()
            .max_h_full()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .p(px(12.0))
            .rounded(px(8.0))
            .border_1()
            .border_color(rgb(p.selected_border))
            .bg(rgb(p.panel_raised))
            .shadow_lg()
            .child(
                div()
                    .text_size(px(14.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Commands"),
            )
            .child(self.filter.clone())
            .child(
                div()
                    .id("command-rows")
                    .max_h(px(360.0))
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .flex()
                    .flex_col()
                    .when(rows.is_empty(), |d| {
                        d.child(
                            div()
                                .p(px(10.0))
                                .text_color(rgb(p.caption))
                                .child("No matching commands"),
                        )
                    })
                    .children(rows.into_iter().enumerate().map(
                        |(ix, (command, name, shortcut))| {
                            div()
                                .id(SharedString::from(format!("command-{ix}")))
                                .flex()
                                .items_center()
                                .gap(px(12.0))
                                .px(px(10.0))
                                .py(px(7.0))
                                .rounded(px(5.0))
                                .when(ix == self.selected, |d| d.bg(rgb(p.selected)))
                                .hover(move |s| s.bg(rgb(p.hover)))
                                .cursor_pointer()
                                .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                                    cx.emit(PaletteEvent::Run(command))
                                }))
                                .child(div().flex_1().child(name))
                                .child(
                                    div()
                                        .text_size(px(12.0))
                                        .text_color(rgb(p.caption))
                                        .child(shortcut),
                                )
                        },
                    )),
            )
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(rgb(p.caption))
                    .child("↑ ↓ to move · Enter to run · Esc to close"),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn filters_case_insensitively() {
        assert_eq!(matching(" INBOX ")[0].0, Command::Inbox);
        assert!(matching("missing").is_empty());
    }
}
