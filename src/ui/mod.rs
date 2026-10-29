//! GPUI front end: the main window's three panes over the shared [`crate::store::Store`], and
//! the actions and key map (SPEC 9).
//!
//! Key contexts: `Eloquawk` on the main window root (always active), `Mail` on the panes
//! container, `ReadOnlyText` on each message body (ctrl-c, ctrl-a), `Search` on the header's
//! search field wrapper and `LabelPicker` on the label picker popover. Mail bindings use the
//! predicate [`MAIL`], so no Mail key fires while a text field anywhere inside is focused; a
//! focused body does not block them. The help overlay lists the same table ([`help::SHORTCUTS`]),
//! and a test keeps the two in step.

pub mod auth;
pub mod body_text;
pub mod format;
pub mod help;
pub mod label_picker;
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
        /// Focuses the next message card in the reader and expands it.
        NextMessage,
        /// Focuses the previous message card in the reader and expands it.
        PrevMessage,
        /// Opens the cursor thread; in the reader, toggles the focused message.
        OpenThread,
        /// Closes the help overlay, else the reader, else clears the selection.
        BackToList,
        /// Scrolls the reader down by a page.
        ScrollReaderDown,
        /// Scrolls the reader up by a page.
        ScrollReaderUp,
        /// Toggles the cursor thread in the selection.
        ToggleSelect,
        /// Archives (Not spam in the Spam view).
        Archive,
        /// Moves to Trash (Restore in the Trash view).
        Trash,
        ToggleStar,
        MarkRead,
        MarkUnread,
        /// Reports spam (Not spam in the Spam view).
        ReportSpam,
        OpenLabelPicker,
        Reply,
        ReplyAll,
        Forward,
        Compose,
        /// Undoes the last undoable change while its notice shows.
        Undo,
        ExpandAll,
        CollapseAll,
        FocusSearch,
        GoInbox,
        GoStarred,
        GoSent,
        GoDrafts,
        GoAllMail,
        NextAccount,
        PrevAccount,
        ToggleHelp,
        /// Closes the label picker.
        ClosePicker,
        /// Moves the label picker's highlight up.
        PickerUp,
        /// Moves the label picker's highlight down.
        PickerDown,
        /// Escape in the search field: clears it and focuses the list.
        CancelSearch,
        /// The search results' ✕: returns to the view the search replaced (not bound to a key).
        CloseSearch,
        OpenSettings,
        Refresh,
        CloseWindow,
        Quit,
    ]
);

/// The predicate every Mail binding uses: never while typing in a field.
pub const MAIL: &str = "Mail && !TextInput && !TextArea";

/// The Mail, LabelPicker, Search and Eloquawk key map of SPEC 9 (the field and body contexts
/// bind their own; enter in the search field and the picker's filter is the field's `Submit`).
/// Linux reports `#`, `!`, `?`, `:` without shift, so those are bound as bare characters.
pub fn bindings() -> Vec<KeyBinding> {
    let mail = Some(MAIL);
    let app = Some("Eloquawk");
    vec![
        KeyBinding::new("j", NextThread, mail),
        KeyBinding::new("down", NextThread, mail),
        KeyBinding::new("k", PrevThread, mail),
        KeyBinding::new("up", PrevThread, mail),
        KeyBinding::new("n", NextMessage, mail),
        KeyBinding::new("p", PrevMessage, mail),
        KeyBinding::new("o", OpenThread, mail),
        KeyBinding::new("enter", OpenThread, mail),
        KeyBinding::new("u", BackToList, mail),
        KeyBinding::new("escape", BackToList, mail),
        KeyBinding::new("space", ScrollReaderDown, mail),
        KeyBinding::new("pagedown", ScrollReaderDown, mail),
        KeyBinding::new("shift-space", ScrollReaderUp, mail),
        KeyBinding::new("pageup", ScrollReaderUp, mail),
        KeyBinding::new("x", ToggleSelect, mail),
        KeyBinding::new("e", Archive, mail),
        KeyBinding::new("#", Trash, mail),
        KeyBinding::new("s", ToggleStar, mail),
        KeyBinding::new("shift-i", MarkRead, mail),
        KeyBinding::new("shift-u", MarkUnread, mail),
        KeyBinding::new("!", ReportSpam, mail),
        KeyBinding::new("l", OpenLabelPicker, mail),
        KeyBinding::new("r", Reply, mail),
        KeyBinding::new("a", ReplyAll, mail),
        KeyBinding::new("f", Forward, mail),
        KeyBinding::new("c", Compose, mail),
        KeyBinding::new("z", Undo, mail),
        KeyBinding::new(";", ExpandAll, mail),
        KeyBinding::new(":", CollapseAll, mail),
        KeyBinding::new("/", FocusSearch, mail),
        KeyBinding::new("g i", GoInbox, mail),
        KeyBinding::new("g s", GoStarred, mail),
        KeyBinding::new("g t", GoSent, mail),
        KeyBinding::new("g d", GoDrafts, mail),
        KeyBinding::new("g a", GoAllMail, mail),
        KeyBinding::new("g n", NextAccount, mail),
        KeyBinding::new("g p", PrevAccount, mail),
        KeyBinding::new("?", ToggleHelp, mail),
        KeyBinding::new("escape", ClosePicker, Some("LabelPicker")),
        KeyBinding::new("up", PickerUp, Some("LabelPicker")),
        KeyBinding::new("down", PickerDown, Some("LabelPicker")),
        KeyBinding::new("escape", CancelSearch, Some("Search")),
        KeyBinding::new("ctrl-r", Refresh, app),
        KeyBinding::new("ctrl-,", OpenSettings, app),
        KeyBinding::new("ctrl-n", Compose, app),
        KeyBinding::new("ctrl-w", CloseWindow, app),
        // Context-free so it also quits from windows without the Eloquawk root.
        KeyBinding::new("ctrl-q", Quit, None),
    ]
}

pub fn bind_keys(cx: &mut App) {
    text_input::bind_keys(cx);
    body_text::bind_keys(cx);
    cx.bind_keys(bindings());
}
