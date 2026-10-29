//! The sidebar (SPEC 8.2): Compose, one collapsible section per account with its labels and
//! unread counts, and the account footer.

use gpui::prelude::*;
use gpui::{ClickEvent, Context, Div, FontWeight, SharedString, div, px, rgb};
use jiff::tz::TimeZone;

use super::format;
use super::main_view::MainView;
use super::theme::{LABEL_ROW_H, Palette, SIDEBAR_W, palette};
use super::widgets::{ButtonStyle, badge, button};
use crate::model::{ViewKey, ViewKind, labels};
use crate::protocol::AuthState;
use crate::store::{Mode, Store};

impl MainView {
    pub(super) fn sidebar(&self, store: &Store, cx: &Context<Self>) -> Div {
        let p = palette(cx);
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
            // Compose arrives in M8.
            .child(button(p, "compose", "Compose", ButtonStyle::Primary, None).h(px(34.0)))
            .child(accounts)
            .child(self.account_box(p, store))
    }

    fn account_section(
        &self,
        p: &Palette,
        store: &Store,
        ai: usize,
        account: &str,
        cx: &Context<Self>,
    ) -> Div {
        // The active account is always expanded (SPEC 8.2), so its selected label stays in
        // sight whatever `collapsed_accounts` remembers; its header does not collapse it.
        let active = store.view.as_ref().is_some_and(|v| v.account == account);
        let collapsed = !active && store.prefs.collapsed_accounts.contains(account);
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
                    if s.view.as_ref().is_some_and(|v| v.account == account) {
                        return;
                    }
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
        for (li, entry) in store.labels_for(account).into_iter().enumerate() {
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
                .pl(px(24.0 + 14.0 * entry.depth as f32))
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
