//! Native [`WindowSource`]: the X11 backend owns the toplevel, so live
//! geometry comes from the geometry thread's state, not mpv ingest.

use jfn_platform_abi::{
    AppCreatedWindow, BootGeometry, PhysicalSize, WindowPos, WindowSource, WindowState,
    WindowStateError,
};

pub struct X11WindowSource;

pub static X11_WINDOW_SOURCE: X11WindowSource = X11WindowSource;

impl WindowSource for X11WindowSource {
    fn window_state(&self) -> Result<WindowState, WindowStateError> {
        if crate::x11_state::host().is_none() {
            return Err(WindowStateError::NoWindow);
        }
        let Some(parent) = crate::x11_state::published_parent() else {
            return Err(WindowStateError::NoWindow);
        };
        let extent = crate::scale::extent(
            PhysicalSize {
                w: parent.width,
                h: parent.height,
            },
            parent.scale,
        )?;
        Ok(WindowState {
            extent,
            position: Some(WindowPos {
                x: parent.origin_x,
                y: parent.origin_y,
            }),
            maximized: parent.maximized,
            fullscreen: parent.fullscreen,
            web_top: 0,
            basis: jfn_platform_abi::WindowBasis::FIXED,
        })
    }
}

impl AppCreatedWindow for X11WindowSource {
    /// `ensure_host_window` creates the WM toplevel at this geometry, so the
    /// seed must land before it runs.
    fn seed_boot_geometry(&self, g: &BootGeometry) {
        crate::lifecycle::set_boot_geometry(g.clone());
    }
}
