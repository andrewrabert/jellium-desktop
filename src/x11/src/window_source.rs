//! Native [`WindowSource`]: the X11 backend owns the toplevel, so live
//! geometry comes from the geometry thread's state, not mpv ingest.

use jfn_platform_abi::{
    PhysicalSize, Scale, WindowExtent, WindowPos, WindowSnapshot, WindowSource,
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
        let m = crate::x11_state::parent_snapshot();
        let extent = (m.width > 0 && m.height > 0).then(|| {
            WindowExtent::new(
                PhysicalSize {
                    w: m.width,
                    h: m.height,
                },
                Scale(m.scale),
            )
        });
        WindowSnapshot {
            extent,
            position: Some(WindowPos {
                x: m.origin_x,
                y: m.origin_y,
            }),
            maximized: m.maximized,
            fullscreen: m.fullscreen,
        }
    }
}
