//! Eloquawk entry point: parses arguments, starts the worker, wraps the [`Store`] in a GPUI
//! entity, pumps worker events into it, applies the theme, opens the main window and, in demo
//! mode, runs a `--demo-actions` script. Quits when the last window closes, telling the
//! worker to shut down.

mod branding;
mod cli;
mod config;
mod demo;
mod demo_data;
mod demo_script;
mod google;
mod mail;
mod model;
mod protocol;
mod store;
mod ui;

use std::io::Write as _;
use std::time::Duration;

use gpui::{AnyWindowHandle, App, AppContext, Entity, Keystroke, QuitMode};
use gpui_platform::application;

use demo_script::Step;
use protocol::{Command, SyncEvent, SyncHandle};
use store::{Mode, Store};
use ui::theme::{self, ThemeOverride};

/// The pause before each script step.
const STEP_DELAY: Duration = Duration::from_millis(250);

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
    let script = args.demo_actions.as_deref().map(demo_script::parse_steps);
    let theme_override = args.theme;

    // Explicit: the app decides when to quit, which is when its last window has closed.
    application()
        .with_quit_mode(QuitMode::Explicit)
        .run(move |cx: &mut App| {
            ui::bind_keys(cx);
            cx.set_global(ThemeOverride(theme_override));
            theme::init_ui_font(cx);
            let store = cx.new(|_| Store::new(sync.clone(), mode, prefs));

            // Signals the first ThreadPage, which starts the demo script.
            let (first_page_tx, first_page_rx) = async_channel::bounded::<()>(1);

            // Worker → store. The count is logged whenever the queue drains, so a run shows
            // that events flow without logging any mail.
            if let Some(events) = events {
                let store = store.clone();
                cx.spawn(async move |cx| {
                    let mut count: u64 = 0;
                    while let Ok(ev) = events.recv().await {
                        count += 1;
                        let page = matches!(ev, SyncEvent::ThreadPage { .. });
                        store.update(cx, |s, cx| {
                            s.apply(ev);
                            cx.notify();
                        });
                        if page {
                            let _ = first_page_tx.try_send(());
                        }
                        if events.is_empty() {
                            eprintln!("eloquawk: applied {count} worker events");
                        }
                    }
                })
                .detach();
            }

            cx.on_action(|_: &ui::Quit, cx| cx.quit());
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
            ui::windows::show_main_window(&store, cx);

            if let Some(steps) = script {
                run_script(steps, store, first_page_rx, cx);
            }
        });
}

/// Runs `--demo-actions` steps after the first thread page, one every [`STEP_DELAY`].
fn run_script(
    steps: Vec<Step>,
    store: Entity<Store>,
    first_page: async_channel::Receiver<()>,
    cx: &mut App,
) {
    cx.spawn(async move |cx| {
        if first_page.recv().await.is_err() {
            return;
        }
        for step in steps {
            cx.background_executor().timer(STEP_DELAY).await;
            if let Step::Wait(ms) = step {
                cx.background_executor()
                    .timer(Duration::from_millis(ms))
                    .await;
                continue;
            }
            cx.update(|cx| run_step(&step, &store, cx));
        }
    })
    .detach();
}

fn report(line: String) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

/// Sends one keystroke to `window` as if typed.
fn send_key(window: AnyWindowHandle, ks: Keystroke, cx: &mut App) {
    if let Err(e) = window.update(cx, |_, window, cx| {
        window.dispatch_keystroke(ks, cx);
    }) {
        eprintln!("eloquawk: could not send a scripted key: {e}");
    }
}

fn run_step(step: &Step, store: &Entity<Store>, cx: &mut App) {
    let Some(main) = ui::windows::main_window(cx) else {
        report("ELOQUAWK_EXPECT fail step (no window)".into());
        return;
    };
    // The focused window; the main window when none is (for example, unfocused on niri).
    let target: AnyWindowHandle = cx.active_window().unwrap_or_else(|| main.into());
    match step {
        Step::Wait(_) => {}
        Step::Shot => {
            // gpui's Wayland and X11 windows report an empty title, so the main window's
            // known title stands in.
            let title = target
                .update(cx, |_, window, _| window.window_title())
                .unwrap_or_default();
            let main_any: AnyWindowHandle = main.into();
            let title = if title.is_empty() && target == main_any {
                ui::windows::MAIN_TITLE.to_string()
            } else {
                title
            };
            report(format!("ELOQUAWK_READY {title}"));
        }
        Step::Theme(pref) => {
            cx.set_global(ThemeOverride(Some(*pref)));
            let saved = store.read(cx).prefs.theme;
            let _ = main.update(cx, |_, window, cx| theme::apply(saved, window, cx));
        }
        Step::Open(n) => {
            let _ = main.update(cx, |view, _, cx| view.open_row(*n, cx));
        }
        Step::View(id) => {
            let _ = main.update(cx, |view, window, cx| view.show_view(id, window, cx));
        }
        Step::SelectAll(n) => {
            let result = main.update(cx, |view, window, cx| {
                view.select_all_in_card(*n, window, cx)
            });
            match result {
                Ok(Ok(())) => {}
                Ok(Err(why)) => report(format!("ELOQUAWK_EXPECT fail step selectall:{n} ({why})")),
                Err(_) => report(format!(
                    "ELOQUAWK_EXPECT fail step selectall:{n} (no window)"
                )),
            }
        }
        Step::Act(name) => match demo_script::demo_action(name) {
            Some(action) => {
                let _ = target.update(cx, |_, window, cx| window.dispatch_action(action, cx));
            }
            None => report(format!("ELOQUAWK_EXPECT fail step act:{name}")),
        },
        Step::Type(text) => {
            for ch in text.chars() {
                send_key(target, typed_keystroke(ch), cx);
            }
        }
        Step::Keys(keys) => {
            for key in keys {
                match Keystroke::parse(key) {
                    Ok(ks) => send_key(target, ks, cx),
                    Err(_) => report(format!("ELOQUAWK_EXPECT fail step {key}")),
                }
            }
        }
        Step::Expect { raw, cond } => {
            let result = main.update(cx, |view, window, cx| view.check(cond, window, cx));
            match result {
                Ok(Ok(())) => report(format!("ELOQUAWK_EXPECT ok {raw}")),
                Ok(Err(actual)) => report(format!("ELOQUAWK_EXPECT fail {raw} (actual {actual})")),
                Err(_) => report(format!("ELOQUAWK_EXPECT fail {raw} (actual no window)")),
            }
        }
        Step::Unknown(s) => report(format!("ELOQUAWK_EXPECT fail step {s}")),
    }
}

/// The keystroke a `type:` step sends for one character, shaped like a real key press so it
/// matches bindings the same way: a space is the `space` key and a capital letter is its
/// lowercase key with shift held. The text itself always arrives through `key_char`.
fn typed_keystroke(ch: char) -> Keystroke {
    let mut modifiers = gpui::Modifiers::default();
    let key = match ch {
        ' ' => "space".to_string(),
        c if c.is_uppercase() => {
            modifiers.shift = true;
            c.to_lowercase().to_string()
        }
        c => c.to_string(),
    };
    Keystroke {
        modifiers,
        key,
        key_char: Some(ch.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_keystrokes_look_like_key_presses() {
        let space = typed_keystroke(' ');
        assert_eq!(space.key, "space");
        assert!(!space.modifiers.shift);
        assert_eq!(space.key_char.as_deref(), Some(" "));

        let capital = typed_keystroke('Q');
        assert_eq!(capital.key, "q");
        assert!(capital.modifiers.shift);
        assert_eq!(capital.key_char.as_deref(), Some("Q"));

        let plain = typed_keystroke(',');
        assert_eq!(plain.key, ",");
        assert!(!plain.modifiers.shift);
    }
}
