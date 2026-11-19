//! Window ownership as a two-valued fact, and everything that follows from
//! it: the boot-geometry seeding path, whether mpv's size is reconciled at
//! boot, and which [`WindowSource`] is authoritative.

use crate::geometry::{
    BootGeometry, LogicalSize, MpvScreen, PhysicalSize, Scale, Unrepresentable, mpv_reconcile_size,
};
use crate::window_source::WindowSource;

/// The app window where the app creates it: its live geometry, and the boot
/// geometry seeded into it.
pub trait AppCreatedWindow: WindowSource {
    /// Sizes, places and maximizes the app window per `g` before returning.
    fn seed_boot_geometry(&self, g: &BootGeometry);
}

/// The app window where mpv creates it: its live geometry, observed from
/// mpv's own window.
pub trait MpvCreatedWindow: WindowSource {}

/// The window options mpv is started with where mpv creates the app window.
pub struct MpvBootWindow {
    /// mpv's `--geometry`, physical pixels.
    pub geometry: String,
    pub force_position: bool,
    pub screen: Option<MpvScreen>,
    pub maximized: bool,
}

/// Which party creates and holds the app window.
///
/// The sole determinant of the boot-geometry seeding path, of whether mpv's
/// size is reconciled at boot, and of which [`WindowSource`] is
/// authoritative. A compositor-backed source implements [`AppCreatedWindow`]
/// and nothing else, so it cannot stand under [`WindowOwner::Mpv`];
/// [`MpvBootWindow`] is produced by the [`WindowOwner::Mpv`] arm alone, so an
/// app-created window has no geometry string to hand mpv.
#[derive(Clone, Copy)]
pub enum WindowOwner<'src> {
    App(&'src dyn AppCreatedWindow),
    Mpv(&'src dyn MpvCreatedWindow),
}

impl<'src> WindowOwner<'src> {
    /// The live geometry authority for the app window.
    pub fn source(&self) -> &'src dyn WindowSource {
        match self {
            WindowOwner::App(w) => *w,
            WindowOwner::Mpv(w) => *w,
        }
    }

    /// Seeds the app-created window with `g`, answering `None`.
    ///
    /// Seeds nothing where mpv creates the window, answering the options it
    /// is started with.
    pub fn apply_boot_geometry(&self, g: &BootGeometry) -> Option<MpvBootWindow> {
        match self {
            WindowOwner::App(w) => {
                w.seed_boot_geometry(g);
                None
            }
            WindowOwner::Mpv(_) => Some(MpvBootWindow {
                geometry: g.mpv_geometry_string(),
                force_position: g.force_position(),
                screen: g.mpv_screen().cloned(),
                maximized: g.maximized(),
            }),
        }
    }

    pub fn reconcile_mpv_size(
        &self,
        reported: Scale,
        saved_logical: LogicalSize,
        saved_physical: PhysicalSize,
        locked: bool,
    ) -> Result<Option<PhysicalSize>, Unrepresentable> {
        match self {
            WindowOwner::App(_) => Ok(None),
            WindowOwner::Mpv(_) => {
                mpv_reconcile_size(reported, saved_logical, saved_physical, locked)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::{
        Bounds, COVERED_SCALES, ClampError, ScreenName, TargetScreen, WindowGeometry,
        WindowPlacement, WindowPos, clamp_to_screen,
    };
    use crate::window_source::{WindowState, WindowStateError};
    use parking_lot::Mutex;

    struct SeedingWindow {
        seeded: Mutex<Vec<BootGeometry>>,
    }

    impl SeedingWindow {
        fn new() -> Self {
            Self {
                seeded: Mutex::new(Vec::new()),
            }
        }
    }

    impl WindowSource for SeedingWindow {
        fn window_state(&self) -> Result<WindowState, WindowStateError> {
            Err(WindowStateError::NoWindow)
        }
    }

    impl AppCreatedWindow for SeedingWindow {
        fn seed_boot_geometry(&self, g: &BootGeometry) {
            self.seeded.lock().push(g.clone());
        }
    }

    struct MpvWindow;

    impl WindowSource for MpvWindow {
        fn window_state(&self) -> Result<WindowState, WindowStateError> {
            Err(WindowStateError::NoWindow)
        }
    }

    impl MpvCreatedWindow for MpvWindow {}

    fn boot(
        scale: Scale,
        position: Option<WindowPos>,
        maximized: bool,
    ) -> Result<BootGeometry, Unrepresentable> {
        let logical = LogicalSize { w: 1280, h: 720 };
        let physical = logical.to_physical(scale)?;
        Ok(BootGeometry::from_clamped(
            logical,
            scale,
            WindowGeometry {
                w: physical.w,
                h: physical.h,
                placement: position.map(|p| WindowPlacement::new(None, p)),
            }
            .on_primary(),
            maximized,
        ))
    }

    #[test]
    fn an_mpv_owner_hands_mpv_the_screen_the_clamp_named() -> Result<(), ClampError> {
        let saved = WindowGeometry {
            w: 1280,
            h: 720,
            placement: Some(WindowPlacement::new(
                ScreenName::new("DISPLAY2".to_owned()),
                WindowPos { x: 100, y: 50 },
            )),
        };
        let clamped = clamp_to_screen(
            &saved,
            |name| {
                Ok(Some(TargetScreen {
                    mpv: MpvScreen::Name(name.clone()),
                    bounds: Bounds { w: 2560, h: 1440 },
                }))
            },
            || Ok(None),
        )?;
        let logical = LogicalSize { w: 1280, h: 720 };
        let g = BootGeometry::from_clamped(logical, Scale::ONE, clamped, false);
        let handed = WindowOwner::Mpv(&MpvWindow)
            .apply_boot_geometry(&g)
            .map(|w| (w.geometry, w.screen));
        assert_eq!(
            handed,
            Some((
                "1280x720+100+50".to_owned(),
                ScreenName::new("DISPLAY2".to_owned()).map(MpvScreen::Name)
            ))
        );
        Ok(())
    }

    #[test]
    fn an_app_owner_seeds_and_hands_mpv_nothing_at_every_covered_scale()
    -> Result<(), Unrepresentable> {
        let window = SeedingWindow::new();
        let owner = WindowOwner::App(&window);
        let mut handed = Vec::new();
        for scale in COVERED_SCALES {
            handed.push(
                owner
                    .apply_boot_geometry(&boot(scale, Some(WindowPos { x: 100, y: 50 }), false)?)
                    .map(|w| w.geometry),
            );
        }
        assert_eq!(handed, vec![None; COVERED_SCALES.len()]);
        assert_eq!(window.seeded.lock().len(), COVERED_SCALES.len());
        Ok(())
    }

    #[test]
    fn an_app_owner_reconciles_no_mpv_size_at_any_covered_scale() {
        let window = SeedingWindow::new();
        let owner = WindowOwner::App(&window);
        let logical = LogicalSize { w: 1280, h: 720 };
        let physical = PhysicalSize { w: 1280, h: 720 };
        let answers: Vec<Result<Option<PhysicalSize>, Unrepresentable>> = COVERED_SCALES
            .into_iter()
            .map(|s| owner.reconcile_mpv_size(s, logical, physical, false))
            .collect();
        assert_eq!(answers, vec![Ok(None); COVERED_SCALES.len()]);
    }

    #[test]
    fn an_mpv_owner_seeds_nothing_and_answers_the_exact_geometry_string() {
        let owner = WindowOwner::Mpv(&MpvWindow);
        let describe = |position, maximized| {
            boot(Scale::ONE, position, maximized).map(|g| {
                owner
                    .apply_boot_geometry(&g)
                    .map(|w| (w.geometry, w.force_position, w.maximized))
            })
        };
        assert_eq!(
            describe(None, false),
            Ok(Some(("1280x720".to_owned(), false, false)))
        );
        assert_eq!(
            describe(Some(WindowPos { x: 100, y: 50 }), false),
            Ok(Some(("1280x720+100+50".to_owned(), true, false)))
        );
        assert_eq!(
            describe(None, true),
            Ok(Some(("1280x720".to_owned(), false, true)))
        );
        assert_eq!(
            describe(Some(WindowPos { x: 100, y: 50 }), true),
            Ok(Some(("1280x720+100+50".to_owned(), true, true)))
        );
    }

    #[test]
    fn an_mpv_owner_reconciles_the_size_the_reported_scale_names() {
        let owner = WindowOwner::Mpv(&MpvWindow);
        let logical = LogicalSize { w: 1280, h: 720 };
        let physical = PhysicalSize { w: 1280, h: 720 };
        let answers: Vec<Result<Option<PhysicalSize>, Unrepresentable>> = COVERED_SCALES
            .into_iter()
            .map(|s| owner.reconcile_mpv_size(s, logical, physical, false))
            .collect();
        assert_eq!(
            answers,
            vec![
                Ok(Some(PhysicalSize { w: 640, h: 360 })),
                Ok(Some(PhysicalSize { w: 960, h: 540 })),
                Ok(None),
                Ok(Some(PhysicalSize { w: 1600, h: 900 })),
                Ok(Some(PhysicalSize { w: 1920, h: 1080 })),
                Ok(Some(PhysicalSize { w: 2560, h: 1440 })),
            ]
        );
    }

    #[test]
    fn a_locked_window_reconciles_no_mpv_size() {
        let owner = WindowOwner::Mpv(&MpvWindow);
        let logical = LogicalSize { w: 1280, h: 720 };
        let physical = PhysicalSize { w: 1280, h: 720 };
        let answers: Vec<Result<Option<PhysicalSize>, Unrepresentable>> = COVERED_SCALES
            .into_iter()
            .map(|s| owner.reconcile_mpv_size(s, logical, physical, true))
            .collect();
        assert_eq!(answers, vec![Ok(None); COVERED_SCALES.len()]);
    }
}
