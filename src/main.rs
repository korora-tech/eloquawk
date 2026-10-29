//! Eloquawk entry point: parses arguments, starts the worker, wraps the [`Store`] in a GPUI
//! entity, pumps worker events into it, and opens the main window. Quits when the last window
//! closes, telling the worker to shut down.
//!
//! At M3 only the demo worker exists (`--demo`); the window is still the placeholder until
//! the three-pane UI lands in M4.

mod branding;
mod cli;
mod config;
mod demo;
mod demo_data;
mod google;
mod mail;
mod model;
mod protocol;
mod store;

use gpui::{
    App, AppContext, Bounds, Context, FocusHandle, Focusable, InteractiveElement, IntoElement,
    KeyBinding, ParentElement, QuitMode, Render, Styled, TitlebarOptions, Window, WindowBounds,
    WindowOptions, actions, div, px, rgb, size,
};
use gpui_platform::application;

use protocol::{Command, SyncHandle};
use store::{Mode, Store};

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
    let args: Vec<String> = std::env::args().skip(1).collect();
    let env_demo = cli::env_demo(std::env::var_os("ELOQUAWK_DEMO").as_deref());
    let args = match cli::parse(&args, env_demo) {
        Ok(cli::Parsed::Run(args)) => args,
        Ok(cli::Parsed::Help) => {
            println!("{}", cli::USAGE);
            return;
        }
        Ok(cli::Parsed::Version) => {
            println!("eloquawk {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        Err(e) => {
            eprintln!("eloquawk: {e}\n{}", cli::USAGE);
            std::process::exit(2);
        }
    };

    let mode = if args.demo { Mode::Demo } else { Mode::Google };
    let mut prefs = config::Prefs::load();
    prefs.ephemeral = mode == Mode::Demo;
    let (sync, events) = if mode == Mode::Demo {
        match demo::start(&prefs) {
            Ok(worker) => (worker.0, Some(worker.1)),
            Err(e) => {
                eprintln!("eloquawk: could not start the demo worker: {e}");
                std::process::exit(1);
            }
        }
    } else {
        // The Google worker arrives in M9/M10. Until then commands go nowhere.
        eprintln!("eloquawk: Gmail sync is not available yet; run with --demo");
        (SyncHandle::from_sender(std::sync::mpsc::channel().0), None)
    };

    // Explicit: the app decides when to quit, which is when its last window has closed.
    application()
        .with_quit_mode(QuitMode::Explicit)
        .run(move |cx: &mut App| {
            let store = cx.new(|_| Store::new(sync.clone(), mode, prefs));

            // Worker → store. The count is logged whenever the queue drains, so a demo run
            // shows that events flow without logging any mail.
            if let Some(events) = events {
                let store = store.clone();
                cx.spawn(async move |cx| {
                    let mut count: u64 = 0;
                    while let Ok(ev) = events.recv().await {
                        count += 1;
                        store.update(cx, |s, cx| {
                            s.apply(ev);
                            cx.notify();
                        });
                        if events.is_empty() {
                            eprintln!("eloquawk: applied {count} worker events");
                        }
                    }
                })
                .detach();
            }

            cx.on_action(|_: &Quit, cx| cx.quit());
            cx.bind_keys([KeyBinding::new("ctrl-q", Quit, None)]);
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            cx.on_app_quit(move |_| {
                sync.send(Command::Shutdown);
                async {}
            })
            .detach();
            open_main_window(cx);
        });
}
