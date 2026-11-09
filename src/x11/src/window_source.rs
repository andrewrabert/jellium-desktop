//! Native [`WindowSource`]: the X11 backend owns the toplevel, so live
//! geometry comes from the geometry thread's state, not mpv ingest.

use jfn_platform_abi::{
    AppCreatedWindow, BootGeometry, PhysicalSize, WindowPos, WindowSnapshot, WindowSource,
};

pub struct X11WindowSource;

pub static X11_WINDOW_SOURCE: X11WindowSource = X11WindowSource;

impl WindowSource for X11WindowSource {
    fn snapshot(&self) -> WindowSnapshot {
        if crate::x11_state::host().is_none() {
            return WindowSnapshot {
                extent: None,
                position: None,
                maximized: false,
                fullscreen: false,
            };
        }
        let Some(m) = crate::x11_state::parent_snapshot() else {
            return WindowSnapshot {
                extent: None,
                position: None,
                maximized: false,
                fullscreen: false,
            };
        };
        WindowSnapshot {
            extent: crate::scale::extent(
                PhysicalSize {
                    w: m.width,
                    h: m.height,
                },
                m.scale,
            ),
            position: Some(WindowPos {
                x: m.origin_x,
                y: m.origin_y,
            }),
            maximized: m.maximized,
            fullscreen: m.fullscreen,
        }
    }
}

impl AppCreatedWindow for X11WindowSource {
    /// `ensure_host_window` creates the WM toplevel at this geometry, so the
    /// seed must land before it runs.
    fn seed_boot_geometry(&self, g: &BootGeometry) {
        crate::lifecycle::set_boot_geometry(*g);
    }
}
