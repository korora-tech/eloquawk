//! Eloquawk entry point: parses arguments, claims the single instance (or hands a `show` or
//! `mailto:` request to the running one and exits), starts the worker, wraps the [`Store`] in
//! a GPUI entity, pumps worker events into it, applies the theme, opens the main window,
//! notifies about new mail, opens compose windows for `mailto:` links and, in demo mode, runs
//! a `--demo-actions` script. Quits when the last window closes, telling the worker to shut
//! down.

mod branding;
mod cli;
mod config;
mod demo;
mod demo_data;
mod demo_script;
mod google;
mod instance;
mod mail;
mod model;
mod notify;
mod protocol;
mod secrets;
mod store;
mod sync;
mod ui;

use std::cell::RefCell;
use std::io::Write as _;
use std::rc::Rc;
use std::time::Duration;

use gpui::{AnyWindowHandle, App, AppContext, Entity, Keystroke, QuitMode};
use gpui_platform::application;

use demo_script::{Cond, FocusTarget, Step};
use protocol::{Command, SyncEvent};
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

    // Before the keyring or the worker: either another Eloquawk takes the request, or this one
    // becomes the instance. A demo stays out unless asked (`--instance`), and then uses its
    // own names.
    let listener = if args.claims_instance() {
        let request = instance::Request::for_launch(args.mailto.as_deref());
        match instance::claim(args.demo, &request) {
            instance::Claim::Bound(l) => Some(l),
            instance::Claim::HandedOff => return,
            instance::Claim::Unavailable(why) => {
                eprintln!("eloquawk: running without single instance ({why})");
                None
            }
        }
    } else {
        None
    };

    let mode = if args.demo { Mode::Demo } else { Mode::Google };
    let mut prefs = config::Prefs::load();
    prefs.ephemeral = mode == Mode::Demo;
    let (sync, events) = if mode == Mode::Demo {
        match demo::start(&prefs) {
            Ok(worker) => worker,
            Err(e) => {
                eprintln!("eloquawk: could not start the demo worker: {e}");
                std::process::exit(1);
            }
        }
    } else {
        match sync::start_google(secrets::Secrets::from_env(), &prefs) {
            Ok(worker) => worker,
            Err(e) => {
                eprintln!("eloquawk: could not start the sync worker: {e}");
                std::process::exit(1);
            }
        }
    };
    let script = args.demo_actions.as_deref().map(demo_script::parse_steps);
    let theme_override = args.theme;
    let launch_mailto = args.mailto;

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
            {
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

            // mailto: links waiting for an account to compose from: this launch's own, and any
            // handed over by later launches.
            let mailtos: Mailtos = Rc::new(RefCell::new(Waiting {
                links: launch_mailto.into_iter().map(|url| (url, None)).collect(),
                announced: false,
            }));

            // New mail → desktop notifications, and pending mailto links once an account is
            // known. Store notifications are frequent; both queues are usually empty.
            {
                let mailtos = mailtos.clone();
                let mut seen = notify::Seen::new();
                cx.observe(&store, move |store, cx| {
                    let threads = store.update(cx, |s, _| s.take_new_mail());
                    if !threads.is_empty() {
                        for note in notify::new_mail_notes(&threads, &mut seen) {
                            // notify-rust blocks on D-Bus; keep it off the UI thread.
                            cx.background_spawn(async move { notify::show(note) })
                                .detach();
                        }
                    }
                    if !mailtos.borrow().links.is_empty() {
                        compose_mailtos(&store, &mailtos, cx);
                    }
                })
                .detach();
            }

            // Later launches → this instance.
            let server = listener.map(|listener| {
                let (tx, rx) = async_channel::unbounded();
                let store = store.clone();
                let mailtos = mailtos.clone();
                cx.spawn(async move |cx| {
                    while let Ok(instance::Handoff { request, token }) = rx.recv().await {
                        cx.update(|cx| match request {
                            instance::Request::Show => {
                                ui::windows::raise_main_window(&store, token.as_deref(), cx)
                            }
                            instance::Request::Compose(url) => {
                                {
                                    let mut waiting = mailtos.borrow_mut();
                                    waiting.links.push((url, token.clone()));
                                    waiting.announced = false;
                                }
                                if !compose_mailtos(&store, &mailtos, cx) {
                                    // No account yet: bring up the window that says why.
                                    ui::windows::raise_main_window(&store, token.as_deref(), cx);
                                }
                            }
                        });
                    }
                })
                .detach();
                listener.serve(tx)
            });

            // Edited compose windows ask before the app quits.
            cx.on_action(|_: &ui::Quit, cx| cx.defer(ui::compose::request_quit));
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            cx.on_app_quit(move |_| {
                if let Some(server) = &server {
                    server.shutdown();
                }
                sync.send(Command::Shutdown);
                async {}
            })
            .detach();
            ui::windows::show_main_window(&store, cx);
            compose_mailtos(&store, &mailtos, cx);

            if let Some(steps) = script {
                run_script(steps, store, first_page_rx, cx);
            }
        });
}

/// `mailto:` links waiting to be composed, each with the activation token of the launch that
/// handed it over.
struct Waiting {
    links: Vec<(String, Option<String>)>,
    /// The "sign in" toast was shown for these links.
    announced: bool,
}

type Mailtos = Rc<RefCell<Waiting>>;

/// The toast for a `mailto:` link that arrives before there is an account to send from.
const MAILTO_NEEDS_ACCOUNT: &str = "Sign in to compose this message";

/// Opens a compose window for each waiting `mailto:` link, from the current account, once
/// there is one. The links go through the whitelisted parser only: no attachments, sender or
/// threading from the link, and nothing is sent until the user presses Send.
///
/// Returns false when links are left waiting for an account. Once the store knows there is
/// none (auth no longer `Starting`) that is said with a toast, once per arrival: posting it
/// notifies the store, which calls this again from the observer.
fn compose_mailtos(store: &Entity<Store>, mailtos: &Mailtos, cx: &mut App) -> bool {
    let Some(account) = store.read(cx).current_account() else {
        let mut waiting = mailtos.borrow_mut();
        if waiting.links.is_empty() {
            return true;
        }
        let settled = !matches!(store.read(cx).auth, protocol::AuthState::Starting);
        if settled && !waiting.announced {
            waiting.announced = true;
            drop(waiting);
            store.update(cx, |s, cx| {
                s.inform(MAILTO_NEEDS_ACCOUNT);
                cx.notify();
            });
        }
        return false;
    };
    let urls = {
        let mut waiting = mailtos.borrow_mut();
        waiting.announced = false;
        std::mem::take(&mut waiting.links)
    };
    for (url, token) in urls {
        let from = model::Address::new(None, &account);
        match mail::mailto::parse(&url, &account, from) {
            Some(prefill) => {
                ui::windows::open_compose_window_raised(store, prefill, token.as_deref(), cx)
            }
            None => eprintln!("eloquawk: ignored a link that is not a mailto URL"),
        }
    }
    true
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
    // The focused window. When none is (an idle or locked niri session gives new windows no
    // focus), the newest compose window stands in for the one a compositor would focus, and
    // otherwise the main window.
    let target: AnyWindowHandle = cx
        .active_window()
        .or_else(|| ui::compose::newest_compose_window(cx))
        .unwrap_or_else(|| main.into());
    match step {
        Step::Wait(_) => {}
        Step::Shot => {
            // gpui's Wayland and X11 windows report an empty title, so the main window's
            // known title stands in.
            let title = target
                .update(cx, |_, window, _| window.window_title())
                .unwrap_or_default();
            let main_any: AnyWindowHandle = main.into();
            let title = if !title.is_empty() {
                title
            } else if let Some(view) = ui::compose::compose_view(target, cx) {
                view.read(cx).title().to_string()
            } else if target == main_any {
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
        Step::Search(query) => {
            let _ = main.update(cx, |view, window, cx| view.search_for(query, window, cx));
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
        Step::Attach(path) => match ui::compose::compose_view(target, cx) {
            Some(view) => view.update(cx, |v, cx| {
                v.attach_paths(vec![std::path::PathBuf::from(path)], cx)
            }),
            None => report(format!(
                "ELOQUAWK_EXPECT fail step attach:{path} (no compose window)"
            )),
        },
        Step::Save(n) => match main.read_with(cx, |view, cx| view.save_attachment(*n, cx)) {
            Ok(Ok(())) => {}
            Ok(Err(why)) => report(format!("ELOQUAWK_EXPECT fail step save:{n} ({why})")),
            Err(_) => report(format!("ELOQUAWK_EXPECT fail step save:{n} (no window)")),
        },
        Step::Expect { raw, cond } => {
            // A focused compose window answers the focus and its own conditions; the main
            // window answers the rest.
            let from_compose = ui::compose::compose_view(target, cx).and_then(|view| {
                target
                    .update(cx, |_, window, cx| match cond {
                        Cond::Focus(want) => Some(if *want == FocusTarget::Compose {
                            Ok(())
                        } else {
                            Err("compose".to_string())
                        }),
                        _ => view.read(cx).check(cond, window, cx),
                    })
                    .ok()
                    .flatten()
            });
            let result = match from_compose {
                Some(r) => Ok(r),
                None => main.update(cx, |view, window, cx| view.check(cond, window, cx)),
            };
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
