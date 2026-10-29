//! GPUI front end: the main window's three panes over the shared [`crate::store::Store`], and
//! the actions and key map (SPEC 9).
//!
//! Key contexts: `Eloquawk` on the main window root (always active), `Mail` on the panes
//! container and `ReadOnlyText` on each message body (ctrl-c, ctrl-a). Mail bindings use the
//! predicate [`MAIL`], so no Mail key fires while a text field anywhere inside is focused; a
//! focused body does not block them. M4 binds the navigation keys; M6 binds the rest.

pub mod auth;
pub mod body_text;
pub mod format;
pub mod main_view;
pub mod reader;
pub mod sidebar;
pub mod text_input;
pub mod text_layout;
pub mod theme;
pub mod thread_list;
pub mod widgets;
pub mod windows;

use gpui::{App, KeyBinding, actions};

actions!(
    eloquawk,
    [
        /// Cursor down; opens the next thread when the reader is open.
        NextThread,
        /// Cursor up; opens the previous thread when the reader is open.
        PrevThread,
        /// Opens the cursor thread.
        OpenThread,
        /// Closes the reader (or clears the selection when it is already closed).
        BackToList,
        Refresh,
        CloseWindow,
        Quit,
    ]
);

/// The predicate every Mail binding uses: never while typing in a field.
pub const MAIL: &str = "Mail && !TextInput && !TextArea";

pub fn bind_keys(cx: &mut App) {
    text_input::bind_keys(cx);
    body_text::bind_keys(cx);
    let mail = Some(MAIL);
    cx.bind_keys([
        KeyBinding::new("j", NextThread, mail),
        KeyBinding::new("down", NextThread, mail),
        KeyBinding::new("k", PrevThread, mail),
        KeyBinding::new("up", PrevThread, mail),
        KeyBinding::new("o", OpenThread, mail),
        KeyBinding::new("enter", OpenThread, mail),
        KeyBinding::new("u", BackToList, mail),
        KeyBinding::new("escape", BackToList, mail),
        KeyBinding::new("ctrl-w", CloseWindow, Some("Eloquawk")),
        KeyBinding::new("ctrl-q", Quit, None),
    ]);
}
