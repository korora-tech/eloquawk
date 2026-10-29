//! The label picker (SPEC 9, `l`): a popover listing the account's user labels with a filter
//! field. Up and down move the highlight, enter (the filter's `Submit`) toggles the
//! highlighted label on the targets, and escape closes it. A toggle is an optimistic,
//! undoable `Modify` through [`Store::set_label`], so the sidebar counts follow at once.
//!
//! The picker acts on the threads it was opened for (the selection, open thread or cursor row
//! for `l`; the open thread for the reader's "Labels…" button), fixed when it opens. Its key
//! context is `LabelPicker`, rendered outside the `Mail` container, and the filter is a
//! `TextInput`, so typing `e` there filters and never archives.

use std::collections::BTreeSet;

use gpui::prelude::*;
use gpui::{
    App, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    ScrollHandle, SharedString, Subscription, Window, div, px, rgb,
};

use super::text_input::{Submit, TextInput};
use super::theme::{Palette, palette, tint};
use super::{ClosePicker, PickerDown, PickerUp};
use crate::model::{Label, LabelKind, ThreadKey};
use crate::store::Store;

/// How many of the targets carry a label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    All,
    Some,
    None,
}

/// One row of the picker.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub id: String,
    /// The full name, nested names included ("Projects/Eloquawk").
    pub name: String,
    pub color: Option<(u32, u32)>,
    pub mark: Mark,
}

impl Entry {
    /// What a toggle does: add the label unless every target already has it.
    pub fn adds(&self) -> bool {
        self.mark != Mark::All
    }
}

/// The picker's rows: the user labels (hidden ones too, since hiding only affects the
/// sidebar) whose name contains `filter` ignoring case, sorted by name, each marked by how
/// many of `targets` (their label id sets) carry it.
pub fn entries(labels: &[Label], targets: &[&BTreeSet<String>], filter: &str) -> Vec<Entry> {
    let filter = filter.trim().to_lowercase();
    let mut out: Vec<Entry> = labels
        .iter()
        .filter(|l| l.kind == LabelKind::User)
        .filter(|l| filter.is_empty() || l.name.to_lowercase().contains(&filter))
        .map(|l| {
            let with = targets.iter().filter(|t| t.contains(&l.id)).count();
            let mark = match with {
                0 => Mark::None,
                n if n == targets.len() => Mark::All,
                _ => Mark::Some,
            };
            Entry {
                id: l.id.clone(),
                name: l.name.clone(),
                color: l.color,
                mark,
            }
        })
        .collect();
    out.sort_by_key(|e| e.name.to_lowercase());
    out
}

/// Moves a highlight by `delta` within `len` rows, clamped.
pub fn step(highlight: usize, delta: isize, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    (highlight as isize + delta).clamp(0, len as isize - 1) as usize
}

/// Asks the main view to close the picker.
pub enum PickerEvent {
    Close,
}

pub struct LabelPicker {
    store: Entity<Store>,
    targets: Vec<ThreadKey>,
    filter: Entity<TextInput>,
    highlight: usize,
    /// The filter text the highlight was last reset for.
    last_filter: String,
    /// The rows' scroll, kept on the highlighted row.
    scroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<PickerEvent> for LabelPicker {}

impl LabelPicker {
    pub fn new(store: Entity<Store>, targets: Vec<ThreadKey>, cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| TextInput::new("Filter labels", false, 0, cx));
        let subscriptions = vec![
            cx.observe(&filter, |_, _, cx| cx.notify()),
            cx.observe(&store, |_, _, cx| cx.notify()),
        ];
        Self {
            store,
            targets,
            filter,
            highlight: 0,
            last_filter: String::new(),
            scroll: ScrollHandle::new(),
            _subscriptions: subscriptions,
        }
    }

    /// The filter field, which takes the focus when the picker opens.
    pub fn filter_focus(&self, cx: &App) -> FocusHandle {
        self.filter.focus_handle(cx)
    }

    pub fn filter_text(&self, cx: &App) -> String {
        self.filter.read(cx).text().to_string()
    }

    /// The rows for the current filter and targets.
    pub fn rows(&self, cx: &App) -> Vec<Entry> {
        let store = self.store.read(cx);
        let Some(account) = self.targets.first().map(|k| k.account.clone()) else {
            return Vec::new();
        };
        let labels = store.labels.get(&account).map(Vec::as_slice).unwrap_or(&[]);
        let sets: Vec<&BTreeSet<String>> = self
            .targets
            .iter()
            .filter_map(|k| store.threads.get(k).map(|s| &s.label_ids))
            .collect();
        entries(labels, &sets, self.filter.read(cx).text())
    }

    /// Resets the highlight when the filter changed, and keeps it within the rows.
    fn sync_highlight(&mut self, len: usize, cx: &App) {
        let text = self.filter.read(cx).text();
        if text != self.last_filter {
            self.last_filter = text.to_string();
            self.highlight = 0;
            self.scroll.scroll_to_item(0);
        }
        self.highlight = self.highlight.min(len.saturating_sub(1));
    }

    /// Moves the highlight and scrolls the rows just enough to show it, so enter never
    /// toggles a label out of sight.
    fn move_highlight(&mut self, delta: isize, cx: &mut Context<Self>) {
        let len = self.rows(cx).len();
        self.sync_highlight(len, cx);
        self.highlight = step(self.highlight, delta, len);
        self.scroll.scroll_to_item(self.highlight);
        cx.notify();
    }

    fn up(&mut self, _: &PickerUp, _: &mut Window, cx: &mut Context<Self>) {
        self.move_highlight(-1, cx);
    }

    fn down(&mut self, _: &PickerDown, _: &mut Window, cx: &mut Context<Self>) {
        self.move_highlight(1, cx);
    }

    fn close(&mut self, _: &ClosePicker, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(PickerEvent::Close);
    }

    /// Enter in the filter: toggles the highlighted label.
    fn submit(&mut self, _: &Submit, _: &mut Window, cx: &mut Context<Self>) {
        let rows = self.rows(cx);
        self.sync_highlight(rows.len(), cx);
        if let Some(entry) = rows.get(self.highlight) {
            self.toggle(entry, cx);
        }
    }

    fn toggle(&mut self, entry: &Entry, cx: &mut Context<Self>) {
        let targets = self.targets.clone();
        let (id, on) = (entry.id.clone(), entry.adds());
        self.store.update(cx, |s, cx| {
            s.set_label(targets, &id, on);
            cx.notify();
        });
    }

    fn row(&self, p: &Palette, ix: usize, entry: &Entry, cx: &Context<Self>) -> impl IntoElement {
        let highlighted = ix == self.highlight;
        let entry_for_click = entry.clone();
        let (box_border, box_bg) = match entry.mark {
            Mark::None => (p.input_border, None),
            Mark::All | Mark::Some => (p.accent, Some(p.accent)),
        };
        let tick = match entry.mark {
            Mark::All => Some("✓"),
            Mark::Some => Some("–"),
            Mark::None => None,
        };
        let hover = p.hover;
        div()
            .id(SharedString::from(format!("picker-{}", entry.id)))
            .h(px(30.0))
            .flex()
            .items_center()
            .gap(px(10.0))
            .px(px(10.0))
            .rounded(px(5.0))
            .when(highlighted, |d| d.bg(rgb(p.selected)))
            .hover(move |s| s.bg(rgb(hover)))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.highlight = ix;
                this.toggle(&entry_for_click, cx);
            }))
            .child(
                div()
                    .size(px(14.0))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(3.0))
                    .border_1()
                    .border_color(rgb(box_border))
                    .when_some(box_bg, |d, bg| d.bg(rgb(bg)))
                    .children(tick.map(|t| {
                        div()
                            .text_size(px(10.0))
                            .line_height(px(12.0))
                            .font_weight(FontWeight::BOLD)
                            .text_color(rgb(p.on_accent))
                            .child(t)
                    })),
            )
            .child(
                div()
                    .size(px(8.0))
                    .flex_shrink_0()
                    .rounded_full()
                    .bg(rgb(entry
                        .color
                        .map_or(tint(p.muted, p.panel, 0.6), |(_, bg)| bg))),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .truncate()
                    .text_size(px(13.0))
                    .line_height(px(18.0))
                    .text_color(rgb(p.text))
                    .child(entry.name.clone()),
            )
    }
}

impl Render for LabelPicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let rows = self.rows(cx);
        self.sync_highlight(rows.len(), cx);
        let n = self.targets.len();
        let title = if n == 1 {
            "Label conversation".to_string()
        } else {
            format!("Label {n} conversations")
        };
        let empty = if self.filter.read(cx).text().trim().is_empty() {
            "No labels"
        } else {
            "No matching labels"
        };
        let list = div()
            .id("picker-rows")
            .max_h(px(320.0))
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .flex()
            .flex_col()
            .gap(px(1.0))
            .when(rows.is_empty(), |d| {
                d.child(
                    div()
                        .px(px(10.0))
                        .py(px(8.0))
                        .text_size(px(12.5))
                        .text_color(rgb(p.caption))
                        .child(empty),
                )
            })
            .children(
                rows.iter()
                    .enumerate()
                    .map(|(ix, e)| self.row(p, ix, e, cx).into_any_element()),
            );
        div()
            .id("label-picker")
            .key_context("LabelPicker")
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::close))
            .on_action(cx.listener(Self::submit))
            // Clicks inside the card do not reach the scrim, which closes the picker.
            .on_click(|_, _, cx| cx.stop_propagation())
            .w(px(300.0))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .p(px(10.0))
            .rounded(px(8.0))
            .border_1()
            .border_color(rgb(p.selected_border))
            .bg(rgb(p.panel_raised))
            .shadow_lg()
            .child(
                div()
                    .px(px(4.0))
                    .text_size(px(12.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(p.secondary))
                    .child(title),
            )
            .child(self.filter.clone())
            .child(list)
            .child(
                div()
                    .px(px(4.0))
                    .text_size(px(11.0))
                    .line_height(px(16.0))
                    .text_color(rgb(p.caption))
                    .child("↑ ↓ to move · Enter to toggle · Esc to close"),
            )
    }
}

impl Focusable for LabelPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.filter_focus(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn label(id: &str, name: &str, kind: LabelKind, hidden: bool) -> Label {
        Label {
            account: "you@example.com".into(),
            id: id.into(),
            name: name.into(),
            kind,
            color: None,
            threads_unread: 0,
            threads_total: 0,
            messages_total: 0,
            hidden,
        }
    }

    fn set(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    fn labels() -> Vec<Label> {
        vec![
            label("INBOX", "INBOX", LabelKind::System, false),
            label("L2", "Travel", LabelKind::User, false),
            label("L1", "receipts", LabelKind::User, false),
            label("L4", "Projects/Eloquawk", LabelKind::User, false),
            label("L9", "Archive/Old", LabelKind::User, true),
        ]
    }

    #[test]
    fn user_labels_only_sorted_by_name() {
        let names: Vec<String> = entries(&labels(), &[], "")
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(
            names,
            ["Archive/Old", "Projects/Eloquawk", "receipts", "Travel"]
        );
    }

    #[test]
    fn filter_ignores_case_and_matches_inside() {
        let names = |f: &str| -> Vec<String> {
            entries(&labels(), &[], f)
                .into_iter()
                .map(|e| e.name)
                .collect()
        };
        assert_eq!(names("TRAV"), ["Travel"]);
        assert_eq!(
            names("e"),
            ["Archive/Old", "Projects/Eloquawk", "receipts", "Travel"]
        );
        assert_eq!(names(" quawk "), ["Projects/Eloquawk"]);
        assert!(names("zzz").is_empty());
    }

    #[test]
    fn marks_count_the_targets() {
        let a = set(&["INBOX", "L2", "L1"]);
        let b = set(&["L2"]);
        let rows = entries(&labels(), &[&a, &b], "");
        let mark = |id: &str| rows.iter().find(|e| e.id == id).map(|e| e.mark);
        assert_eq!(mark("L2"), Some(Mark::All));
        assert_eq!(mark("L1"), Some(Mark::Some));
        assert_eq!(mark("L4"), Some(Mark::None));
        let adds = |id: &str| rows.iter().find(|e| e.id == id).is_some_and(Entry::adds);
        assert!(!adds("L2"), "all have it: removes");
        assert!(adds("L1"), "some have it: adds to all");
        assert!(adds("L4"));
    }

    #[test]
    fn highlight_steps_are_clamped() {
        assert_eq!(step(0, -1, 3), 0);
        assert_eq!(step(0, 1, 3), 1);
        assert_eq!(step(2, 1, 3), 2);
        assert_eq!(step(5, 1, 0), 0);
    }
}
