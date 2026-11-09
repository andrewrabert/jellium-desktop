//! Wayland proxy between mpv and the compositor.
//!
//! mpv connects here instead of the real compositor (via WAYLAND_DISPLAY env).
//! Messages forward in both directions; selected requests are intercepted.
//!
//! We don't use `SimpleProxy` because it builds each per-client `State` using
//! the current process `WAYLAND_DISPLAY` env to find the upstream compositor —
//! but the caller overrides that env to OUR socket so mpv connects to us. We
//! must capture the original `WAYLAND_DISPLAY` here at `start` (before any
//! override) and pass it explicitly via `with_server_display_name`.

mod app;
mod mpv;

use std::ffi::{CStr, CString};
use std::os::fd::IntoRawFd;
use std::rc::Rc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::thread;

use error_reporter::Report;
use parking_lot::Mutex;
use wl_proxy::client::ClientHandler;
use wl_proxy::protocols::fractional_scale_v1::wp_fractional_scale_manager_v1::{
    WpFractionalScaleManagerV1, WpFractionalScaleManagerV1Handler,
};
use wl_proxy::protocols::fractional_scale_v1::wp_fractional_scale_v1::{
    WpFractionalScaleV1, WpFractionalScaleV1Handler,
};
use wl_proxy::protocols::wayland::wl_surface::WlSurface;

use crate::runtime::WlRuntime;
use crate::window_state::WindowSize;

use self::app::{AppStartup, run_app_state};

pub struct Proxy {
    display_name: CString,
    _app_thread: thread::JoinHandle<()>,
}

impl Proxy {
    /// The `WAYLAND_DISPLAY` value clients should connect to (e.g. "wayland-1").
    pub(crate) fn display_name(&self) -> &CStr {
        &self.display_name
    }

    /// The listener threads are detached; the OS cleans up on process exit.
    fn stop(&self) {}
}

/// The authoritative window size and the generation it was published at, in
/// one lock: a reader can't pair a width with another generation's height.
#[derive(Clone, Copy)]
struct PublishedSize {
    size: WindowSize,
    generation: u32,
}

/// What the proxy's two threads share with the rest of the process.
pub(crate) struct ProxyShared {
    /// `None` until the host toplevel has been configured — there is no
    /// boot/default to fall back to, so mpv only ever mirrors real geometry.
    window: Mutex<Option<PublishedSize>>,
    // S_mpv records this from `server_id()`; S_app matches it against
    // `client_id()` on client M. Same wire object => the two ids are equal.
    mpv_video_surface_id: AtomicU32,
    /// Set until the proxy thread has placed the video subsurface at the bottom
    /// of the host root's stack. Sticky, so a request raised before the splice
    /// is honored by the splice itself.
    pin_video_pending: AtomicBool,
    app_client_fd: AtomicI32,
    mpv_wake: Mutex<Option<calloop::ping::Ping>>,
    proxy: OnceLock<Proxy>,
}

impl ProxyShared {
    pub(crate) fn new() -> Self {
        Self {
            window: Mutex::new(None),
            mpv_video_surface_id: AtomicU32::new(0),
            pin_video_pending: AtomicBool::new(false),
            app_client_fd: AtomicI32::new(-1),
            mpv_wake: Mutex::new(None),
            proxy: OnceLock::new(),
        }
    }

    /// Takes the published fd and leaves the slot empty; a second call is
    /// `None`.
    pub(crate) fn take_app_client_fd(&self) -> Option<std::os::fd::OwnedFd> {
        let fd = self.app_client_fd.swap(-1, Ordering::AcqRel);
        // SAFETY: the swap hands this fd to exactly one caller, and nothing
        // else in the process holds it after publication.
        (fd >= 0).then(|| unsafe { std::os::fd::FromRawFd::from_raw_fd(fd) })
    }

    /// Asks the proxy thread to place mpv's video subsurface directly above the
    /// host root; the placement applies on the parent's next commit.
    ///
    /// Order-independent: raised before the splice, the splice's own placement
    /// discharges it; raised after, the proxy thread's next turn does.
    pub(crate) fn pin_video_bottom(&self) {
        self.pin_video_pending.store(true, Ordering::Release);
    }

    /// Claims a pending pin, leaving the flag clear only for the caller that
    /// takes it.
    fn take_pin_video_bottom(&self) -> bool {
        self.pin_video_pending.swap(false, Ordering::AcqRel)
    }

    fn window_size(&self) -> Option<WindowSize> {
        self.window.lock().map(|p| p.size)
    }

    /// The published size, or `None` when nothing newer than `seen` exists.
    fn window_size_since(&self, seen: u32) -> Option<PublishedSize> {
        self.window.lock().filter(|p| p.generation != seen)
    }

    pub(crate) fn set_window_size(&self, size: WindowSize) {
        {
            let mut cur = self.window.lock();
            let generation = cur.map_or(1, |p| p.generation.wrapping_add(1));
            *cur = Some(PublishedSize { size, generation });
        }
        self.wake_mpv_thread();
    }

    fn wake_mpv_thread(&self) {
        if let Some(ping) = self.mpv_wake.lock().as_ref() {
            ping.ping();
        }
    }

    /// Publish the running proxy. Returns `Err` if one was already set.
    pub(crate) fn set_proxy(&self, proxy: Proxy) -> Result<&Proxy, ()> {
        self.proxy.set(proxy).map_err(|_| ())?;
        self.proxy.get().ok_or(())
    }

    pub(crate) fn stop(&self) {
        if let Some(proxy) = self.proxy.get() {
            proxy.stop();
        }
    }
}

/// Forwarding sends can't unwind a handler; a failure desyncs a single message
/// but is unrecoverable in place, so surface it through our infra and continue.
fn log_send(op: &str, res: Result<(), wl_proxy::object::ObjectError>) {
    if let Err(e) = res {
        tracing::warn!(target: "MpvProxy", "{op}: {}", Report::new(&e));
    }
}

pub fn start(rt: &'static WlRuntime) -> Option<Proxy> {
    // Capture upstream BEFORE the caller overrides WAYLAND_DISPLAY, so S_app
    // connects to the real compositor rather than our own socket.
    let upstream = std::env::var("WAYLAND_DISPLAY").ok();

    let (tx_app, rx_app) = crossbeam_channel::bounded::<Result<AppStartup, String>>(1);
    let app_thread = match thread::Builder::new()
        .name("proxy-app".into())
        .spawn(move || run_app_state(rt, tx_app, upstream))
    {
        Ok(h) => h,
        Err(e) => {
            eprintln!("proxy: app thread spawn failed: {e}");
            return None;
        }
    };
    let startup = match rx_app.recv() {
        Ok(Ok(startup)) => startup,
        Ok(Err(msg)) => {
            eprintln!("proxy: {msg}");
            return None;
        }
        Err(_) => {
            eprintln!("proxy: app thread exited before publishing startup state");
            return None;
        }
    };
    rt.proxy()
        .app_client_fd
        .store(startup.app_fd.into_raw_fd(), Ordering::Release);
    Some(Proxy {
        display_name: startup.display_name,
        _app_thread: app_thread,
    })
}

struct NoopClient;
impl ClientHandler for NoopClient {
    fn disconnected(self: Box<Self>) {}
}

struct FracScaleMgrH;
impl WpFractionalScaleManagerV1Handler for FracScaleMgrH {
    fn handle_get_fractional_scale(
        &mut self,
        slf: &Rc<WpFractionalScaleManagerV1>,
        id: &Rc<WpFractionalScaleV1>,
        surface: &Rc<WlSurface>,
    ) {
        id.set_handler(FracScaleH);
        log_send(
            "wp_fractional_scale_manager_v1.get_fractional_scale",
            slf.try_send_get_fractional_scale(id, surface),
        );
    }
}

struct FracScaleH;
impl WpFractionalScaleV1Handler for FracScaleH {
    fn handle_preferred_scale(&mut self, slf: &Rc<WpFractionalScaleV1>, scale: u32) {
        log_send(
            "wp_fractional_scale_v1.preferred_scale",
            slf.try_send_preferred_scale(scale),
        );
    }
}
