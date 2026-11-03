//! The sidebar (SPEC 8.2): Compose, one collapsible section per account with its labels and
//! unread counts, and the account footer. Folded (`prefs.sidebar_collapsed`, Ctrl+B) it is a
//! rail of icons for the current account's main views.

use gpui::prelude::*;
use gpui::{ClickEvent, Context, Div, FontWeight, SharedString, div, px, rgb};
use jiff::tz::TimeZone;

use super::ToggleSidebar;
use super::format;
use super::main_view::MainView;
use super::theme::{LABEL_ROW_H, Palette, SIDEBAR_RAIL_W, SIDEBAR_W, palette};
use super::widgets::{ButtonStyle, OnClick, badge, button, icon_button};
use crate::model::{ViewKey, ViewKind, labels};
use crate::protocol::AuthState;
use crate::store::{Mode, Store};

impl MainView {
    pub(super) fn sidebar(&self, store: &Store, cx: &Context<Self>) -> Div {
        let p = palette(cx);
        if store.prefs.sidebar_collapsed {
            return self.rail(p, store, cx);
        }
        let accounts = div()
            .id("accounts")
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .children(
                store
                    .accounts
                    .iter()
                    .enumerate()
                    .map(|(ai, account)| self.account_section(p, store, ai, account, cx)),
            );
        div()
            .w(px(SIDEBAR_W))
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .px(px(10.0))
            .pt(px(12.0))
            .bg(rgb(p.sidebar))
            .border_r_1()
            .border_color(rgb(p.border))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(
                        button(
                            p,
                            "compose",
                            "Compose",
                            ButtonStyle::Primary,
                            Some(Box::new(cx.listener(
                                |this, _: &gpui::ClickEvent, _, cx| this.compose_new(cx),
                            ))),
                        )
                        .flex_1()
                        .h(px(34.0)),
                    )
                    .child(fold_button(p, true, cx)),
            )
            .child(accounts)
            .child(self.account_box(p, store))
    }

    /// The folded sidebar: unfold, compose, then the current account's main views with the
    /// inbox's unread count.
    fn rail(&self, p: &Palette, store: &Store, cx: &Context<Self>) -> Div {
        let account = store
            .view
            .as_ref()
            .map(|v| v.account.clone())
            .or_else(|| store.accounts.first().cloned());
        let mut rail = div()
            .w(px(SIDEBAR_RAIL_W))
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(6.0))
            .pt(px(12.0))
            .bg(rgb(p.sidebar))
            .border_r_1()
            .border_color(rgb(p.border))
            .child(fold_button(p, false, cx))
            .child(icon_button(
                p,
                "rail-compose",
                "Compose",
                r#"<path d="M4 20h4L19 9l-4-4L4 16z"/><path d="m13.5 6.5 4 4"/>"#,
                Some(Box::new(cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.compose_new(cx)
                }))),
            ))
            .child(div().h(px(6.0)));
        let Some(account) = account else {
            return rail;
        };
        for (id, name, icon) in RAIL_VIEWS {
            let kind = match *id {
                "ALL_MAIL" => ViewKind::AllMail,
                id => ViewKind::Label(id.to_string()),
            };
            let view = ViewKey {
                account: account.clone(),
                kind,
            };
            let active = store.view.as_ref() == Some(&view);
            let count = (*id == labels::INBOX)
                .then(|| store.unread_badge(&account, labels::INBOX))
                .flatten();
            let on_click: OnClick =
                Box::new(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    let view = view.clone();
                    this.store.update(cx, |s, cx| {
                        s.set_view(view);
                        cx.notify();
                    });
                    this.focus_mail(window, cx);
                }));
            let button = icon_button(
                p,
                SharedString::from(format!("rail-{id}")),
                name,
                icon,
                Some(on_click),
            )
            .when(active, |d| d.bg(rgb(p.selected)));
            rail = rail.child(div().relative().child(button).when_some(count, |d, n| {
                d.child(
                    div()
                        .absolute()
                        .top(px(-4.0))
                        .right(px(-8.0))
                        .child(badge(p, n, true)),
                )
            }));
        }
        rail
    }

    fn account_section(
        &self,
        p: &Palette,
        store: &Store,
        ai: usize,
        account: &str,
        cx: &Context<Self>,
    ) -> Div {
        let collapsed = store.prefs.collapsed_accounts.contains(account);
        let toggle_account = account.to_string();
        let inbox_unread = store.unread_badge(account, labels::INBOX);
        let header = div()
            .id(("account", ai))
            .h(px(LABEL_ROW_H))
            .flex()
            .items_center()
            .gap(px(6.0))
            .px(px(6.0))
            .rounded(px(6.0))
            .cursor_pointer()
            .hover({
                let hover = p.hover;
                move |s| s.bg(rgb(hover))
            })
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                let account = toggle_account.clone();
                this.store.update(cx, |s, cx| {
                    if !s.prefs.collapsed_accounts.remove(&account) {
                        s.prefs.collapsed_accounts.insert(account);
                    }
                    s.prefs.save();
                    cx.notify();
                });
            }))
            .child(
                div()
                    .w(px(12.0))
                    .flex_shrink_0()
                    .text_size(px(11.0))
                    .text_color(rgb(p.muted))
                    .child(if collapsed { "›" } else { "⌄" }),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .truncate()
                    .text_size(px(12.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(p.secondary))
                    .child(account.to_string()),
            )
            .when_some(inbox_unread.filter(|_| collapsed), |d, n| {
                d.child(badge(p, n, true))
            });
        let mut section = div().flex().flex_col().child(header);
        if collapsed {
            return section;
        }
        let entries = store.labels_for(account);
        let mut hidden_below = None;
        for (li, entry) in entries.iter().enumerate() {
            if let Some(depth) = hidden_below {
                if entry.depth > depth {
                    continue;
                }
                hidden_below = None;
            }
            let has_children = entries
                .get(li + 1)
                .is_some_and(|next| next.depth > entry.depth);
            let label_key = match &entry.view {
                ViewKind::Label(id) if has_children => Some((account.to_string(), id.clone())),
                _ => None,
            };
            let label_collapsed = label_key
                .as_ref()
                .is_some_and(|key| store.prefs.collapsed_labels.contains(key));
            if label_collapsed {
                hidden_below = Some(entry.depth);
            }
            let view = ViewKey {
                account: account.to_string(),
                kind: entry.view.clone(),
            };
            let active = store.view.as_ref() == Some(&view);
            let unread = entry.count.is_some();
            let dot = match &entry.view {
                ViewKind::Label(_) => entry.color.map(|(_, bg)| bg),
                _ => None,
            };
            let hover = p.hover;
            let row = div()
                .id(SharedString::from(format!("label-{ai}-{li}")))
                .h(px(LABEL_ROW_H))
                .flex()
                .items_center()
                .gap(px(8.0))
                .pl(px(8.0 + 14.0 * entry.depth as f32))
                .pr(px(8.0))
                .rounded(px(6.0))
                .cursor_pointer()
                .when(active, |d| d.bg(rgb(p.selected)))
                .when(!active, |d| d.hover(move |s| s.bg(rgb(hover))))
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    let view = view.clone();
                    this.store.update(cx, |s, cx| {
                        s.set_view(view);
                        cx.notify();
                    });
                    this.focus_mail(window, cx);
                }))
                .child(match label_key {
                    Some(key) => div()
                        .id(SharedString::from(format!("label-toggle-{ai}-{li}")))
                        .w(px(16.0))
                        .flex_shrink_0()
                        .text_color(rgb(p.muted))
                        .child(if label_collapsed { "›" } else { "⌄" })
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            cx.stop_propagation();
                            let key = key.clone();
                            this.store.update(cx, |s, cx| {
                                if !s.prefs.collapsed_labels.remove(&key) {
                                    s.prefs.collapsed_labels.insert(key);
                                }
                                s.prefs.save();
                                cx.notify();
                            });
                        }))
                        .into_any_element(),
                    None => div().w(px(16.0)).flex_shrink_0().into_any_element(),
                })
                .when_some(dot, |d, color| {
                    d.child(
                        div()
                            .size(px(8.0))
                            .flex_shrink_0()
                            .rounded_full()
                            .bg(rgb(color)),
                    )
                })
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .truncate()
                        .text_size(px(13.0))
                        .font_weight(if active || unread {
                            FontWeight::SEMIBOLD
                        } else {
                            FontWeight::NORMAL
                        })
                        .text_color(rgb(if active { p.accent_text } else { p.text }))
                        .child(entry.name.clone()),
                )
                .when_some(entry.count, |d, n| d.child(badge(p, n, true)));
            section = section.child(row);
        }
        section
    }

    fn account_box(&self, p: &Palette, store: &Store) -> Div {
        let (dot, line1) = match (store.auth, store.accounts.as_slice()) {
            (AuthState::Starting, _) => (p.disabled, "Opening keyring…".to_string()),
            (AuthState::Locked, _) => (p.amber, "Keyring locked".into()),
            (AuthState::NeedsClient, _) => (p.amber, "No Google client".into()),
            (AuthState::Demo, _) => (p.accent, "Demo mailbox".into()),
            _ if store.signing_in.is_some() => (p.amber, "Waiting for browser…".into()),
            (AuthState::Ready, []) => (p.amber, "Not signed in".into()),
            (AuthState::Ready, [one]) => (p.green, one.clone()),
            (AuthState::Ready, many) => (p.green, format!("{} Google accounts", many.len())),
        };
        let dot = if store.busy { p.amber } else { dot };
        let line2 = if store.busy {
            "Syncing…".to_string()
        } else if store.mode == Mode::Demo {
            "Nothing is sent or saved".to_string()
        } else if let Some(t) = store.last_sync {
            let t = format::time(&t.to_zoned(TimeZone::system()), store.prefs.clock_24h);
            format!("Synced {t}")
        } else {
            String::new()
        };
        div()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .py(px(10.0))
            .px(px(6.0))
            .border_t_1()
            .border_color(rgb(p.border))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .size(px(6.0))
                            .flex_shrink_0()
                            .rounded_full()
                            .bg(rgb(dot)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .truncate()
                            .text_size(px(11.5))
                            .line_height(px(16.0))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(rgb(p.secondary))
                            .child(line1),
                    ),
            )
            .when(!line2.is_empty(), |d| {
                d.child(
                    div()
                        .pl(px(14.0))
                        .truncate()
                        .text_size(px(11.0))
                        .line_height(px(16.0))
                        .text_color(rgb(p.caption))
                        .child(line2),
                )
            })
    }
}

/// The rail's views: the label id (`ALL_MAIL` for All Mail), its name and its icon.
const RAIL_VIEWS: &[(&str, &str, &str)] = &[
    (
        labels::INBOX,
        "Inbox",
        r#"<path d="M3 13h5l1.5 3h5L16 13h5"/><path d="M5 5h14l2 8v6H3v-6z"/>"#,
    ),
    (
        labels::STARRED,
        "Starred",
        r#"<path d="m12 3 2.8 5.7 6.2.9-4.5 4.4 1.1 6.2L12 17.3l-5.6 2.9 1.1-6.2L3 9.6l6.2-.9z"/>"#,
    ),
    (
        labels::SENT,
        "Sent",
        r#"<path d="M21 3 3 10.5l7 2.5 2.5 7z"/><path d="m10 13 4-4"/>"#,
    ),
    (
        labels::DRAFT,
        "Drafts",
        r#"<path d="M6 3h8l4 4v14H6z"/><path d="M14 3v4h4M9 12h6M9 16h4"/>"#,
    ),
    (
        "ALL_MAIL",
        "All Mail",
        r#"<rect x="3" y="5" width="18" height="14" rx="2"/><path d="m3 7 9 6 9-6"/>"#,
    ),
];

/// Folds (`fold`) or unfolds the sidebar.
fn fold_button(p: &Palette, fold: bool, cx: &Context<MainView>) -> gpui::Stateful<Div> {
    let (label, icon) = if fold {
        (
            "Fold the sidebar (Ctrl B)",
            r#"<rect x="3" y="4" width="18" height="16" rx="2"/><path d="M9 4v16m6-5-3-3 3-3"/>"#,
        )
    } else {
        (
            "Unfold the sidebar (Ctrl B)",
            r#"<rect x="3" y="4" width="18" height="16" rx="2"/><path d="M9 4v16m3-11 3 3-3 3"/>"#,
        )
    };
    icon_button(
        p,
        if fold {
            "sidebar-fold"
        } else {
            "sidebar-unfold"
        },
        label,
        icon,
        Some(Box::new(cx.listener(|this, _: &ClickEvent, window, cx| {
            this.toggle_sidebar(&ToggleSidebar, window, cx)
        }))),
    )
}
