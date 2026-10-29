//! `TextArea`: the multi-line, wrapping editor for compose bodies (SPEC 8.6), built on the
//! shaping and hit-testing of [`super::text_layout`].
//!
//! The editing state is a pure [`TextBuffer`] (text, selection, IME marked range, undo and
//! redo with typing runs coalesced), unit-tested without a window. The entity wraps it with a
//! focus handle, the `TextArea` key context, IME (`EntityInputHandler`, UTF-16 ranges) and
//! mouse selection. It takes whatever height its parent gives it and scrolls itself: it keeps
//! its own offset, paints inside a content mask, handles the scroll wheel and moves the offset
//! after every cursor move so the caret stays visible.
//!
//! On Linux `key_char` is `None` for control characters, so enter and tab never reach the
//! input handler and are bound explicitly. ctrl-enter is left unbound for the `Compose`
//! context, and the area never emits `Submit`.

use std::ops::Range;

use gpui::prelude::*;
use gpui::{
    App, Bounds, ClipboardItem, ContentMask, Context, CursorStyle, ElementId, ElementInputHandler,
    Entity, EntityInputHandler, FocusHandle, Focusable, GlobalElementId, Hitbox, HitboxBehavior,
    KeyBinding, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point,
    ScrollWheelEvent, SharedString, Style, TextRun, UTF16Selection, UnderlineStyle, Window,
    actions, div, fill, point, px, relative, rgb, size,
};
use unicode_segmentation::UnicodeSegmentation;

use super::text_layout::{TextLayout, normalise, word_at};
use super::theme::palette;

actions!(
    text_area,
    [
        Newline,
        Backspace,
        Delete,
        DeleteWordLeft,
        DeleteWordRight,
        Left,
        Right,
        Up,
        Down,
        WordLeft,
        WordRight,
        Home,
        End,
        DocStart,
        DocEnd,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        SelectWordLeft,
        SelectWordRight,
        SelectHome,
        SelectEnd,
        SelectDocStart,
        SelectDocEnd,
        SelectAll,
        Copy,
        Cut,
        Paste,
        Undo,
        Redo,
        FocusNext,
        FocusPrev,
    ]
);

pub fn bind_keys(cx: &mut App) {
    let ctx = Some("TextArea");
    cx.bind_keys([
        KeyBinding::new("enter", Newline, ctx),
        KeyBinding::new("shift-enter", Newline, ctx),
        KeyBinding::new("tab", FocusNext, ctx),
        KeyBinding::new("shift-tab", FocusPrev, ctx),
        KeyBinding::new("backspace", Backspace, ctx),
        KeyBinding::new("shift-backspace", Backspace, ctx),
        KeyBinding::new("delete", Delete, ctx),
        KeyBinding::new("ctrl-backspace", DeleteWordLeft, ctx),
        KeyBinding::new("ctrl-delete", DeleteWordRight, ctx),
        KeyBinding::new("left", Left, ctx),
        KeyBinding::new("right", Right, ctx),
        KeyBinding::new("up", Up, ctx),
        KeyBinding::new("down", Down, ctx),
        KeyBinding::new("ctrl-left", WordLeft, ctx),
        KeyBinding::new("ctrl-right", WordRight, ctx),
        KeyBinding::new("home", Home, ctx),
        KeyBinding::new("end", End, ctx),
        KeyBinding::new("ctrl-home", DocStart, ctx),
        KeyBinding::new("ctrl-end", DocEnd, ctx),
        KeyBinding::new("shift-left", SelectLeft, ctx),
        KeyBinding::new("shift-right", SelectRight, ctx),
        KeyBinding::new("shift-up", SelectUp, ctx),
        KeyBinding::new("shift-down", SelectDown, ctx),
        KeyBinding::new("ctrl-shift-left", SelectWordLeft, ctx),
        KeyBinding::new("ctrl-shift-right", SelectWordRight, ctx),
        KeyBinding::new("shift-home", SelectHome, ctx),
        KeyBinding::new("shift-end", SelectEnd, ctx),
        KeyBinding::new("ctrl-shift-home", SelectDocStart, ctx),
        KeyBinding::new("ctrl-shift-end", SelectDocEnd, ctx),
        KeyBinding::new("ctrl-a", SelectAll, ctx),
        KeyBinding::new("ctrl-c", Copy, ctx),
        KeyBinding::new("ctrl-insert", Copy, ctx),
        KeyBinding::new("ctrl-x", Cut, ctx),
        KeyBinding::new("shift-delete", Cut, ctx),
        KeyBinding::new("ctrl-v", Paste, ctx),
        KeyBinding::new("shift-insert", Paste, ctx),
        KeyBinding::new("ctrl-z", Undo, ctx),
        KeyBinding::new("ctrl-shift-z", Redo, ctx),
        KeyBinding::new("ctrl-y", Redo, ctx),
    ]);
}

// ----- pure buffer ---------------------------------------------------------------------------

/// What an edit was, for coalescing undo steps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditKind {
    /// Typed text: a run of these is one undo step.
    Typing,
    /// Backspace or delete: a run of these is one undo step.
    Deleting,
    /// Anything else (paste, cut, newline, IME commit over a selection): its own step.
    Other,
}

#[derive(Clone, Debug, PartialEq)]
struct Snapshot {
    text: String,
    selection: Range<usize>,
}

/// Text with a selection, an IME marked range and undo history. Offsets are UTF-8 bytes on
/// char boundaries; the selection is ascending, and `reversed` puts the caret at its start.
#[derive(Clone, Debug, Default)]
pub struct TextBuffer {
    text: String,
    selection: Range<usize>,
    reversed: bool,
    marked: Option<Range<usize>>,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    /// The kind of the last edit and where it left the caret, while a run can continue.
    run: Option<(EditKind, usize)>,
}

/// The most undo steps kept. Each step is a whole-text snapshot, so the cap bounds the
/// memory a long, much-edited body can hold.
pub const UNDO_LIMIT: usize = 200;

/// Normalises pasted line breaks: CRLF and lone CR become LF.
pub fn normalise_newlines(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n")
}

/// The start of every visual line: 0, after each newline, and each soft wrap (absolute byte
/// offsets, as `TextLayout::wrap_offsets` gives them), ascending without repeats.
pub fn line_starts(text: &str, wraps: &[usize]) -> Vec<usize> {
    let mut starts: Vec<usize> = std::iter::once(0)
        .chain(text.match_indices('\n').map(|(i, _)| i + 1))
        .chain(wraps.iter().copied().filter(|&w| w <= text.len()))
        .collect();
    starts.sort_unstable();
    starts.dedup();
    starts
}

/// The visual line holding `ix`: the last line starting at or before it.
pub fn visual_line(starts: &[usize], ix: usize) -> usize {
    starts.partition_point(|&s| s <= ix).saturating_sub(1)
}

/// The byte range of visual line `line`, without its trailing newline.
pub fn line_range(text: &str, starts: &[usize], line: usize) -> Range<usize> {
    let start = starts.get(line).copied().unwrap_or(text.len());
    let mut end = starts.get(line + 1).copied().unwrap_or(text.len());
    if end > start && text.as_bytes().get(end - 1) == Some(&b'\n') {
        end -= 1;
    }
    start..end.max(start)
}

/// `ix` as (visual line, column in chars).
pub fn line_col(text: &str, starts: &[usize], ix: usize) -> (usize, usize) {
    let line = visual_line(starts, ix);
    let r = line_range(text, starts, line);
    let col = text[r.start..ix.clamp(r.start, r.end)].chars().count();
    (line, col)
}

/// The offset at (line, column in chars), clamped to the line's end and the last line.
pub fn offset_at(text: &str, starts: &[usize], line: usize, col: usize) -> usize {
    let line = line.min(starts.len().saturating_sub(1));
    let r = line_range(text, starts, line);
    text[r.clone()]
        .char_indices()
        .nth(col)
        .map_or(r.end, |(i, _)| r.start + i)
}

impl TextBuffer {
    pub fn new(text: &str) -> Self {
        Self {
            text: normalise_newlines(text),
            ..Self::default()
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn selection(&self) -> Range<usize> {
        self.selection.clone()
    }

    pub fn marked(&self) -> Option<Range<usize>> {
        self.marked.clone()
    }

    /// The caret: the moving end of the selection.
    pub fn cursor(&self) -> usize {
        if self.reversed {
            self.selection.start
        } else {
            self.selection.end
        }
    }

    fn anchor(&self) -> usize {
        if self.reversed {
            self.selection.end
        } else {
            self.selection.start
        }
    }

    fn clamp(&self, ix: usize) -> usize {
        let mut ix = ix.min(self.text.len());
        while !self.text.is_char_boundary(ix) {
            ix -= 1;
        }
        ix
    }

    /// Ends the current typing run, so the next edit is a new undo step.
    pub fn break_run(&mut self) {
        self.run = None;
    }

    /// Puts the caret at `ix` with nothing selected.
    pub fn move_to(&mut self, ix: usize) {
        let ix = self.clamp(ix);
        self.selection = ix..ix;
        self.reversed = false;
        self.break_run();
    }

    /// Moves the caret to `ix`, keeping the anchor (shift-movement and drags).
    pub fn select_to(&mut self, ix: usize) {
        let ix = self.clamp(ix);
        let anchor = self.anchor();
        self.reversed = ix < anchor;
        self.selection = normalise(anchor, ix);
        self.break_run();
    }

    /// Selects `range` with the caret at its end.
    pub fn select_range(&mut self, range: Range<usize>) {
        let (a, b) = (self.clamp(range.start), self.clamp(range.end));
        self.selection = normalise(a, b);
        self.reversed = false;
        self.break_run();
    }

    pub fn select_all(&mut self) {
        self.select_range(0..self.text.len());
    }

    pub fn selected_text(&self) -> &str {
        &self.text[self.selection.clone()]
    }

    pub fn prev_grapheme(&self, ix: usize) -> usize {
        self.text
            .grapheme_indices(true)
            .rev()
            .find_map(|(i, _)| (i < ix).then_some(i))
            .unwrap_or(0)
    }

    pub fn next_grapheme(&self, ix: usize) -> usize {
        self.text
            .grapheme_indices(true)
            .find_map(|(i, _)| (i > ix).then_some(i))
            .unwrap_or(self.text.len())
    }

    /// The start of the word before `ix` (skipping whitespace and punctuation first).
    pub fn prev_word(&self, ix: usize) -> usize {
        self.text
            .split_word_bound_indices()
            .rev()
            .find(|(i, w)| *i < ix && w.chars().any(char::is_alphanumeric))
            .map_or(0, |(i, _)| i)
    }

    /// The end of the word after `ix` (skipping whitespace and punctuation first).
    pub fn next_word(&self, ix: usize) -> usize {
        self.text
            .split_word_bound_indices()
            .find(|(i, w)| i + w.len() > ix && w.chars().any(char::is_alphanumeric))
            .map_or(self.text.len(), |(i, w)| i + w.len())
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            text: self.text.clone(),
            selection: self.selection.clone(),
        }
    }

    /// Replaces `range` with `new`, leaving the caret after it. Consecutive typing (or
    /// deleting) at the caret coalesces into one undo step.
    pub fn replace(&mut self, range: Range<usize>, new: &str, kind: EditKind) {
        let range = self.clamp(range.start)..self.clamp(range.end);
        let range = normalise(range.start, range.end);
        let continues = kind != EditKind::Other
            && self
                .run
                .is_some_and(|(k, at)| k == kind && (at == range.start || at == range.end));
        if !continues {
            self.push_undo();
        }
        self.redo.clear();
        self.text.replace_range(range.clone(), new);
        let caret = range.start + new.len();
        self.selection = caret..caret;
        self.reversed = false;
        self.marked = None;
        self.run = (kind != EditKind::Other).then_some((kind, caret));
    }

    /// Replaces the selection (typing, paste).
    pub fn insert(&mut self, new: &str, kind: EditKind) {
        let kind = if self.selection.is_empty() {
            kind
        } else {
            EditKind::Other
        };
        self.replace(self.selection.clone(), new, kind);
    }

    /// Backspace: deletes the selection, else the grapheme (or word) before the caret.
    pub fn backspace(&mut self, word: bool) {
        if self.selection.is_empty() {
            let at = self.cursor();
            let from = if word {
                self.prev_word(at)
            } else {
                self.prev_grapheme(at)
            };
            if from < at {
                self.replace(from..at, "", EditKind::Deleting);
            }
        } else {
            self.replace(self.selection.clone(), "", EditKind::Other);
        }
    }

    /// Delete: deletes the selection, else the grapheme (or word) after the caret.
    pub fn delete(&mut self, word: bool) {
        if self.selection.is_empty() {
            let at = self.cursor();
            let to = if word {
                self.next_word(at)
            } else {
                self.next_grapheme(at)
            };
            if to > at {
                self.replace(at..to, "", EditKind::Deleting);
            }
        } else {
            self.replace(self.selection.clone(), "", EditKind::Other);
        }
    }

    /// Sets the IME's marked text over `range` (the selection when `None`), with `selected`
    /// relative to the new text.
    pub fn mark(&mut self, range: Option<Range<usize>>, new: &str, selected: Option<Range<usize>>) {
        let range = range
            .or_else(|| self.marked.clone())
            .unwrap_or_else(|| self.selection.clone());
        self.replace(range.clone(), new, EditKind::Typing);
        let start = range.start.min(self.text.len());
        self.marked = (!new.is_empty()).then(|| start..start + new.len());
        if let Some(sel) = selected {
            let s = self.clamp(start + sel.start.min(new.len()));
            let e = self.clamp(start + sel.end.min(new.len()));
            self.selection = normalise(s, e);
        }
    }

    pub fn unmark(&mut self) {
        self.marked = None;
    }

    /// Records the current state as an undo step, dropping the oldest past [`UNDO_LIMIT`].
    fn push_undo(&mut self) {
        if self.undo.len() >= UNDO_LIMIT {
            self.undo.remove(0);
        }
        self.undo.push(self.snapshot());
    }

    pub fn undo(&mut self) -> bool {
        let Some(prev) = self.undo.pop() else {
            return false;
        };
        self.redo.push(self.snapshot());
        self.restore(prev);
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(next) = self.redo.pop() else {
            return false;
        };
        self.push_undo();
        self.restore(next);
        true
    }

    fn restore(&mut self, s: Snapshot) {
        self.text = s.text;
        self.selection = self.clamp(s.selection.start)..self.clamp(s.selection.end);
        self.reversed = false;
        self.marked = None;
        self.run = None;
    }

    // UTF-16 conversions for the input handler.

    pub fn offset_to_utf16(&self, ix: usize) -> usize {
        self.text[..self.clamp(ix)].encode_utf16().count()
    }

    pub fn offset_from_utf16(&self, ix16: usize) -> usize {
        let mut n16 = 0;
        for (i, ch) in self.text.char_indices() {
            if n16 >= ix16 {
                return i;
            }
            n16 += ch.len_utf16();
        }
        self.text.len()
    }

    pub fn range_to_utf16(&self, r: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(r.start)..self.offset_to_utf16(r.end)
    }

    pub fn range_from_utf16(&self, r: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(r.start)..self.offset_from_utf16(r.end)
    }
}

// ----- entity -------------------------------------------------------------------------------

pub struct TextArea {
    focus: FocusHandle,
    buffer: TextBuffer,
    placeholder: SharedString,
    layout: TextLayout,
    /// How far the content is scrolled up, in pixels.
    scroll: Pixels,
    /// The caret moved: bring it into view at the next layout.
    autoscroll: bool,
    /// The x position up and down aim for, kept across a run of vertical moves.
    goal_x: Option<Pixels>,
    selecting: bool,
    /// Where the area was last painted (its viewport), in window coordinates.
    viewport: Option<Bounds<Pixels>>,
}

impl TextArea {
    pub fn new(
        text: &str,
        placeholder: impl Into<SharedString>,
        tab_index: isize,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            focus: cx.focus_handle().tab_index(tab_index).tab_stop(true),
            buffer: TextBuffer::new(text),
            placeholder: placeholder.into(),
            layout: TextLayout::default(),
            scroll: px(0.0),
            autoscroll: true,
            goal_x: None,
            selecting: false,
            viewport: None,
        }
    }

    pub fn text(&self) -> &str {
        self.buffer.text()
    }

    fn changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.autoscroll = true;
        window.notify_text_input_changed(&self.focus);
        cx.notify();
    }

    fn moved(&mut self, keep_goal: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !keep_goal {
            self.goal_x = None;
        }
        self.autoscroll = true;
        window.notify_text_selection_changed(&self.focus);
        cx.notify();
    }

    fn starts(&self) -> Vec<usize> {
        line_starts(self.buffer.text(), &self.layout.wrap_offsets())
    }

    fn home_of(&self, ix: usize) -> usize {
        let starts = self.starts();
        line_range(self.buffer.text(), &starts, visual_line(&starts, ix)).start
    }

    fn end_of(&self, ix: usize) -> usize {
        let starts = self.starts();
        let line = visual_line(&starts, ix);
        let r = line_range(self.buffer.text(), &starts, line);
        // A soft-wrapped line ends before the space it wrapped at, so the caret stays on it.
        let wrapped = starts.get(line + 1) == Some(&r.end);
        if wrapped && self.buffer.text()[..r.end].ends_with(char::is_whitespace) && r.end > r.start
        {
            self.buffer.prev_grapheme(r.end)
        } else {
            r.end
        }
    }

    /// The offset one visual line up or down from the caret, aiming for `goal_x`.
    fn vertical(&mut self, down: bool) -> usize {
        let at = self.buffer.cursor();
        let lh = self.layout.line_height();
        match self.layout.position_for_index(at) {
            Some(p) if lh > px(0.0) => {
                let x = *self.goal_x.get_or_insert(p.x);
                let y = if down { p.y + lh * 1.5 } else { p.y - lh * 0.5 };
                self.layout.closest_index(point(x, y)).unwrap_or(at)
            }
            // Not laid out yet: by characters.
            _ => {
                let text = self.buffer.text();
                let starts = line_starts(text, &[]);
                let (line, col) = line_col(text, &starts, at);
                let line = if down {
                    line + 1
                } else if line == 0 {
                    return 0;
                } else {
                    line - 1
                };
                if line >= starts.len() {
                    return text.len();
                }
                offset_at(text, &starts, line, col)
            }
        }
    }

    // ----- actions ----------------------------------------------------------------------

    fn newline(&mut self, _: &Newline, window: &mut Window, cx: &mut Context<Self>) {
        self.buffer.insert("\n", EditKind::Other);
        self.changed(window, cx);
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        self.buffer.backspace(false);
        self.changed(window, cx);
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        self.buffer.delete(false);
        self.changed(window, cx);
    }

    fn delete_word_left(
        &mut self,
        _: &DeleteWordLeft,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.buffer.backspace(true);
        self.changed(window, cx);
    }

    fn delete_word_right(
        &mut self,
        _: &DeleteWordRight,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.buffer.delete(true);
        self.changed(window, cx);
    }

    fn go(
        &mut self,
        ix: usize,
        extend: bool,
        keep_goal: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if extend {
            self.buffer.select_to(ix);
        } else {
            self.buffer.move_to(ix);
        }
        self.moved(keep_goal, window, cx);
    }

    fn left(&mut self, _: &Left, window: &mut Window, cx: &mut Context<Self>) {
        let sel = self.buffer.selection();
        let ix = if sel.is_empty() {
            self.buffer.prev_grapheme(sel.start)
        } else {
            sel.start
        };
        self.go(ix, false, false, window, cx);
    }

    fn right(&mut self, _: &Right, window: &mut Window, cx: &mut Context<Self>) {
        let sel = self.buffer.selection();
        let ix = if sel.is_empty() {
            self.buffer.next_grapheme(sel.end)
        } else {
            sel.end
        };
        self.go(ix, false, false, window, cx);
    }

    fn up(&mut self, _: &Up, window: &mut Window, cx: &mut Context<Self>) {
        let ix = self.vertical(false);
        self.go(ix, false, true, window, cx);
    }

    fn down(&mut self, _: &Down, window: &mut Window, cx: &mut Context<Self>) {
        let ix = self.vertical(true);
        self.go(ix, false, true, window, cx);
    }

    fn word_left(&mut self, _: &WordLeft, window: &mut Window, cx: &mut Context<Self>) {
        let ix = self.buffer.prev_word(self.buffer.cursor());
        self.go(ix, false, false, window, cx);
    }

    fn word_right(&mut self, _: &WordRight, window: &mut Window, cx: &mut Context<Self>) {
        let ix = self.buffer.next_word(self.buffer.cursor());
        self.go(ix, false, false, window, cx);
    }

    fn home(&mut self, _: &Home, window: &mut Window, cx: &mut Context<Self>) {
        let ix = self.home_of(self.buffer.cursor());
        self.go(ix, false, false, window, cx);
    }

    fn end(&mut self, _: &End, window: &mut Window, cx: &mut Context<Self>) {
        let ix = self.end_of(self.buffer.cursor());
        self.go(ix, false, false, window, cx);
    }

    fn doc_start(&mut self, _: &DocStart, window: &mut Window, cx: &mut Context<Self>) {
        self.go(0, false, false, window, cx);
    }

    fn doc_end(&mut self, _: &DocEnd, window: &mut Window, cx: &mut Context<Self>) {
        let end = self.buffer.text().len();
        self.go(end, false, false, window, cx);
    }

    fn select_left(&mut self, _: &SelectLeft, window: &mut Window, cx: &mut Context<Self>) {
        let ix = self.buffer.prev_grapheme(self.buffer.cursor());
        self.go(ix, true, false, window, cx);
    }

    fn select_right(&mut self, _: &SelectRight, window: &mut Window, cx: &mut Context<Self>) {
        let ix = self.buffer.next_grapheme(self.buffer.cursor());
        self.go(ix, true, false, window, cx);
    }

    fn select_up(&mut self, _: &SelectUp, window: &mut Window, cx: &mut Context<Self>) {
        let ix = self.vertical(false);
        self.go(ix, true, true, window, cx);
    }

    fn select_down(&mut self, _: &SelectDown, window: &mut Window, cx: &mut Context<Self>) {
        let ix = self.vertical(true);
        self.go(ix, true, true, window, cx);
    }

    fn select_word_left(
        &mut self,
        _: &SelectWordLeft,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ix = self.buffer.prev_word(self.buffer.cursor());
        self.go(ix, true, false, window, cx);
    }

    fn select_word_right(
        &mut self,
        _: &SelectWordRight,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ix = self.buffer.next_word(self.buffer.cursor());
        self.go(ix, true, false, window, cx);
    }

    fn select_home(&mut self, _: &SelectHome, window: &mut Window, cx: &mut Context<Self>) {
        let ix = self.home_of(self.buffer.cursor());
        self.go(ix, true, false, window, cx);
    }

    fn select_end(&mut self, _: &SelectEnd, window: &mut Window, cx: &mut Context<Self>) {
        let ix = self.end_of(self.buffer.cursor());
        self.go(ix, true, false, window, cx);
    }

    fn select_doc_start(
        &mut self,
        _: &SelectDocStart,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.go(0, true, false, window, cx);
    }

    fn select_doc_end(&mut self, _: &SelectDocEnd, window: &mut Window, cx: &mut Context<Self>) {
        let end = self.buffer.text().len();
        self.go(end, true, false, window, cx);
    }

    fn select_all(&mut self, _: &SelectAll, window: &mut Window, cx: &mut Context<Self>) {
        self.buffer.select_all();
        self.moved(false, window, cx);
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        let s = self.buffer.selected_text();
        if !s.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(s.to_string()));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        let s = self.buffer.selected_text().to_string();
        if !s.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(s));
            self.buffer.insert("", EditKind::Other);
            self.changed(window, cx);
        }
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            // Newlines are kept; only the line-break style is normalised.
            self.buffer
                .insert(&normalise_newlines(&text), EditKind::Other);
            self.changed(window, cx);
        }
    }

    fn undo(&mut self, _: &Undo, window: &mut Window, cx: &mut Context<Self>) {
        if self.buffer.undo() {
            self.changed(window, cx);
        }
    }

    fn redo(&mut self, _: &Redo, window: &mut Window, cx: &mut Context<Self>) {
        if self.buffer.redo() {
            self.changed(window, cx);
        }
    }

    fn focus_next(&mut self, _: &FocusNext, window: &mut Window, cx: &mut Context<Self>) {
        window.focus_next(cx);
    }

    fn focus_prev(&mut self, _: &FocusPrev, window: &mut Window, cx: &mut Context<Self>) {
        window.focus_prev(cx);
    }

    // ----- mouse ------------------------------------------------------------------------

    fn on_mouse_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
        let Some(ix) = self.layout.closest_index(ev.position) else {
            return;
        };
        if ev.click_count == 2 {
            self.buffer.select_range(word_at(self.buffer.text(), ix));
        } else if ev.modifiers.shift {
            self.buffer.select_to(ix);
            self.selecting = true;
        } else {
            self.buffer.move_to(ix);
            self.selecting = true;
        }
        self.moved(false, window, cx);
    }

    fn on_mouse_move(&mut self, ev: &MouseMoveEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.selecting || !ev.dragging() {
            return;
        }
        if let Some(ix) = self.layout.closest_index(ev.position)
            && ix != self.buffer.cursor()
        {
            self.buffer.select_to(ix);
            self.moved(false, window, cx);
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.selecting = false;
    }

    fn on_scroll(&mut self, ev: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let lh = self.layout.line_height().max(px(16.0));
        let delta = ev.delta.pixel_delta(lh);
        self.scroll = (self.scroll - delta.y).max(px(0.0));
        self.autoscroll = false;
        cx.notify();
    }

    /// Clamps the scroll offset to the content and, after a caret move, scrolls the caret
    /// into the viewport with the least movement. Called from prepaint with fresh shaping.
    fn settle_scroll(&mut self, viewport: Bounds<Pixels>, content_height: Pixels) {
        let max = (content_height - viewport.size.height).max(px(0.0));
        if self.autoscroll {
            self.autoscroll = false;
            let lh = self.layout.line_height();
            self.layout.set_bounds(Bounds::new(
                point(viewport.left(), viewport.top() - self.scroll),
                size(viewport.size.width, content_height),
            ));
            if let Some(p) = self.layout.position_for_index(self.buffer.cursor()) {
                let top = p.y - viewport.top();
                if top < px(0.0) {
                    self.scroll += top;
                } else if top + lh > viewport.size.height {
                    self.scroll += top + lh - viewport.size.height;
                }
            }
        }
        self.scroll = self.scroll.clamp(px(0.0), max);
    }
}

impl EntityInputHandler for TextArea {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.buffer.range_from_utf16(&range_utf16);
        actual_range.replace(self.buffer.range_to_utf16(&range));
        self.buffer.text().get(range).map(str::to_string)
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.buffer.range_to_utf16(&self.buffer.selection()),
            reversed: self.buffer.reversed,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.buffer.marked().map(|r| self.buffer.range_to_utf16(&r))
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.buffer.unmark();
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .map(|r| self.buffer.range_from_utf16(&r))
            .or_else(|| self.buffer.marked())
            .unwrap_or_else(|| self.buffer.selection());
        let kind = if range.is_empty() {
            EditKind::Typing
        } else {
            EditKind::Other
        };
        self.buffer.replace(range, &normalise_newlines(text), kind);
        self.goal_x = None;
        self.changed(window, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16.map(|r| self.buffer.range_from_utf16(&r));
        // The selected range is in UTF-16 units of the new text.
        let selected = new_selected_range_utf16.map(|r| {
            let to8 = |n16: usize| {
                let mut n = 0;
                for (i, ch) in new_text.char_indices() {
                    if n >= n16 {
                        return i;
                    }
                    n += ch.len_utf16();
                }
                new_text.len()
            };
            to8(r.start)..to8(r.end)
        });
        self.buffer.mark(range, new_text, selected);
        self.changed(window, cx);
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        _element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let range = self.buffer.range_from_utf16(&range_utf16);
        let lh = self.layout.line_height();
        let start = self.layout.position_for_index(range.start)?;
        let end = self.layout.position_for_index(range.end).unwrap_or(start);
        let end_x = if end.y == start.y { end.x } else { start.x };
        Some(Bounds::from_corners(start, point(end_x, start.y + lh)))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let ix = self.layout.closest_index(point)?;
        Some(self.buffer.offset_to_utf16(ix))
    }
}

// ----- element ------------------------------------------------------------------------------

/// Paints the area's text, selection and caret inside its own bounds, scrolled.
struct TextAreaElement {
    area: Entity<TextArea>,
}

struct Prepainted {
    hitbox: Hitbox,
    selection: Vec<Bounds<Pixels>>,
    caret: Option<Bounds<Pixels>>,
    origin: Point<Pixels>,
    placeholder: Option<gpui::ShapedLine>,
}

impl IntoElement for TextAreaElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextAreaElement {
    type RequestLayoutState = ();
    type PrepaintState = Prepainted;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line_height = window.line_height();
        let base = style.to_run(0);
        let pal = palette(cx);
        let (text, runs, layout) = {
            let area = self.area.read(cx);
            let text: SharedString = area.buffer.text().to_string().into();
            let runs: Vec<TextRun> = match area.buffer.marked() {
                Some(m) if m.end <= text.len() => [
                    (0..m.start, false),
                    (m.clone(), true),
                    (m.end..text.len(), false),
                ]
                .into_iter()
                .filter(|(r, _)| !r.is_empty())
                .map(|(r, marked)| TextRun {
                    len: r.len(),
                    underline: marked.then_some(UnderlineStyle {
                        color: Some(base.color),
                        thickness: px(1.0),
                        wavy: false,
                    }),
                    ..base.clone()
                })
                .collect(),
                _ => vec![TextRun {
                    len: text.len(),
                    ..base.clone()
                }],
            };
            (text, runs, area.layout.clone())
        };
        layout.set_signature(text.clone(), runs.clone(), font_size, line_height);
        let content = layout.shape(text.clone(), &runs, Some(bounds.size.width), window);
        let (origin, selection, caret) = self.area.update(cx, |area, _| {
            area.viewport = Some(bounds);
            area.settle_scroll(bounds, content.height);
            let origin = point(bounds.left(), bounds.top() - area.scroll);
            layout.set_bounds(Bounds::new(origin, size(bounds.size.width, content.height)));
            let sel = area.buffer.selection();
            let selection = layout.rects(sel.clone(), true);
            let caret = sel.is_empty().then(|| {
                let p = layout
                    .position_for_index(area.buffer.cursor())
                    .unwrap_or(origin);
                Bounds::new(p, size(px(1.5), line_height))
            });
            (origin, selection, caret)
        });
        let placeholder = if text.is_empty() {
            let ph = self.area.read(cx).placeholder.clone();
            let run = TextRun {
                len: ph.len(),
                color: rgb(pal.disabled).into(),
                ..base
            };
            Some(window.text_system().shape_line(ph, font_size, &[run], None))
        } else {
            None
        };
        Prepainted {
            hitbox: window.insert_hitbox(bounds, HitboxBehavior::Normal),
            selection,
            caret,
            origin,
            placeholder,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let (focus, layout) = {
            let area = self.area.read(cx);
            (area.focus.clone(), area.layout.clone())
        };
        window.handle_input(
            &focus,
            ElementInputHandler::new(bounds, self.area.clone()),
            cx,
        );
        window.set_cursor_style(CursorStyle::IBeam, &prepaint.hitbox);
        let pal = palette(cx);
        let lh = window.line_height();
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            for rect in &prepaint.selection {
                window.paint_quad(fill(*rect, rgb(pal.selection)));
            }
            if let Some(line) = prepaint.placeholder.take()
                && let Err(e) =
                    line.paint(prepaint.origin, lh, gpui::TextAlign::Left, None, window, cx)
            {
                eprintln!("eloquawk: could not paint a placeholder: {e}");
            }
            layout.paint(
                prepaint.origin,
                Some(Bounds::new(prepaint.origin, bounds.size)),
                window,
                cx,
            );
            if focus.is_focused(window)
                && let Some(caret) = prepaint.caret
            {
                window.paint_quad(fill(caret, rgb(pal.accent)));
            }
        });
    }
}

impl Render for TextArea {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        div()
            .id("text-area")
            .key_context("TextArea")
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::newline))
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::delete_word_left))
            .on_action(cx.listener(Self::delete_word_right))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::word_left))
            .on_action(cx.listener(Self::word_right))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::doc_start))
            .on_action(cx.listener(Self::doc_end))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_up))
            .on_action(cx.listener(Self::select_down))
            .on_action(cx.listener(Self::select_word_left))
            .on_action(cx.listener(Self::select_word_right))
            .on_action(cx.listener(Self::select_home))
            .on_action(cx.listener(Self::select_end))
            .on_action(cx.listener(Self::select_doc_start))
            .on_action(cx.listener(Self::select_doc_end))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::redo))
            .on_action(cx.listener(Self::focus_next))
            .on_action(cx.listener(Self::focus_prev))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .size_full()
            .overflow_hidden()
            .text_size(px(13.5))
            .line_height(px(20.0))
            .text_color(rgb(p.text))
            .child(TextAreaElement { area: cx.entity() })
    }
}

impl Focusable for TextArea {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(b: &mut TextBuffer, s: &str) {
        for ch in s.chars() {
            b.insert(&ch.to_string(), EditKind::Typing);
        }
    }

    #[test]
    fn insert_replaces_the_selection() {
        let mut b = TextBuffer::new("hello world");
        b.move_to(5);
        b.insert(",", EditKind::Typing);
        assert_eq!(b.text(), "hello, world");
        assert_eq!(b.cursor(), 6);
        b.select_range(7..12);
        b.insert("Zoë", EditKind::Typing);
        assert_eq!(b.text(), "hello, Zoë");
        assert_eq!(b.selection(), 11..11);
    }

    #[test]
    fn backspace_and_delete_by_grapheme_and_word() {
        let mut b = TextBuffer::new("café e\u{301} done");
        b.move_to(b.text().len());
        b.backspace(true);
        assert_eq!(b.text(), "café e\u{301} ");
        b.backspace(false);
        b.backspace(false);
        // The combining accent goes with its letter.
        assert_eq!(b.text(), "café ");
        b.move_to(0);
        b.delete(false);
        assert_eq!(b.text(), "afé ");
        b.delete(true);
        assert_eq!(b.text(), " ");
        // At the edges nothing happens.
        b.move_to(0);
        b.backspace(false);
        assert_eq!(b.text(), " ");
    }

    #[test]
    fn word_boundaries_skip_punctuation_and_space() {
        let b = TextBuffer::new("Hi, Zoë Ångström! ok");
        assert_eq!(b.prev_word(b.text().len()), b.text().find("ok").unwrap());
        let ang = b.text().find("Ångström").unwrap();
        assert_eq!(b.prev_word(ang + 3), ang);
        assert_eq!(b.prev_word(ang), b.text().find("Zoë").unwrap());
        assert_eq!(b.next_word(0), 2);
        assert_eq!(b.next_word(2), b.text().find("Zoë").unwrap() + "Zoë".len());
        assert_eq!(b.next_word(b.text().len()), b.text().len());
        assert_eq!(b.prev_word(0), 0);
    }

    #[test]
    fn undo_history_is_capped() {
        let mut b = TextBuffer::new("");
        for i in 0..UNDO_LIMIT + 50 {
            let end = b.text().len();
            b.replace(
                end..end,
                if i % 2 == 0 { "\n" } else { "x" },
                EditKind::Other,
            );
        }
        let mut steps = 0;
        while b.undo() {
            steps += 1;
        }
        assert_eq!(steps, UNDO_LIMIT);
        // The oldest steps were dropped, so the text cannot go all the way back.
        assert_eq!(b.text().len(), 50);
    }

    #[test]
    fn typing_runs_coalesce_into_one_undo_step() {
        let mut b = TextBuffer::new("");
        typed(&mut b, "Dear Zoë");
        b.insert("\n", EditKind::Other);
        typed(&mut b, "Thanks");
        assert_eq!(b.text(), "Dear Zoë\nThanks");
        assert!(b.undo());
        assert_eq!(b.text(), "Dear Zoë\n");
        assert!(b.undo());
        assert_eq!(b.text(), "Dear Zoë");
        assert!(b.undo());
        assert_eq!(b.text(), "");
        assert!(!b.undo());
        assert!(b.redo());
        assert!(b.redo());
        assert!(b.redo());
        assert_eq!(b.text(), "Dear Zoë\nThanks");
        assert!(!b.redo());

        // A caret move ends the run.
        let mut b = TextBuffer::new("");
        typed(&mut b, "ab");
        b.move_to(1);
        typed(&mut b, "X");
        assert_eq!(b.text(), "aXb");
        b.undo();
        assert_eq!(b.text(), "ab");

        // Backspaces coalesce too, and a new edit clears redo.
        let mut b = TextBuffer::new("");
        typed(&mut b, "abcd");
        b.backspace(false);
        b.backspace(false);
        assert_eq!(b.text(), "ab");
        b.undo();
        assert_eq!(b.text(), "abcd");
        b.insert("!", EditKind::Other);
        assert!(!b.redo());
    }

    #[test]
    fn paste_keeps_newlines_and_normalises_crlf() {
        assert_eq!(normalise_newlines("a\r\nb\rc\nd"), "a\nb\nc\nd");
        let mut b = TextBuffer::new("x");
        b.move_to(1);
        b.insert(&normalise_newlines("1\r\n2\r\n"), EditKind::Other);
        assert_eq!(b.text(), "x1\n2\n");
        b.undo();
        assert_eq!(b.text(), "x");
    }

    #[test]
    fn selection_follows_the_caret_both_ways() {
        let mut b = TextBuffer::new("abcdef");
        b.move_to(3);
        b.select_to(5);
        assert_eq!((b.selection(), b.cursor()), (3..5, 5));
        b.select_to(1);
        assert_eq!((b.selection(), b.cursor()), (1..3, 1));
        assert_eq!(b.selected_text(), "bc");
        b.select_all();
        assert_eq!(b.selection(), 0..6);
        // Offsets inside a char are pulled back to its start.
        let mut b = TextBuffer::new("Zoë");
        b.move_to(3);
        assert_eq!(b.cursor(), 2);
    }

    #[test]
    fn ime_marked_text_replaces_itself() {
        let mut b = TextBuffer::new("ab");
        b.move_to(1);
        b.mark(None, "ㅎ", Some(1..1));
        assert_eq!(b.text(), "aㅎb");
        assert_eq!(b.marked(), Some(1..4));
        b.mark(None, "한", None);
        assert_eq!(b.text(), "a한b");
        // Commit through the marked range.
        let marked = b.marked().unwrap();
        b.replace(marked, "한", EditKind::Typing);
        assert_eq!(b.text(), "a한b");
        assert_eq!(b.marked(), None);
        assert_eq!(b.offset_to_utf16(b.text().len()), 3);
        assert_eq!(b.offset_from_utf16(2), 4);
        // Astral chars are two UTF-16 units.
        let b = TextBuffer::new("a😀b");
        assert_eq!(b.offset_to_utf16(5), 3);
        assert_eq!(b.offset_from_utf16(3), 5);
    }

    #[test]
    fn line_and_column_over_wrap_boundaries() {
        // "hello world again\nnext", soft-wrapped after "hello " and after "world ".
        let text = "hello world again\nnext";
        let wraps = [6, 12];
        let starts = line_starts(text, &wraps);
        assert_eq!(starts, vec![0, 6, 12, 18]);
        assert_eq!(visual_line(&starts, 0), 0);
        assert_eq!(visual_line(&starts, 5), 0);
        // At a wrap boundary the caret is on the next line.
        assert_eq!(visual_line(&starts, 6), 1);
        assert_eq!(visual_line(&starts, 17), 2);
        assert_eq!(visual_line(&starts, 18), 3);
        assert_eq!(line_range(text, &starts, 2), 12..17);
        assert_eq!(line_range(text, &starts, 3), 18..22);
        assert_eq!(line_col(text, &starts, 8), (1, 2));
        assert_eq!(offset_at(text, &starts, 2, 2), 14);
        // A short line clamps the column to its end; past the last line clamps too.
        assert_eq!(offset_at(text, &starts, 3, 40), 22);
        assert_eq!(offset_at(text, &starts, 9, 1), 19);
        // Columns count chars, not bytes.
        let t = "Zoë!\nab";
        let s = line_starts(t, &[]);
        assert_eq!(line_col(t, &s, 4), (0, 3));
        assert_eq!(offset_at(t, &s, 0, 3), 4);
        // A trailing newline starts an empty last line.
        assert_eq!(line_starts("a\n", &[]), vec![0, 2]);
        assert_eq!(line_range("a\n", &[0, 2], 1), 2..2);
    }
}
