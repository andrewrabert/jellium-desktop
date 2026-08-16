//! Window ownership as a two-valued fact, and everything that follows from
//! it: the boot-geometry seeding path, whether mpv's size is reconciled at
//! boot, and which [`WindowSource`] is authoritative.

use crate::geometry::{BootGeometry, LogicalSize, PhysicalSize, Scale, mpv_reconcile_size};
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
                maximized: g.maximized(),
            }),
        }
    }

    /// The physical size mpv's window is resized to at boot.
    ///
    /// `None` where the app creates the app window: mpv sizes nothing there.
    /// `None` where `locked`, where `saved_logical` maps to no representable
    /// physical size, and where it maps to `saved_physical`.
    pub fn reconcile_mpv_size(
        &self,
        reported: Scale,
        saved_logical: LogicalSize,
        saved_physical: PhysicalSize,
        locked: bool,
    ) -> Option<PhysicalSize> {
        match self {
            WindowOwner::App(_) => None,
            WindowOwner::Mpv(_) => {
                mpv_reconcile_size(reported, saved_logical, saved_physical, locked)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::{COVERED_SCALES, WindowGeometry};
    use crate::window_source::WindowSnapshot;
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
        fn snapshot(&self) -> WindowSnapshot {
            WindowSnapshot {
                extent: None,
                position: None,
                maximized: false,
                fullscreen: false,
            }
        }
    }

    impl AppCreatedWindow for SeedingWindow {
        fn seed_boot_geometry(&self, g: &BootGeometry) {
            self.seeded.lock().push(*g);
        }
    }

    struct MpvWindow;

    impl WindowSource for MpvWindow {
        fn snapshot(&self) -> WindowSnapshot {
            WindowSnapshot {
                extent: None,
                position: None,
                maximized: false,
                fullscreen: false,
            }
        }
    }

    impl MpvCreatedWindow for MpvWindow {}

    fn boot(scale: Scale, x: i32, y: i32, maximized: bool) -> BootGeometry {
        let logical = LogicalSize { w: 1280, h: 720 };
        let physical = logical
            .to_physical(scale)
            .unwrap_or(PhysicalSize { w: 1280, h: 720 });
        BootGeometry::from_clamped(
            logical,
            scale,
            WindowGeometry::from_raw(physical.w, physical.h, x, y),
            maximized,
        )
    }

    #[test]
    fn an_app_owner_seeds_and_hands_mpv_nothing_at_every_covered_scale() {
        let window = SeedingWindow::new();
        let owner = WindowOwner::App(&window);
        let mut handed = Vec::new();
        for scale in COVERED_SCALES {
            handed.push(
                owner
                    .apply_boot_geometry(&boot(scale, 100, 50, false))
                    .map(|w| w.geometry),
            );
        }
        assert_eq!(handed, vec![None; COVERED_SCALES.len()]);
        assert_eq!(window.seeded.lock().len(), COVERED_SCALES.len());
    }

    #[test]
    fn an_app_owner_reconciles_no_mpv_size_at_any_covered_scale() {
        let window = SeedingWindow::new();
        let owner = WindowOwner::App(&window);
        let logical = LogicalSize { w: 1280, h: 720 };
        let physical = PhysicalSize { w: 1280, h: 720 };
        let answers: Vec<Option<PhysicalSize>> = COVERED_SCALES
            .into_iter()
            .map(|s| owner.reconcile_mpv_size(s, logical, physical, false))
            .collect();
        assert_eq!(answers, vec![None; COVERED_SCALES.len()]);
    }

    #[test]
    fn an_mpv_owner_seeds_nothing_and_answers_the_exact_geometry_string() {
        let owner = WindowOwner::Mpv(&MpvWindow);
        let describe = |x, y, maximized| {
            owner
                .apply_boot_geometry(&boot(Scale::ONE, x, y, maximized))
                .map(|w| (w.geometry, w.force_position, w.maximized))
        };
        assert_eq!(
            describe(-1, -1, false),
            Some(("1280x720".to_owned(), false, false))
        );
        assert_eq!(
            describe(100, 50, false),
            Some(("1280x720+100+50".to_owned(), true, false))
        );
        assert_eq!(
            describe(-1, -1, true),
            Some(("1280x720".to_owned(), false, true))
        );
        assert_eq!(
            describe(100, 50, true),
            Some(("1280x720+100+50".to_owned(), true, true))
        );
    }

    #[test]
    fn an_mpv_owner_reconciles_the_size_the_reported_scale_names() {
        let owner = WindowOwner::Mpv(&MpvWindow);
        let logical = LogicalSize { w: 1280, h: 720 };
        let physical = PhysicalSize { w: 1280, h: 720 };
        let answers: Vec<Option<PhysicalSize>> = COVERED_SCALES
            .into_iter()
            .map(|s| owner.reconcile_mpv_size(s, logical, physical, false))
            .collect();
        assert_eq!(
            answers,
            vec![
                Some(PhysicalSize { w: 640, h: 360 }),
                Some(PhysicalSize { w: 960, h: 540 }),
                None,
                Some(PhysicalSize { w: 1600, h: 900 }),
                Some(PhysicalSize { w: 1920, h: 1080 }),
                Some(PhysicalSize { w: 2560, h: 1440 }),
            ]
        );
    }

    #[test]
    fn a_locked_window_reconciles_no_mpv_size() {
        let owner = WindowOwner::Mpv(&MpvWindow);
        let logical = LogicalSize { w: 1280, h: 720 };
        let physical = PhysicalSize { w: 1280, h: 720 };
        let answers: Vec<Option<PhysicalSize>> = COVERED_SCALES
            .into_iter()
            .map(|s| owner.reconcile_mpv_size(s, logical, physical, true))
            .collect();
        assert_eq!(answers, vec![None; COVERED_SCALES.len()]);
    }
}
