//! Stateless building blocks, coloured from the active palette. Disabled controls never get
//! an `on_click`.

use gpui::prelude::*;
use gpui::{
    App, ClickEvent, Div, ElementId, FontWeight, Hsla, SharedString, Stateful, Window, div, px,
    rgb, transparent_black,
};

use super::theme::{Palette, tint};

pub type OnClick = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ButtonStyle {
    Primary,
    Secondary,
    /// Outlined, transparent: the reader's action bar.
    Toolbar,
}

/// A button. `on_click: None` renders it disabled.
pub fn button(
    p: &Palette,
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    style: ButtonStyle,
    on_click: Option<OnClick>,
) -> Stateful<Div> {
    let enabled = on_click.is_some();
    let (bg, fg, border): (Hsla, u32, u32) = match (style, enabled) {
        (ButtonStyle::Toolbar, false) => (transparent_black(), p.disabled, p.border),
        (_, false) => (rgb(p.panel).into(), p.disabled, p.border),
        (ButtonStyle::Primary, true) => (rgb(p.accent).into(), p.on_accent, p.accent),
        (ButtonStyle::Secondary, true) => (rgb(p.panel_raised).into(), p.text, p.input_border),
        (ButtonStyle::Toolbar, true) => (transparent_black(), p.secondary, p.border),
    };
    let el = div()
        .id(id)
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .gap_2()
        .rounded(px(6.0))
        .border_1()
        .border_color(rgb(border))
        .bg(bg)
        .px(px(10.0))
        .h(px(28.0))
        .text_size(px(12.0))
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(fg))
        .child(label.into());
    let hover = p.hover;
    let text = p.text;
    match on_click {
        Some(handler) if style == ButtonStyle::Primary => el
            .hover(|s| s.opacity(0.88).cursor_pointer())
            .on_click(handler),
        Some(handler) => el
            .hover(move |s| s.bg(rgb(hover)).text_color(rgb(text)).cursor_pointer())
            .on_click(handler),
        None => el,
    }
}

/// A borderless text button (close, refresh, toggles).
pub fn ghost_button(
    p: &Palette,
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    on_click: Option<OnClick>,
) -> Stateful<Div> {
    let el = div()
        .id(id)
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .rounded_md()
        .px(px(8.0))
        .py(px(4.0))
        .text_size(px(13.0))
        .text_color(rgb(if on_click.is_some() {
            p.secondary
        } else {
            p.disabled
        }));
    let (hover, text) = (p.hover, p.text);
    match on_click {
        Some(handler) => el
            .hover(move |s| s.bg(rgb(hover)).text_color(rgb(text)).cursor_pointer())
            .on_click(handler)
            .child(label.into()),
        None => el.child(label.into()),
    }
}

/// Small, semibold caption text.
pub fn caption(p: &Palette, text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(11.0))
        .line_height(px(16.0))
        .text_color(rgb(p.caption))
        .child(text.into())
}

/// A titled panel (auth cards).
pub fn card(p: &Palette, title: impl Into<SharedString>) -> Div {
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_3()
        .rounded_lg()
        .border_1()
        .border_color(rgb(p.border))
        .bg(rgb(p.panel))
        .p_5()
        .child(
            div()
                .text_size(px(14.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(p.text))
                .child(title.into()),
        )
}

/// A paragraph of explanatory text.
pub fn body_text(p: &Palette, text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(12.0))
        .line_height(px(18.0))
        .text_color(rgb(p.secondary))
        .child(text.into())
}

/// A count pill (sidebar unread counts).
pub fn badge(p: &Palette, count: u32, strong: bool) -> Div {
    div()
        .flex_shrink_0()
        .text_size(px(11.0))
        .line_height(px(16.0))
        .font_weight(if strong {
            FontWeight::SEMIBOLD
        } else {
            FontWeight::NORMAL
        })
        .text_color(rgb(if strong { p.text } else { p.muted }))
        .child(count.to_string())
}

/// A label chip in the label's own colours, or tinted accent when it has none.
pub fn label_chip(p: &Palette, name: impl Into<SharedString>, color: Option<(u32, u32)>) -> Div {
    let (fg, bg) = color.unwrap_or((p.accent_text, tint(p.accent, p.panel, 0.16)));
    div()
        .flex_shrink_0()
        .px(px(6.0))
        .rounded(px(4.0))
        .bg(rgb(bg))
        .text_size(px(11.0))
        .line_height(px(18.0))
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(fg))
        .child(name.into())
}

/// A square checkbox; filled with the accent when checked.
pub fn checkbox(
    p: &Palette,
    id: impl Into<ElementId>,
    checked: bool,
    on_click: OnClick,
) -> Stateful<Div> {
    div()
        .id(id)
        .size(px(14.0))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(3.0))
        .border_1()
        .border_color(rgb(if checked { p.accent } else { p.input_border }))
        .when(checked, |d| d.bg(rgb(p.accent)))
        .when(checked, |d| {
            d.child(
                div()
                    .text_size(px(10.0))
                    .line_height(px(12.0))
                    .font_weight(FontWeight::BOLD)
                    .text_color(rgb(p.on_accent))
                    .child("✓"),
            )
        })
        .cursor_pointer()
        .on_click(on_click)
}
