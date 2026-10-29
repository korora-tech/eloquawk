//! Eloquawk entry point: opens the main window and quits when the last window closes.
//!
//! This is the scaffold. The worker, store and three-pane UI arrive in later milestones.

mod branding;

use gpui::{
    App, AppContext, Bounds, Context, FocusHandle, Focusable, InteractiveElement, IntoElement,
    KeyBinding, ParentElement, QuitMode, Render, Styled, TitlebarOptions, Window, WindowBounds,
    WindowOptions, actions, div, px, rgb, size,
};
use gpui_platform::application;

actions!(eloquawk, [Quit]);

/// The window title, also used by the screenshot script to find the window.
const TITLE: &str = "Eloquawk";

/// Placeholder root view until the mail view lands.
struct MainView {
    focus: FocusHandle,
}

impl Focusable for MainView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for MainView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgb(0x1c1d21))
            .text_color(rgb(0xe8e8ec))
            .text_xl()
            .child(TITLE)
    }
}

fn open_main_window(cx: &mut App) {
    let bounds = Bounds::centered(None, size(px(1280.), px(820.)), cx);
    let window = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some(TITLE.into()),
                ..Default::default()
            }),
            app_id: Some(branding::APP_ID.into()),
            ..Default::default()
        },
        |window, cx| {
            let view = cx.new(|cx| MainView {
                focus: cx.focus_handle(),
            });
            window.focus(&view.focus_handle(cx), cx);
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

fn main() {
    // Explicit: the app decides when to quit, which is when its last window has closed.
    application()
        .with_quit_mode(QuitMode::Explicit)
        .run(|cx: &mut App| {
            cx.on_action(|_: &Quit, cx| cx.quit());
            cx.bind_keys([KeyBinding::new("ctrl-q", Quit, None)]);
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            open_main_window(cx);
        });
}
