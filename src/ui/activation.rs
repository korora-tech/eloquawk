//! Raising a window with an xdg-activation token that a later launch handed over.
//!
//! On Wayland a client may only take focus with a token the compositor issued for a user
//! action (a launcher, a key binding, a clicked link). Such a token arrives in the launching
//! process's `XDG_ACTIVATION_TOKEN`; when that process is a second Eloquawk, it passes the
//! token over the instance socket, and the running instance spends it here on its own window.
//! gpui only asks for a fresh token of its own (which compositors refuse without recent
//! input), so this talks to gpui's Wayland connection directly: it wraps the same
//! `wl_display` and `wl_surface` through their raw handles, binds `xdg_activation_v1` on a
//! private event queue and sends `activate(token, surface)`.
//!
//! The unsafe wrapping depends on gpui_linux (at the zed rev pinned in `Cargo.toml`) using
//! `wayland-backend` 0.3 with `client_system`, so that our `Backend::from_foreign_display`
//! and gpui's connection are the same libwayland-client objects. Recheck this when the gpui
//! rev moves. Each call does a registry roundtrip and leaves one `wl_registry` proxy behind;
//! hand-offs are rare user actions, so nothing is cached.

use gpui::Window;
use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use wayland_backend::client::{Backend, ObjectId};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_registry, wl_surface::WlSurface};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::xdg::activation::v1::client::xdg_activation_v1::XdgActivationV1;

use crate::instance::valid_token;

struct State;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

wayland_client::delegate_noop!(State: ignore XdgActivationV1);

/// Activates `window` with `token`. Errors when the window is not a Wayland one, the
/// compositor has no `xdg_activation_v1`, or the connection fails; the caller then falls back
/// to gpui's own request. Whether the compositor honours the token is up to it.
pub fn activate(window: &Window, token: &str) -> Result<(), String> {
    if !valid_token(token) {
        return Err("not an activation token".into());
    }
    let display = match window.display_handle().map(|h| h.as_raw()) {
        Ok(RawDisplayHandle::Wayland(h)) => h.display,
        Ok(_) => return Err("not a Wayland window".into()),
        Err(e) => return Err(format!("no display handle: {e}")),
    };
    let surface = match HasWindowHandle::window_handle(window).map(|h| h.as_raw()) {
        Ok(RawWindowHandle::Wayland(h)) => h.surface,
        Ok(_) => return Err("not a Wayland window".into()),
        Err(e) => return Err(format!("no window handle: {e}")),
    };
    // SAFETY: both pointers come from gpui's live Wayland connection and window, which
    // outlive this call; a foreign display is not disconnected when the backend is dropped.
    let backend = unsafe { Backend::from_foreign_display(display.as_ptr().cast()) };
    let conn = Connection::from_backend(backend);
    // SAFETY: `surface` is the window's `wl_surface` proxy on that same display.
    let surface_id = unsafe { ObjectId::from_ptr(WlSurface::interface(), surface.as_ptr().cast()) }
        .map_err(|_| "the window's surface is not usable".to_string())?;
    let surface = WlSurface::from_id(&conn, surface_id)
        .map_err(|_| "the window's surface is not usable".to_string())?;
    let (globals, mut queue) =
        registry_queue_init::<State>(&conn).map_err(|e| format!("no registry: {e}"))?;
    let activation: XdgActivationV1 = globals
        .bind(&queue.handle(), 1..=1, ())
        .map_err(|_| "the compositor has no xdg-activation".to_string())?;
    activation.activate(token.to_string(), &surface);
    activation.destroy();
    queue
        .roundtrip(&mut State)
        .map_err(|e| format!("the compositor did not answer: {e}"))?;
    Ok(())
}
