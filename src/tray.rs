//! Background mode: a StatusNotifierItem tray icon that lets Eloquawk keep running, and keep
//! notifying about new mail, after its last window closes.
//!
//! The tray lives on its own threads (ksni over zbus). Its clicks arrive as [`Event`]s on a
//! channel that `main` drains on the UI thread. The app only stays alive with no windows
//! while the icon is actually showing ([`Tray::visible`]): on a desktop with no
//! StatusNotifier host, closing the last window still quits, because a running app with no
//! window and no icon could not be reached or stopped.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use ksni::blocking::{Handle, TrayMethods as _};
use ksni::menu::StandardItem;
use ksni::{MenuItem, OfflineReason};

use crate::branding::APP_ID;

/// What the user asked of the tray.
#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    /// Open or raise the main window, with the activation token the desktop handed over.
    Show(Option<String>),
    /// Quit for real.
    Quit,
    /// The tray icon went away while the app may have no window: the user needs a way back.
    Gone,
}

/// A running tray icon. Dropping it removes the icon.
pub struct Tray {
    visible: Arc<AtomicBool>,
    handle: Handle<Item>,
}

impl Tray {
    /// Registers the icon. Blocks on D-Bus, so call it off the UI thread. Fails when there is
    /// no session bus, no StatusNotifier watcher, or no host to draw the icon.
    pub fn start(events: async_channel::Sender<Event>) -> Result<Tray, ksni::Error> {
        let visible = Arc::new(AtomicBool::new(true));
        let handle = Item {
            events,
            visible: visible.clone(),
        }
        .spawn()?;
        Ok(Tray { visible, handle })
    }

    /// Whether the icon is showing now, so the user can reopen or quit the app.
    pub fn visible(&self) -> bool {
        self.visible.load(Ordering::Relaxed) && !self.handle.is_closed()
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        let _ = self.handle.shutdown();
    }
}

struct Item {
    events: async_channel::Sender<Event>,
    visible: Arc<AtomicBool>,
}

impl Item {
    fn send(&self, event: Event) {
        let _ = self.events.try_send(event);
    }
}

/// The activation token a click carries, which may arrive just after the click itself.
fn activation_token() -> Option<String> {
    for _ in 0..3 {
        if let Some(token) = ksni::current_xdg_activation_token() {
            return Some(token);
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    None
}

impl ksni::Tray for Item {
    fn id(&self) -> String {
        APP_ID.into()
    }

    fn title(&self) -> String {
        "Eloquawk".into()
    }

    fn icon_name(&self) -> String {
        APP_ID.into()
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.send(Event::Show(activation_token()));
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        vec![
            StandardItem {
                label: "Open Eloquawk".into(),
                activate: Box::new(|this: &mut Self| this.send(Event::Show(activation_token()))),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|this: &mut Self| this.send(Event::Quit)),
                ..Default::default()
            }
            .into(),
        ]
    }

    fn watcher_online(&self) {
        self.visible.store(true, Ordering::Relaxed);
    }

    fn watcher_offline(&self, _reason: OfflineReason) -> bool {
        self.visible.store(false, Ordering::Relaxed);
        self.send(Event::Gone);
        // Keep the service: the icon comes back with the watcher.
        true
    }
}

/// What to do when the last window has closed.
pub fn stays_running(run_in_background: bool, tray_visible: bool) -> bool {
    run_in_background && tray_visible
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_visible_tray_keeps_the_app_running() {
        assert!(stays_running(true, true));
        assert!(
            !stays_running(true, false),
            "no icon: nobody could reopen it"
        );
        assert!(!stays_running(false, true), "the user turned it off");
        assert!(!stays_running(false, false));
    }
}
