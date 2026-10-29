//! The thread list pane (SPEC 8.2): a title row, the rows in a `uniform_list` with fixed
//! `ROW_H` rows, and a "Loading…" / "Load more" footer as a sibling below the list. The list
//! follows the cursor through a `UniformListScrollHandle`, and asks for the next page when
//! the visible range nears the end.

use std::ops::Range;

use gpui::prelude::*;
use gpui::{
    AnyElement, ClickEvent, Context, Div, FontWeight, HighlightStyle, SharedString, StyledText,
    div, px, rgb, uniform_list,
};
use jiff::Zoned;

use super::format;
use super::main_view::MainView;
use super::theme::{LIST_W, Palette, ROW_H, palette, tabular};
use super::widgets::{checkbox, ghost_button};
use crate::model::{LabelDelta, ThreadSummary, ViewKind};
use crate::store::{MutationKind, Row, Store};

/// Rows from the end at which the next page is requested.
const MORE_WITHIN: usize = 10;

/// The list title for the current view.
fn view_title(store: &Store) -> String {
    let Some(view) = &store.view else {
        return String::new();
    };
    match &view.kind {
        ViewKind::AllMail => "All Mail".into(),
        ViewKind::Search(q) => format!("Search results for \u{201c}{q}\u{201d}"),
        ViewKind::Label(id) => store
            .labels
            .get(&view.account)
            .and_then(|ls| ls.iter().find(|l| l.id == *id))
            .map(|l| {
                store
                    .labels_for(&view.account)
                    .into_iter()
                    .find(|e| e.view == view.kind)
                    .map_or_else(|| l.name.clone(), |e| e.name)
            })
            .unwrap_or_else(|| id.clone()),
    }
}

/// "1,234": thousands separated by commas.
fn grouped(n: u32) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

impl MainView {
    pub(super) fn thread_list(&self, store: &Store, cx: &Context<Self>) -> Div {
        let p = palette(cx);
        let rows = store.visible_rows().len();
        let list = store.current_list();
        let loading = list.is_some_and(|l| l.loading);
        let has_more = list.is_some_and(|l| l.has_more);
        let total = match store.view.as_ref().map(|v| (&v.account, &v.kind)) {
            Some((account, ViewKind::Label(id))) => store
                .labels
                .get(account)
                .and_then(|ls| ls.iter().find(|l| l.id == *id))
                .map_or(rows as u32, |l| l.threads_total.max(rows as u32)),
            _ => rows as u32,
        };
        let selected = store.selected.len();
        let count = match total {
            1 => "1 conversation".to_string(),
            n => format!("{} conversations", grouped(n)),
        };
        let title_row = div()
            .flex_shrink_0()
            .h(px(48.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .px(px(16.0))
            .border_b_1()
            .border_color(rgb(p.border))
            .child(
                div()
                    .min_w(px(0.0))
                    .truncate()
                    .text_size(px(15.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(p.text))
                    .child(view_title(store)),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_size(px(12.0))
                    .text_color(rgb(p.caption))
                    .child(count),
            )
            .child(div().flex_1())
            .when(selected > 0, |d| {
                d.child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(12.0))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(rgb(p.accent_text))
                        .child(format!("{selected} selected")),
                )
                .child(ghost_button(
                    p,
                    "clear-selection",
                    "Clear",
                    Some(Box::new(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.store.update(cx, |s, cx| {
                            s.clear_selection();
                            cx.notify();
                        });
                    }))),
                ))
            });

        let body: AnyElement = if rows == 0 {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(13.0))
                .text_color(rgb(p.muted))
                .child(if loading || store.view.is_none() {
                    "Loading…"
                } else {
                    "No conversations"
                })
                .into_any_element()
        } else {
            uniform_list(
                "threads",
                rows,
                cx.processor(|this, range: Range<usize>, _window, cx| this.render_rows(range, cx)),
            )
            .track_scroll(&self.list_scroll)
            .flex_1()
            .into_any_element()
        };

        let footer = if rows > 0 && loading {
            Some(
                div()
                    .flex_shrink_0()
                    .h(px(36.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(12.0))
                    .text_color(rgb(p.muted))
                    .child("Loading…"),
            )
        } else if rows > 0 && has_more {
            Some(
                div()
                    .flex_shrink_0()
                    .h(px(36.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(ghost_button(
                        p,
                        "load-more",
                        "Load more",
                        Some(Box::new(cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.store.update(cx, |s, cx| {
                                s.request_more();
                                cx.notify();
                            });
                        }))),
                    )),
            )
        } else {
            None
        };

        div()
            .w(px(LIST_W))
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .bg(rgb(p.panel))
            .border_r_1()
            .border_color(rgb(p.border))
            .child(title_row)
            .child(body)
            .children(footer)
    }

    fn render_rows(&mut self, range: Range<usize>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let p = palette(cx);
        let now = Zoned::now();
        let store = self.store.read(cx);
        let rows = store.visible_rows();
        // Near the end of what is loaded: ask for more (the store sends it once).
        let want_more = store
            .current_list()
            .is_some_and(|l| l.has_more && !l.loading && range.end + MORE_WITHIN >= rows.len());
        let cursor = store.cursor.clone();
        let clock = store.prefs.clock_24h;
        let mut out = Vec::with_capacity(range.len());
        for ix in range {
            let Some(row) = rows.get(ix) else { break };
            let el = match row {
                Row::Skeleton(_) => skeleton_row(p, ix),
                Row::Thread(s) => {
                    let label = store.participants_label(s).to_string();
                    let state = RowState {
                        cursor: cursor.as_ref() == Some(&s.key),
                        selected: store.selected.contains(&s.key),
                    };
                    self.thread_row(p, ix, s, label, state, &now, clock, cx)
                }
            };
            out.push(el);
        }
        if want_more {
            let store = self.store.clone();
            cx.defer(move |cx| {
                store.update(cx, |s, cx| {
                    s.request_more();
                    cx.notify();
                })
            });
        }
        out
    }

    #[allow(clippy::too_many_arguments)]
    fn thread_row(
        &self,
        p: &Palette,
        ix: usize,
        s: &ThreadSummary,
        participants: String,
        state: RowState,
        now: &Zoned,
        clock_24h: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let unread = s.unread();
        let starred = s.starred();
        let key = s.key.clone();
        let open = cx.listener(move |this, _: &ClickEvent, window, cx| {
            let key = key.clone();
            this.store.update(cx, |st, cx| {
                st.set_cursor_index(ix);
                st.open_thread(key);
                cx.notify();
            });
            this.focus_mail(window, cx);
        });
        let star_key = s.key.clone();
        let star = cx.listener(move |this, _: &ClickEvent, _, cx| {
            cx.stop_propagation();
            let delta = if starred {
                LabelDelta::unstar()
            } else {
                LabelDelta::star()
            };
            let key = star_key.clone();
            this.store.update(cx, |st, cx| {
                st.mutate_threads(vec![key], MutationKind::Labels(delta));
                cx.notify();
            });
        });
        let select_key = s.key.clone();
        let select = cx.listener(move |this, _: &ClickEvent, _, cx| {
            cx.stop_propagation();
            let key = select_key.clone();
            this.store.update(cx, |st, cx| {
                st.toggle_select(key);
                cx.notify();
            });
        });
        let subject = if s.subject.trim().is_empty() {
            "(no subject)".to_string()
        } else {
            s.subject.clone()
        };
        // Unread rows set the sender and subject semibold in the text colour; read rows
        // drop both to the regular weight and the secondary colour, so the two read apart at
        // a glance.
        let (weight, strong) = if unread {
            (FontWeight::SEMIBOLD, p.text)
        } else {
            (FontWeight::NORMAL, p.secondary)
        };
        let line2 = format!("{subject} \u{2014} {}", s.snippet);
        let subject_end = subject.len();
        let line2 = StyledText::new(SharedString::from(line2)).with_highlights([
            (
                0..subject_end,
                HighlightStyle {
                    color: Some(rgb(strong).into()),
                    font_weight: Some(weight),
                    ..Default::default()
                },
            ),
            (
                subject_end..subject_end + " \u{2014} ".len() + s.snippet.len(),
                HighlightStyle {
                    color: Some(rgb(p.muted).into()),
                    ..Default::default()
                },
            ),
        ]);
        let hover = p.hover;
        div()
            .id(("thread", ix))
            .relative()
            .h(px(ROW_H))
            .w_full()
            .flex()
            .items_center()
            .gap(px(10.0))
            .pl(px(14.0))
            .pr(px(12.0))
            .border_b_1()
            .border_color(rgb(p.border))
            .cursor_pointer()
            .when(state.cursor, |d| d.bg(rgb(p.selected)))
            .when(!state.cursor, |d| d.hover(move |h| h.bg(rgb(hover))))
            .on_click(open)
            .when(state.cursor, |d| {
                d.child(
                    div()
                        .absolute()
                        .left_0()
                        .top_0()
                        .bottom_0()
                        .w(px(3.0))
                        .bg(rgb(p.accent)),
                )
            })
            .child(checkbox(
                p,
                ("select", ix),
                state.selected,
                Box::new(select),
            ))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .h(px(20.0))
                            .child(
                                div()
                                    .size(px(7.0))
                                    .flex_shrink_0()
                                    .rounded_full()
                                    .when(unread, |d| d.bg(rgb(p.accent))),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .truncate()
                                    .text_size(px(13.5))
                                    .line_height(px(20.0))
                                    .font_weight(weight)
                                    .text_color(rgb(strong))
                                    .child(participants),
                            )
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_size(px(12.0))
                                    .line_height(px(20.0))
                                    .font_features(tabular())
                                    .font_weight(if unread {
                                        FontWeight::SEMIBOLD
                                    } else {
                                        FontWeight::NORMAL
                                    })
                                    .text_color(rgb(if unread { p.accent_text } else { p.caption }))
                                    .child(format::list_date(s.last_date, now, clock_24h)),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .h(px(20.0))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .pl(px(13.0))
                                    .truncate()
                                    .text_size(px(13.0))
                                    .line_height(px(20.0))
                                    .child(line2),
                            )
                            .when(s.has_attachments, |d| d.child(attachment_mark(p)))
                            .child(
                                div()
                                    .id(("star", ix))
                                    .flex_shrink_0()
                                    .w(px(18.0))
                                    .flex()
                                    .justify_center()
                                    .text_size(px(15.0))
                                    .line_height(px(20.0))
                                    .text_color(rgb(if starred { p.amber } else { p.muted }))
                                    .hover(|h| h.opacity(0.75))
                                    .on_click(star)
                                    .child(if starred { "★" } else { "☆" }),
                            ),
                    ),
            )
            .into_any_element()
    }
}

#[derive(Clone, Copy)]
struct RowState {
    cursor: bool,
    selected: bool,
}

/// A paper clip drawn with divs (the UI font has no clip glyph and emoji are not used): the
/// wire's narrow top bend, its wide bottom bend with the free end on the right, and the inner
/// U whose free end points up.
fn attachment_mark(p: &Palette) -> Div {
    let wire = rgb(p.muted);
    let stroke = px(1.5);
    div()
        .flex_shrink_0()
        .relative()
        .w(px(10.0))
        .h(px(15.0))
        // Top bend: outer left side up and over to the inner right side.
        .child(
            div()
                .absolute()
                .left_0()
                .top_0()
                .w(px(7.5))
                .h(px(10.0))
                .border_t(stroke)
                .border_l(stroke)
                .border_r(stroke)
                .rounded_t(px(3.75))
                .border_color(wire),
        )
        // Bottom bend: the outer left side down and round, ending part way up the right.
        .child(
            div()
                .absolute()
                .left_0()
                .top(px(5.0))
                .w(px(10.0))
                .h(px(10.0))
                .border_b(stroke)
                .border_l(stroke)
                .border_r(stroke)
                .rounded_b(px(5.0))
                .border_color(wire),
        )
        // Inner U, sharing the top bend's right side.
        .child(
            div()
                .absolute()
                .left(px(3.0))
                .top(px(3.5))
                .w(px(4.5))
                .h(px(8.0))
                .border_b(stroke)
                .border_l(stroke)
                .border_r(stroke)
                .rounded_b(px(2.25))
                .border_color(wire),
        )
}

/// A row whose summary has not arrived: grey bars.
fn skeleton_row(p: &Palette, ix: usize) -> AnyElement {
    let bar = |w: f32| {
        div()
            .w(px(w))
            .h(px(10.0))
            .rounded(px(3.0))
            .bg(rgb(p.skeleton))
    };
    div()
        .id(("skeleton", ix))
        .h(px(ROW_H))
        .w_full()
        .flex()
        .flex_col()
        .justify_center()
        .gap(px(10.0))
        .px(px(38.0))
        .border_b_1()
        .border_color(rgb(p.border))
        .child(bar(140.0))
        .child(bar(260.0))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_are_grouped() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(1000), "1,000");
        assert_eq!(grouped(1234567), "1,234,567");
    }
}
