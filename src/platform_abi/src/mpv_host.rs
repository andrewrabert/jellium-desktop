//! How the platform hosts mpv: pre-create environment, host-window
//! readiness, the VO wait loop, and severing host links at teardown.
//!
//! No mpv types appear here — shared code owns all mpv event handling via
//! the `pump` closure, and the platform owns only the wait strategy.

use std::time::Duration;

use crate::WindowDecorations;

/// Longest a VO wait may park before it re-reads the readiness gate.
pub const VO_WAIT_TICK: Duration = Duration::from_millis(250);

/// Platform side of mpv's lifecycle. Defaults cover backends where mpv
/// needs no host preparation and the generic blocking wait suffices.
pub trait MpvHost: Send + Sync {
    /// Prepare the process environment for mpv. Runs before `mpv_create`;
    /// position-critical setup (window-ownership proxies, env vars mpv
    /// reads during init) belongs here. `configured` is the user's explicit
    /// decoration preference; `None` leaves the choice to the platform.
    fn prepare(&self, _configured: Option<WindowDecorations>) {}

    /// Whether the host window state mpv's VO depends on (scale, first
    /// configure) is known. Gates VO-startup completion — not VO state
    /// itself, which mpv owns.
    fn host_ready(&self) -> bool {
        true
    }

    fn ensure_host_window(&self) {}

    /// Native window ID mpv should embed into (its `wid` option), or `None`
    /// when mpv creates its own window. Hosts that return `Some` must have
    /// created the window in [`Self::ensure_host_window`], which runs first.
    fn embed_wid(&self) -> Option<i64> {
        None
    }

    /// Own the VO wait loop. `pump(budget)` drains every queued mpv event,
    /// re-reads the readiness gate, and returns `false` once the wait is
    /// over. A non-zero `budget` parks inside mpv for at most that long
    /// after the drain; [`Duration::ZERO`] drains and returns. Platforms
    /// holding a native run loop pass `Duration::ZERO` and block on their
    /// own loop for at most [`VO_WAIT_TICK`].
    fn run_vo_wait(&self, pump: &mut dyn FnMut(Duration) -> bool) {
        while pump(VO_WAIT_TICK) {}
    }

    /// Logical content size of the host window in points, when the OS —
    /// not mpv's osd-dimensions — is the authority for it.
    fn logical_content_size(&self) -> Option<(i32, i32)> {
        None
    }

    /// Sever host↔mpv links that could deadlock teardown. Called
    /// immediately before CEF teardown.
    fn detach(&self) {}
}

/// All-default host for backends where mpv needs nothing from the
/// platform (macOS / Windows: mpv owns its window outright).
pub struct DefaultMpvHost;

impl MpvHost for DefaultMpvHost {}
