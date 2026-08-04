use std::ffi::c_void;
use std::os::unix::net::UnixStream;
use std::ptr::NonNull;

use parking_lot::Mutex;
use wayland_backend::client::Backend;

/// The app-side `wl_display`. Sharing the raw pointer across threads is sound:
/// libwayland's display is internally synchronized and the app already drives it
/// from the root thread. Callers take the raw pointer only at the FFI boundary
/// via [`AppDisplay::as_ptr`], keeping the ownership contract in the type.
#[derive(Clone, Copy)]
pub(crate) struct AppDisplay(NonNull<c_void>);
unsafe impl Send for AppDisplay {}
unsafe impl Sync for AppDisplay {}

impl AppDisplay {
    pub(crate) fn as_ptr(self) -> *mut c_void {
        self.0.as_ptr()
    }
}

/// `Unattempted` retries on the next call; `Failed` does not. Only a missing
/// fd (proxy hasn't published the client fd yet) stays `Unattempted` — once
/// `Backend::connect` runs it has consumed the fd, so its result is terminal
/// either way.
enum DisplayState {
    Unattempted,
    /// The backend owns the connection for the process lifetime; dropping it
    /// would disconnect the display CEF and mpv hold pointers to.
    Connected {
        display: AppDisplay,
        _backend: Backend,
    },
    Failed,
}

pub(crate) struct AppConn {
    display: Mutex<DisplayState>,
}

impl AppConn {
    pub(crate) fn new() -> Self {
        Self {
            display: Mutex::new(DisplayState::Unattempted),
        }
    }
}

pub(crate) fn app_display(rt: &crate::runtime::WlRuntime) -> Option<AppDisplay> {
    let mut state = rt.app_conn().display.lock();
    match &*state {
        DisplayState::Connected { display, .. } => return Some(*display),
        DisplayState::Failed => return None,
        DisplayState::Unattempted => {}
    }
    let Some(fd) = rt.proxy().take_app_client_fd() else {
        tracing::error!(target: "Main", "app_display: no app client fd available");
        return None;
    };
    let backend = match Backend::connect(UnixStream::from(fd)) {
        Ok(backend) => backend,
        Err(e) => {
            tracing::error!(target: "Main", "app_display: {e}");
            *state = DisplayState::Failed;
            return None;
        }
    };
    let Some(d) = NonNull::new(backend.display_ptr().cast::<c_void>()) else {
        tracing::error!(target: "Main", "app_display: null wl_display");
        *state = DisplayState::Failed;
        return None;
    };
    tracing::info!(target: "Main", "app_display: connected -> {:p}", d.as_ptr());
    let display = AppDisplay(d);
    *state = DisplayState::Connected {
        display,
        _backend: backend,
    };
    Some(display)
}
