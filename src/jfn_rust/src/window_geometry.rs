use std::sync::OnceLock;

use jfn_platform_abi::{
    BootGeometry, LogicalSize, PhysicalSize, Platform, Scale, WindowGeometry, WindowSource,
};

use jfn_config::JfnWindowGeometry;

const DEFAULT_LOGICAL: LogicalSize = LogicalSize { w: 1600, h: 900 };

fn plat() -> &'static dyn Platform {
    unsafe { jfn_platform_abi::get() }
}

pub struct WindowGeometryController {
    source: &'static dyn WindowSource,
}

impl WindowGeometryController {
    fn new() -> Self {
        Self {
            source: plat().window_owner().source(),
        }
    }

    pub fn source(&self) -> &dyn WindowSource {
        self.source
    }

    pub fn boot(&self) -> Option<BootGeometry> {
        let g = jfn_config::window_geometry();
        let at = (g.x >= 0 && g.y >= 0).then_some(jfn_platform_abi::WindowPos { x: g.x, y: g.y });
        let scale = plat().display_scale(at);
        resolve_boot(g, scale, |w| plat().clamp_window_geometry(w))
    }

    pub fn persist(&self) {
        let was_max_before_fs =
            jfn_playback::browser_sink::jfn_playback_was_maximized_before_fullscreen();
        if let Some(g) = geometry_to_persist(
            self.source(),
            jfn_config::window_geometry(),
            was_max_before_fs,
        ) {
            jfn_config::set_window_geometry(g);
        }
    }
}

fn resolve_boot(
    g: JfnWindowGeometry,
    scale: Scale,
    clamp: impl Fn(WindowGeometry) -> WindowGeometry,
) -> Option<BootGeometry> {
    let logical = if g.logical_width > 0 && g.logical_height > 0 {
        LogicalSize {
            w: g.logical_width,
            h: g.logical_height,
        }
    } else if g.width > 0 && g.height > 0 {
        LogicalSize {
            w: g.width,
            h: g.height,
        }
    } else {
        DEFAULT_LOGICAL
    };
    let physical = logical.to_physical(scale)?;
    let clamped = clamp(WindowGeometry::from_raw(physical.w, physical.h, g.x, g.y));
    Some(BootGeometry::from_clamped(
        logical,
        scale,
        clamped,
        g.maximized,
    ))
}

pub fn saved_sizes(g: &JfnWindowGeometry) -> Option<(LogicalSize, PhysicalSize)> {
    if g.logical_width <= 0 || g.logical_height <= 0 || g.width <= 0 || g.height <= 0 {
        return None;
    }
    Some((
        LogicalSize {
            w: g.logical_width,
            h: g.logical_height,
        },
        PhysicalSize {
            w: g.width,
            h: g.height,
        },
    ))
}

pub fn controller() -> &'static WindowGeometryController {
    static CONTROLLER: OnceLock<WindowGeometryController> = OnceLock::new();
    CONTROLLER.get_or_init(WindowGeometryController::new)
}

fn geometry_to_persist(
    ws: &dyn WindowSource,
    saved: JfnWindowGeometry,
    was_maximized_before_fullscreen: bool,
) -> Option<JfnWindowGeometry> {
    let snap = ws.snapshot();
    if snap.fullscreen {
        let mut g = saved;
        g.maximized = was_maximized_before_fullscreen;
        return Some(g);
    }
    if snap.maximized {
        let mut g = saved;
        g.maximized = true;
        return Some(g);
    }
    let ext = snap.extent?;
    let physical = ext.physical();
    if physical.w <= 0 || physical.h <= 0 {
        return None;
    }
    let logical = ext.logical();
    Some(JfnWindowGeometry {
        width: physical.w,
        height: physical.h,
        logical_width: logical.w,
        logical_height: logical.h,
        maximized: false,
        x: snap.position.map_or(-1, |p| p.x),
        y: snap.position.map_or(-1, |p| p.y),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use jfn_platform_abi::{WindowExtent, WindowPos, WindowSnapshot};

    struct FakeWindowSource {
        size: Option<PhysicalSize>,
        maximized: bool,
        fullscreen: bool,
        position: Option<WindowPos>,
        scale: Scale,
    }

    impl WindowSource for FakeWindowSource {
        fn snapshot(&self) -> WindowSnapshot {
            WindowSnapshot {
                extent: self.size.and_then(|physical| {
                    WindowExtent::new(physical, self.scale, physical.to_logical(self.scale)?)
                }),
                position: self.position,
                maximized: self.maximized,
                fullscreen: self.fullscreen,
            }
        }
    }

    fn fake(size: Option<PhysicalSize>, scale: Scale) -> FakeWindowSource {
        FakeWindowSource {
            size,
            maximized: false,
            fullscreen: false,
            position: None,
            scale,
        }
    }

    #[test]
    fn wayland_shaped_no_position_scaled() {
        let Some(scale) = Scale::from_f64(1.5) else {
            return;
        };
        let ws = fake(Some(PhysicalSize { w: 2400, h: 1350 }), scale);
        let g = geometry_to_persist(&ws, JfnWindowGeometry::default(), false).unwrap();
        assert_eq!((g.x, g.y), (-1, -1));
        assert_eq!((g.width, g.height), (2400, 1350));
        assert_eq!((g.logical_width, g.logical_height), (1600, 900));
        assert!(!g.maximized);
    }

    #[test]
    fn mpv_shaped_with_position() {
        let ws = FakeWindowSource {
            position: Some(WindowPos { x: 100, y: 50 }),
            ..fake(Some(PhysicalSize { w: 1280, h: 720 }), Scale::ONE)
        };
        let g = geometry_to_persist(&ws, JfnWindowGeometry::default(), false).unwrap();
        assert_eq!((g.x, g.y), (100, 50));
        assert_eq!((g.logical_width, g.logical_height), (1280, 720));
    }

    #[test]
    fn maximized_keeps_prior_size() {
        let saved = JfnWindowGeometry {
            width: 1280,
            height: 720,
            logical_width: 1280,
            logical_height: 720,
            ..Default::default()
        };
        let ws = FakeWindowSource {
            maximized: true,
            ..fake(Some(PhysicalSize { w: 300, h: 200 }), Scale::ONE)
        };
        let g = geometry_to_persist(&ws, saved, false).unwrap();
        assert!(g.maximized);
        assert_eq!((g.width, g.height), (1280, 720));
    }

    #[test]
    fn fullscreen_preserves_pre_fullscreen_state() {
        let saved = JfnWindowGeometry {
            width: 1600,
            height: 900,
            ..Default::default()
        };
        let ws = FakeWindowSource {
            maximized: true,
            fullscreen: true,
            ..fake(Some(PhysicalSize { w: 3840, h: 2160 }), Scale::ONE)
        };
        let g = geometry_to_persist(&ws, saved, true).unwrap();
        assert!(g.maximized);
        assert_eq!((g.width, g.height), (1600, 900));
    }

    #[test]
    fn unknown_size_returns_none() {
        let ws = fake(None, Scale::ONE);
        assert!(geometry_to_persist(&ws, JfnWindowGeometry::default(), false).is_none());
    }

    #[test]
    fn logical_rounding() {
        for (scale, phys, logical) in [(1.25_f64, 2000, 1600), (2.0, 3000, 1500)] {
            let Some(scale) = Scale::from_f64(scale) else {
                return;
            };
            let ws = fake(Some(PhysicalSize { w: phys, h: phys }), scale);
            let g = geometry_to_persist(&ws, JfnWindowGeometry::default(), false).unwrap();
            assert_eq!(g.logical_width, logical);
        }
    }

    #[test]
    fn saved_sizes_declines_a_geometry_missing_either_pair() {
        assert_eq!(saved_sizes(&JfnWindowGeometry::default()), None);
        assert_eq!(
            saved_sizes(&JfnWindowGeometry {
                width: 1600,
                height: 900,
                ..Default::default()
            }),
            None
        );
        assert_eq!(
            saved_sizes(&JfnWindowGeometry {
                logical_width: 1280,
                logical_height: 720,
                ..Default::default()
            }),
            None
        );
        assert_eq!(
            saved_sizes(&JfnWindowGeometry {
                width: 1600,
                height: 900,
                logical_width: 1280,
                logical_height: 720,
                ..Default::default()
            }),
            Some((
                LogicalSize { w: 1280, h: 720 },
                PhysicalSize { w: 1600, h: 900 }
            ))
        );
    }

    fn identity_clamp(w: WindowGeometry) -> WindowGeometry {
        w
    }

    #[test]
    fn boot_restores_cross_scale() {
        let saved = JfnWindowGeometry {
            logical_width: 1280,
            logical_height: 720,
            ..Default::default()
        };
        assert_eq!(
            Scale::from_f64(1.25)
                .and_then(|scale| resolve_boot(saved, scale, identity_clamp))
                .map(|boot| (boot.logical(), boot.physical(), boot.position())),
            Some((
                LogicalSize { w: 1280, h: 720 },
                PhysicalSize { w: 1600, h: 900 },
                None
            ))
        );
    }

    #[test]
    fn maximize_round_trip_preserves_size() {
        let prior = JfnWindowGeometry {
            logical_width: 1280,
            logical_height: 720,
            width: 1280,
            height: 720,
            ..Default::default()
        };
        let ws = FakeWindowSource {
            maximized: true,
            ..fake(Some(PhysicalSize { w: 3840, h: 2160 }), Scale::ONE)
        };
        let Some(saved) = geometry_to_persist(&ws, prior, false) else {
            return;
        };
        assert!(saved.maximized);

        assert_eq!(
            resolve_boot(saved, Scale::ONE, identity_clamp)
                .map(|boot| (boot.maximized(), boot.logical())),
            Some((true, LogicalSize { w: 1280, h: 720 }))
        );
    }
}
