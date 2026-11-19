//! Native [`WindowSource`]: the Wayland backend owns the toplevel, so live
//! geometry comes from compositor state, not mpv ingest.

use jfn_platform_abi::{
    AppCreatedWindow, BootGeometry, WindowSource, WindowState, WindowStateError,
};

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
    fn window_state(&self) -> Result<WindowState, WindowStateError> {
        let (published, basis) = self.rt.window().state();
        let Some(published) = published else {
            return Err(WindowStateError::NoWindow);
        };
        let Some(web_top) = published.web_top() else {
            return Err(WindowStateError::WebOffsetUnstated);
        };
        let extent =
            crate::scale::extent(published.logical(), published.physical(), published.scale())?;
        Ok(WindowState {
            extent,
            position: None,
            maximized: published.mode() == crate::window_state::WindowMode::Maximized,
            fullscreen: published.mode() == crate::window_state::WindowMode::Fullscreen,
            web_top,
            basis,
        })
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
