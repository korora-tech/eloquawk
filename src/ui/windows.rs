//! Opening and finding the main window and the compose windows.

use gpui::{
    App, AppContext, Bounds, Entity, TitlebarOptions, WindowBounds, WindowHandle, WindowOptions,
    px, size,
};

use super::compose::{ComposeView, title_for};
use super::main_view::MainView;
use crate::branding;
use crate::model::Outgoing;
use crate::store::Store;

/// The main window's title; the screenshot script finds the window by it.
pub const MAIN_TITLE: &str = "Eloquawk";

/// The open main window, if any.
pub fn main_window(cx: &App) -> Option<WindowHandle<MainView>> {
    cx.windows()
        .into_iter()
        .find_map(|window| window.downcast::<MainView>())
}

/// Brings the main window forward, opening it when there is none.
pub fn show_main_window(store: &Entity<Store>, cx: &mut App) {
    raise_main_window(store, None, cx);
}

/// Raises `window` with a launcher's activation `token` when there is one (a Wayland
/// compositor only hands focus over for such a token), else asks for activation as gpui does.
fn raise(window: &mut gpui::Window, token: Option<&str>) {
    if let Some(token) = token {
        match super::activation::activate(window, token) {
            Ok(()) => return,
            Err(e) => eprintln!("eloquawk: could not use the activation token: {e}"),
        }
    }
    window.activate_window();
}

/// [`show_main_window`], spending `token` (handed over by a later launch) on the existing
/// window.
pub fn raise_main_window(store: &Entity<Store>, token: Option<&str>, cx: &mut App) {
    if let Some(window) = main_window(cx) {
        if let Err(e) = window.update(cx, |view, window, cx| {
            view.focus_mail(window, cx);
            raise(window, token);
        }) {
            eprintln!("eloquawk: could not activate the main window: {e}");
        }
        cx.activate(true);
        return;
    }
    let bounds = Bounds::centered(None, size(px(1280.), px(820.)), cx);
    let store = store.clone();
    let window = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some(MAIN_TITLE.into()),
                ..Default::default()
            }),
            app_id: Some(branding::APP_ID.into()),
            window_min_size: Some(size(px(960.), px(620.))),
            ..Default::default()
        },
        move |window, cx| {
            let view = cx.new(|cx| MainView::new(store, window, cx));
            view.update(cx, |view, cx| view.focus_mail(window, cx));
            view
        },
    );
    match window {
        Ok(_) => cx.activate(true),
        Err(e) => {
            eprintln!("eloquawk: could not open a window: {e}");
            cx.quit();
        }
    }
}

/// Opens a compose window with `prefill`, titled "New message" or after the reply's or
/// forward's subject. The focus starts in To for a new message or a forward and in the body
/// for a reply.
pub fn open_compose_window(store: &Entity<Store>, prefill: Outgoing, cx: &mut App) {
    open_compose_window_raised(store, prefill, None, cx);
}

/// [`open_compose_window`], spending `token` (handed over by a later launch with a `mailto:`
/// link) on the new window.
pub fn open_compose_window_raised(
    store: &Entity<Store>,
    prefill: Outgoing,
    token: Option<&str>,
    cx: &mut App,
) {
    let bounds = Bounds::centered(None, size(px(720.), px(640.)), cx);
    let title = title_for(&prefill);
    let store = store.clone();
    let result = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some(title.clone().into()),
                ..Default::default()
            }),
            app_id: Some(branding::APP_ID.into()),
            window_min_size: Some(size(px(480.), px(400.))),
            ..Default::default()
        },
        move |window, cx| {
            let view = cx.new(|cx| ComposeView::new(store, prefill, title, window, cx));
            view.update(cx, |view, cx| view.focus_initial(window, cx));
            view
        },
    );
    match result {
        Ok(handle) => {
            let _ = handle.update(cx, |_, window, _| raise(window, token));
        }
        Err(e) => eprintln!("eloquawk: could not open a compose window: {e}"),
    }
}
