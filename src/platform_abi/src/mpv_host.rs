//! How the platform hosts mpv: pre-create environment, host-window
//! readiness, the VO wait loop, and severing host links at teardown.
//!
//! No mpv types appear here — shared code owns all mpv event handling via
//! the `pump` closure, and the platform owns only the wait strategy.

use crate::WindowDecorations;

/// Whether the boot event pump may block after checking readiness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VoWait {
    /// Service queued events without blocking a native run loop.
    Drain,
    /// Wait for an mpv event or an explicit host-readiness wakeup.
    Event,
}

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

    /// Own the VO wait loop. The pump drains queued mpv events, checks all
    /// readiness gates, and returns false when startup is finished. `Event`
    /// waits without a timer; host state changes must wake mpv after publishing
    /// their state. Native run-loop owners use `Drain` and wait on their own
    /// event source instead.
    fn run_vo_wait(&self, pump: &mut dyn FnMut(VoWait) -> bool) {
        while pump(VoWait::Event) {}
    }

    /// The host window's logical content size, or `None` when mpv's
    /// `osd-dimensions`, not the OS, is the authority for it here.
    fn logical_content_size(&self) -> Option<crate::geometry::LogicalSize>;

    /// Sever host↔mpv links that could deadlock teardown. Called
    /// immediately before CEF teardown.
    fn detach(&self) {}
}

/// All-default host for backends where mpv needs nothing from the
/// platform (macOS / Windows: mpv owns its window outright).
pub struct DefaultMpvHost;

impl MpvHost for DefaultMpvHost {
    // mpv's own window: `osd-dimensions` is the authority for its size
    fn logical_content_size(&self) -> Option<crate::geometry::LogicalSize> {
        None
    }
}
