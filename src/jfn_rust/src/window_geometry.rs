//! Window-geometry lifecycle owner: boot restore, live state, exit persist.
//!
//! Live state comes from one [`WindowSource`] — Wayland reads native compositor
//! state, mpv-backed backends (macOS / Windows / X11) read mpv ingest.

use std::sync::OnceLock;

use jfn_platform_abi::{
    BootGeometry, LogicalSize, PhysicalSize, PlacedGeometry, Platform, Scale, Unrepresentable,
    WindowGeometry, WindowPlacement, WindowSource, WindowStateError, geometry::ClampError,
};

use jfn_config::JfnWindowGeometry;

const DEFAULT_LOGICAL: LogicalSize = LogicalSize { w: 1600, h: 900 };

fn plat() -> &'static dyn Platform {
    // SAFETY: geometry wiring is owned by the app boot/run/teardown sequence.
    unsafe { jfn_platform_abi::get() }
}

/// Owns the boot→live→persist lifecycle for window geometry.
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

    /// Resolve saved config into typed boot geometry, sourcing the display
    /// scale + clamp from the platform.
    pub fn boot(&self) -> Result<BootGeometry, BootGeometryError> {
        let g = jfn_config::window_geometry();
        let scale = plat().display_scale(g.placement.as_ref().map(WindowPlacement::position));
        resolve_boot(g, scale, |w| plat().clamp_window_geometry(w))
    }

    /// Read live state and write it back to config. Called at teardown before
    /// any thread-join that could hang.
    pub fn persist(&self) -> Result<(), WindowStateError> {
        let was_max_before_fs =
            jfn_playback::browser_sink::jfn_playback_was_maximized_before_fullscreen();
        if let Some(g) = geometry_to_persist(
            self.source(),
            jfn_config::window_geometry(),
            was_max_before_fs,
        )? {
            jfn_config::set_window_geometry(g);
        }
        Ok(())
    }
}

/// Pure core of [`WindowGeometryController::boot`]: saved config + display scale
/// + a clamp fn → typed boot geometry. No globals, so it's unit-testable.
fn resolve_boot(
    g: JfnWindowGeometry,
    scale: Scale,
    clamp: impl Fn(&WindowGeometry) -> Result<PlacedGeometry, ClampError>,
) -> Result<BootGeometry, BootGeometryError> {
    let (logical, physical) = match (g.logical, g.physical) {
        (Some(logical), _) => (logical, logical.to_physical(scale)?),
        (None, Some(physical)) => (physical.to_logical(scale)?, physical),
        (None, None) => (DEFAULT_LOGICAL, DEFAULT_LOGICAL.to_physical(scale)?),
    };
    // clamp operates on physical backing pixels; on Wayland it's the identity,
    // so the logical size we seed the toplevel with is unaffected.
    let maximized = g.maximized;
    let saved = WindowGeometry {
        w: physical.w,
        h: physical.h,
        placement: g.placement,
    };
    let boot = |geometry| BootGeometry::from_clamped(logical, scale, geometry, maximized);
    match clamp(&saved) {
        Ok(clamped) => Ok(boot(clamped)),
        Err(error) => Err(BootGeometryError::Clamp {
            error,
            unclamped: boot(saved.on_primary()),
        }),
    }
}

#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum BootGeometryError {
    #[error(transparent)]
    Size(#[from] Unrepresentable),
    #[error("not clamped on-screen: {error}")]
    Clamp {
        error: ClampError,
        unclamped: BootGeometry,
    },
}

/// The logical and physical sizes the saved geometry was written at.
///
/// `None` unless it records both, each with two positive axes.
pub fn saved_sizes(g: &JfnWindowGeometry) -> Option<(LogicalSize, PhysicalSize)> {
    Some((g.logical?, g.physical?))
}

pub fn controller() -> &'static WindowGeometryController {
    static CONTROLLER: OnceLock<WindowGeometryController> = OnceLock::new();
    CONTROLLER.get_or_init(WindowGeometryController::new)
}

fn geometry_to_persist(
    ws: &dyn WindowSource,
    saved: JfnWindowGeometry,
    was_maximized_before_fullscreen: bool,
) -> Result<Option<JfnWindowGeometry>, WindowStateError> {
    let window = match ws.window_state() {
        Ok(window) => window,
        Err(WindowStateError::NoWindow | WindowStateError::WebOffsetUnstated) => return Ok(None),
        Err(e) => return Err(e),
    };
    if window.fullscreen {
        let mut g = saved;
        g.maximized = was_maximized_before_fullscreen;
        return Ok(Some(g));
    }
    if window.maximized {
        let mut g = saved;
        g.maximized = true;
        return Ok(Some(g));
    }
    Ok(Some(JfnWindowGeometry {
        logical: Some(window.extent.logical()),
        physical: Some(window.extent.physical()),
        placement: ws.window_placement()?,
        maximized: false,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use jfn_platform_abi::geometry::{Bounds, clamp_to_screen};
    use jfn_platform_abi::{
        MpvScreen, ScreenName, TargetScreen, WindowExtent, WindowPos, WindowState,
    };

    struct FakeWindowSource {
        size: Option<PhysicalSize>,
        maximized: bool,
        fullscreen: bool,
        placement: Option<WindowPlacement>,
        scale: Scale,
    }

    impl WindowSource for FakeWindowSource {
        fn window_state(&self) -> Result<WindowState, WindowStateError> {
            let physical = self.size.ok_or(WindowStateError::NoWindow)?;
            let logical = physical
                .to_logical(self.scale)
                .map_err(jfn_platform_abi::ExtentError::from)?;
            Ok(WindowState {
                extent: WindowExtent::new(physical, self.scale, logical)?,
                position: self.placement.as_ref().map(WindowPlacement::position),
                maximized: self.maximized,
                fullscreen: self.fullscreen,
                web_top: 0,
                basis: jfn_platform_abi::WindowBasis::FIXED,
            })
        }

        fn window_placement(&self) -> Result<Option<WindowPlacement>, WindowStateError> {
            Ok(self.placement.clone())
        }
    }

    fn fake(size: Option<PhysicalSize>, scale: Scale) -> FakeWindowSource {
        FakeWindowSource {
            size,
            maximized: false,
            fullscreen: false,
            placement: None,
            scale,
        }
    }

    fn at(x: i32, y: i32) -> Option<WindowPlacement> {
        Some(WindowPlacement::new(None, WindowPos { x, y }))
    }

    fn on(screen: &str, x: i32, y: i32) -> Option<WindowPlacement> {
        Some(WindowPlacement::new(
            ScreenName::new(screen.to_owned()),
            WindowPos { x, y },
        ))
    }

    const LOGICAL_720: LogicalSize = LogicalSize { w: 1280, h: 720 };
    const PHYSICAL_720: PhysicalSize = PhysicalSize { w: 1280, h: 720 };
    const PHYSICAL_900: PhysicalSize = PhysicalSize { w: 1600, h: 900 };

    #[test]
    fn wayland_shaped_no_position_scaled() {
        let Some(scale) = Scale::from_f64(1.5) else {
            return;
        };
        let ws = fake(Some(PhysicalSize { w: 2400, h: 1350 }), scale);
        let g = geometry_to_persist(&ws, JfnWindowGeometry::default(), false)
            .unwrap()
            .unwrap();
        assert_eq!(g.placement, None);
        assert_eq!(g.physical, Some(PhysicalSize { w: 2400, h: 1350 }));
        assert_eq!(g.logical, Some(LogicalSize { w: 1600, h: 900 }));
        assert!(!g.maximized);
    }

    #[test]
    fn mpv_shaped_with_position() {
        let ws = FakeWindowSource {
            placement: at(100, 50),
            ..fake(Some(PHYSICAL_720), Scale::ONE)
        };
        let g = geometry_to_persist(&ws, JfnWindowGeometry::default(), false)
            .unwrap()
            .unwrap();
        assert_eq!(g.placement, at(100, 50));
        assert_eq!(g.logical, Some(LOGICAL_720));
    }

    #[test]
    fn a_negative_position_persists() {
        let ws = FakeWindowSource {
            placement: at(-1920, -40),
            ..fake(Some(PHYSICAL_720), Scale::ONE)
        };
        let g = geometry_to_persist(&ws, JfnWindowGeometry::default(), false)
            .unwrap()
            .unwrap();
        assert_eq!(g.placement, at(-1920, -40));
    }

    #[test]
    fn the_screen_persists_with_its_position() {
        let ws = FakeWindowSource {
            placement: on(r"\\.\DISPLAY2", 100, 50),
            ..fake(Some(PHYSICAL_720), Scale::ONE)
        };
        let g = geometry_to_persist(&ws, JfnWindowGeometry::default(), false)
            .unwrap()
            .unwrap();
        assert_eq!(g.placement, on(r"\\.\DISPLAY2", 100, 50));
    }

    #[test]
    fn maximized_keeps_the_saved_screen() {
        let saved = JfnWindowGeometry {
            physical: Some(PHYSICAL_720),
            placement: on(r"\\.\DISPLAY2", 100, 50),
            ..Default::default()
        };
        let ws = FakeWindowSource {
            maximized: true,
            placement: on(r"\\.\DISPLAY1", 0, 0),
            ..fake(Some(PhysicalSize { w: 3840, h: 2160 }), Scale::ONE)
        };
        let g = geometry_to_persist(&ws, saved, false).unwrap().unwrap();
        assert_eq!(g.placement, on(r"\\.\DISPLAY2", 100, 50));
    }

    #[test]
    fn maximized_keeps_prior_size() {
        let saved = JfnWindowGeometry {
            logical: Some(LOGICAL_720),
            physical: Some(PHYSICAL_720),
            ..Default::default()
        };
        let ws = FakeWindowSource {
            maximized: true,
            ..fake(Some(PhysicalSize { w: 300, h: 200 }), Scale::ONE)
        };
        let g = geometry_to_persist(&ws, saved, false).unwrap().unwrap();
        assert!(g.maximized);
        assert_eq!(g.physical, Some(PHYSICAL_720));
    }

    #[test]
    fn fullscreen_preserves_pre_fullscreen_state() {
        let saved = JfnWindowGeometry {
            physical: Some(PHYSICAL_900),
            ..Default::default()
        };
        let ws = FakeWindowSource {
            maximized: true,
            fullscreen: true,
            ..fake(Some(PhysicalSize { w: 3840, h: 2160 }), Scale::ONE)
        };
        let g = geometry_to_persist(&ws, saved, true).unwrap().unwrap();
        assert!(g.maximized);
        assert_eq!(g.physical, Some(PHYSICAL_900));
    }

    #[test]
    fn unknown_size_returns_none() {
        let ws = fake(None, Scale::ONE);
        assert!(matches!(
            geometry_to_persist(&ws, JfnWindowGeometry::default(), false),
            Ok(None)
        ));
    }

    #[test]
    fn logical_rounding() {
        for (scale, phys, logical) in [(1.25_f64, 2000, 1600), (2.0, 3000, 1500)] {
            let Some(scale) = Scale::from_f64(scale) else {
                return;
            };
            let ws = fake(Some(PhysicalSize { w: phys, h: phys }), scale);
            let g = geometry_to_persist(&ws, JfnWindowGeometry::default(), false)
                .unwrap()
                .unwrap();
            assert_eq!(g.logical.map(|s| s.w), Some(logical));
        }
    }

    #[test]
    fn saved_sizes_declines_a_geometry_missing_either_pair() {
        assert_eq!(saved_sizes(&JfnWindowGeometry::default()), None);
        assert_eq!(
            saved_sizes(&JfnWindowGeometry {
                physical: Some(PHYSICAL_900),
                ..Default::default()
            }),
            None
        );
        assert_eq!(
            saved_sizes(&JfnWindowGeometry {
                logical: Some(LOGICAL_720),
                ..Default::default()
            }),
            None
        );
        assert_eq!(
            saved_sizes(&JfnWindowGeometry {
                logical: Some(LOGICAL_720),
                physical: Some(PHYSICAL_900),
                ..Default::default()
            }),
            Some((LOGICAL_720, PHYSICAL_900))
        );
    }

    fn identity_clamp(w: &WindowGeometry) -> Result<PlacedGeometry, ClampError> {
        Ok(w.on_primary())
    }

    #[test]
    fn boot_restores_cross_scale() {
        let saved = JfnWindowGeometry {
            logical: Some(LOGICAL_720),
            ..Default::default()
        };
        assert_eq!(
            Scale::from_f64(1.25).map(|scale| {
                resolve_boot(saved, scale, identity_clamp)
                    .map(|boot| (boot.logical(), boot.physical(), boot.position()))
            }),
            Some(Ok((LOGICAL_720, PHYSICAL_900, None)))
        );
    }

    #[test]
    fn boot_prefers_the_logical_size_over_a_stale_physical_one() {
        let saved = JfnWindowGeometry {
            logical: Some(LOGICAL_720),
            physical: Some(PhysicalSize { w: 2560, h: 1440 }),
            ..Default::default()
        };
        assert_eq!(
            Scale::from_f64(1.25).map(|scale| {
                resolve_boot(saved, scale, identity_clamp)
                    .map(|boot| (boot.logical(), boot.physical()))
            }),
            Some(Ok((LOGICAL_720, PHYSICAL_900)))
        );
    }

    #[test]
    fn a_legacy_physical_size_boots_as_is_with_its_logical_at_the_current_scale() {
        let saved = JfnWindowGeometry {
            physical: Some(PHYSICAL_900),
            ..Default::default()
        };
        assert_eq!(
            Scale::from_f64(1.25).map(|scale| {
                resolve_boot(saved, scale, identity_clamp)
                    .map(|boot| (boot.logical(), boot.physical(), boot.scale() == scale))
            }),
            Some(Ok((LOGICAL_720, PHYSICAL_900, true)))
        );
    }

    #[test]
    fn no_saved_size_boots_the_default_at_the_current_scale() {
        assert_eq!(
            Scale::from_f64(2.0).map(|scale| {
                resolve_boot(JfnWindowGeometry::default(), scale, identity_clamp)
                    .map(|boot| (boot.logical(), boot.physical()))
            }),
            Some(Ok((DEFAULT_LOGICAL, PhysicalSize { w: 3200, h: 1800 })))
        );
    }

    #[test]
    fn a_negative_saved_position_reaches_the_clamp_and_the_boot_geometry() {
        let saved = JfnWindowGeometry {
            logical: Some(LOGICAL_720),
            placement: at(-1920, -40),
            ..Default::default()
        };
        let seen = std::cell::Cell::new(None);
        let clamp = |w: &WindowGeometry| {
            seen.set(w.placement.as_ref().map(WindowPlacement::position));
            Ok::<_, ClampError>(w.on_primary())
        };
        assert_eq!(
            resolve_boot(saved, Scale::ONE, clamp).map(|boot| boot.position()),
            Ok(Some(WindowPos { x: -1920, y: -40 }))
        );
        assert_eq!(seen.get(), Some(WindowPos { x: -1920, y: -40 }));
    }

    #[test]
    fn the_saved_screen_reaches_the_clamp_and_its_answer_reaches_mpv() {
        let saved = JfnWindowGeometry {
            logical: Some(LOGICAL_720),
            placement: on(r"\\.\DISPLAY2", 100, 50),
            ..Default::default()
        };
        let clamp = |w: &WindowGeometry| clamp_to_screen(w, second_screen, || Ok(None));
        assert_eq!(
            resolve_boot(saved, Scale::ONE, clamp)
                .map(|boot| (boot.position(), boot.mpv_screen().cloned())),
            Ok((Some(WindowPos { x: 100, y: 50 }), Some(MpvScreen::Index(1))))
        );
    }

    fn second_screen(name: &ScreenName) -> Result<Option<TargetScreen>, ClampError> {
        Ok((name.as_str() == r"\\.\DISPLAY2").then_some(TargetScreen {
            mpv: MpvScreen::Index(1),
            bounds: Bounds { w: 2560, h: 1440 },
        }))
    }

    fn failing_clamp(_: &WindowGeometry) -> Result<PlacedGeometry, ClampError> {
        Err(ClampError::Monitors)
    }

    #[test]
    fn an_unclamped_boot_names_no_mpv_screen() {
        let saved = JfnWindowGeometry {
            logical: Some(LOGICAL_720),
            placement: on(r"\\.\DISPLAY2", 100, 50),
            ..Default::default()
        };
        let unclamped = match resolve_boot(saved, Scale::ONE, failing_clamp) {
            Err(BootGeometryError::Clamp { unclamped, .. }) => Some(unclamped),
            Ok(_) | Err(BootGeometryError::Size(_)) => None,
        };
        assert_eq!(
            unclamped.map(|boot| (boot.position(), boot.mpv_screen().cloned())),
            Some((Some(WindowPos { x: 100, y: 50 }), None))
        );
    }

    #[test]
    fn maximize_round_trip_preserves_size() {
        // Live state: maximized; persist keeps the prior (pre-maximize) size.
        let prior = JfnWindowGeometry {
            logical: Some(LOGICAL_720),
            physical: Some(PHYSICAL_720),
            ..Default::default()
        };
        let ws = FakeWindowSource {
            maximized: true,
            ..fake(Some(PhysicalSize { w: 3840, h: 2160 }), Scale::ONE)
        };
        let Ok(Some(saved)) = geometry_to_persist(&ws, prior, false) else {
            return;
        };
        assert!(saved.maximized);

        // Next boot off that saved state comes up maximized at the prior size.
        assert_eq!(
            resolve_boot(saved, Scale::ONE, identity_clamp)
                .map(|boot| (boot.maximized(), boot.logical())),
            Ok((true, LOGICAL_720))
        );
    }
}
