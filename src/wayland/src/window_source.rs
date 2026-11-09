//! Native [`WindowSource`]: the Wayland backend owns the toplevel, so live
//! geometry comes from compositor state, not mpv ingest.

use jfn_platform_abi::{AppCreatedWindow, BootGeometry, WindowSnapshot, WindowSource};

use crate::runtime::WlRuntime;

pub struct WaylandWindowSource {
    rt: &'static WlRuntime,
}

impl WaylandWindowSource {
    pub(crate) fn new(rt: &'static WlRuntime) -> Self {
        Self { rt }
    }
}

impl WindowSource for WaylandWindowSource {
    fn snapshot(&self) -> WindowSnapshot {
        // One snapshot so extent and mode can't span two generations.
        let snap = self.rt.window().window_extent();
        WindowSnapshot {
            extent: snap
                .as_ref()
                .and_then(|s| crate::scale::extent(s.logical(), s.physical(), s.scale())),
            position: None,
            maximized: snap.is_some_and(|e| e.mode() == crate::window_state::WindowMode::Maximized),
            fullscreen: snap
                .is_some_and(|e| e.mode() == crate::window_state::WindowMode::Fullscreen),
        }
    }
}

impl AppCreatedWindow for WaylandWindowSource {
    /// Only the app window's own geometry uses the boot size; mpv mirrors the
    /// committed window geometry and never the boot guess.
    fn seed_boot_geometry(&self, g: &BootGeometry) {
        self.rt
            .root()
            .set_boot_geometry(g.logical().w, g.logical().h, g.maximized());
    }
}
