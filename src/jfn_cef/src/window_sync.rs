//! Level-triggered CEF sizing: layer size is a function of the current
//! window snapshot, pulled on each platform-abi window wakeup.

use crossbeam_utils::atomic::AtomicCell;
use jfn_platform_abi::{LogicalSize, PhysicalSize, WindowSnapshot};

/// The size handed to CEF, in both coordinate spaces.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct CefViewSize {
    pub logical: LogicalSize,
    pub physical: PhysicalSize,
}

/// What CEF should be sized to for a given snapshot; `None` when the
/// extent is unknown or degenerate.
pub(crate) fn cef_size_from_snapshot(snap: &WindowSnapshot) -> Option<CefViewSize> {
    let extent = snap.extent?;
    let logical = extent.logical();
    let physical = extent.physical();
    if logical.w <= 0 || logical.h <= 0 || physical.w <= 0 || physical.h <= 0 {
        return None;
    }
    Some(CefViewSize { logical, physical })
}

/// Last size handed to CEF. Lock-free, so two concurrent wakeups can apply
/// out of order; `reconcile` re-pulls after each apply and applies again
/// until the cell agrees with the current snapshot.
static LAST_APPLIED: AtomicCell<Option<CefViewSize>> = AtomicCell::new(None);

// Returns once a pull finds `cell` already holding the pulled size, or
// once a pull yields no size.
fn reconcile<P, A>(cell: &AtomicCell<Option<CefViewSize>>, pull: P, mut apply: A)
where
    P: Fn() -> Option<CefViewSize>,
    A: FnMut(CefViewSize),
{
    while let Some(size) = pull() {
        if cell.swap(Some(size)) == Some(size) {
            return;
        }
        apply(size);
    }
}

/// Pull the current window snapshot and size the CEF layers from it.
/// Callable from any thread, any number of times.
pub(crate) fn sync_from_window() {
    reconcile(
        &LAST_APPLIED,
        || cef_size_from_snapshot(&jfn_platform_abi::get().window_source().snapshot()),
        |size| {
            jfn_logging::log(
                jfn_logging::CATEGORY_CEF,
                jfn_logging::LEVEL_DEBUG,
                &format!(
                    "window sync: logical={}x{} physical={}x{}",
                    size.logical.w, size.logical.h, size.physical.w, size.physical.h
                ),
            );
            crate::browsers::jfn_browsers_set_size(
                size.logical.w,
                size.logical.h,
                size.physical.w,
                size.physical.h,
            );
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use jfn_platform_abi::{Scale, WindowExtent, WindowSnapshot};

    fn snap(extent: Option<WindowExtent>) -> WindowSnapshot {
        WindowSnapshot {
            extent,
            position: None,
            maximized: false,
            fullscreen: false,
        }
    }

    #[test]
    fn exact_logical_wins_over_division() {
        // 1497 / 2.5 rounds to 599 — the compositor's exact 598 must win
        // over re-derivation.
        let extent = WindowExtent::with_logical(
            PhysicalSize { w: 1497, h: 843 },
            Scale(2.5),
            LogicalSize { w: 598, h: 337 },
        );
        let size = cef_size_from_snapshot(&snap(Some(extent)));
        let Some(size) = size else {
            panic!("expected size");
        };
        assert_eq!(size.logical, LogicalSize { w: 598, h: 337 });
        assert_eq!(size.physical, PhysicalSize { w: 1497, h: 843 });
    }

    #[test]
    fn derived_logical_divides_by_extent_scale() {
        let extent = WindowExtent::new(PhysicalSize { w: 1196, h: 636 }, Scale(2.0));
        let Some(size) = cef_size_from_snapshot(&snap(Some(extent))) else {
            panic!("expected size");
        };
        assert_eq!(size.logical, LogicalSize { w: 598, h: 318 });
    }

    #[test]
    fn missing_or_degenerate_extent_is_none() {
        assert!(cef_size_from_snapshot(&snap(None)).is_none());
        let zero = WindowExtent::new(PhysicalSize { w: 0, h: 720 }, Scale(1.0));
        assert!(cef_size_from_snapshot(&snap(Some(zero))).is_none());
    }

    fn size_of(extent: WindowExtent) -> CefViewSize {
        let Some(size) = cef_size_from_snapshot(&snap(Some(extent))) else {
            panic!("expected size");
        };
        size
    }

    /// No matter which wakeups are dropped, the final applied size equals
    /// the size derived from the current snapshot.
    #[test]
    fn applied_size_matches_source_regardless_of_dropped_wakeups() {
        let extents = [
            WindowExtent::new(PhysicalSize { w: 1280, h: 720 }, Scale(2.0)),
            WindowExtent::new(PhysicalSize { w: 1196, h: 636 }, Scale(2.0)),
            WindowExtent::new(PhysicalSize { w: 1196, h: 636 }, Scale(1.5)),
            WindowExtent::new(PhysicalSize { w: 2400, h: 1350 }, Scale(1.5)),
        ];
        let n = extents.len() as u32;
        // Each bit decides whether the wakeup after mutation i is dropped.
        for drop_mask in 0..(1u32 << n) {
            let cell = AtomicCell::new(None);
            let current = std::cell::Cell::new(None);
            let mut applied_log: Vec<CefViewSize> = Vec::new();
            {
                let wake = |applied_log: &mut Vec<CefViewSize>| {
                    reconcile(
                        &cell,
                        || cef_size_from_snapshot(&snap(current.get())),
                        |size| applied_log.push(size),
                    );
                };
                for (i, extent) in extents.iter().enumerate() {
                    current.set(Some(*extent));
                    if drop_mask & (1 << i) == 0 {
                        wake(&mut applied_log);
                    }
                }
                // The attach-time reconcile repairs whatever the dropped
                // wakeups missed.
                wake(&mut applied_log);
            }
            assert_eq!(cell.load(), cef_size_from_snapshot(&snap(current.get())));
            assert_eq!(
                applied_log.last().copied(),
                cef_size_from_snapshot(&snap(current.get()))
            );
            for pair in applied_log.windows(2) {
                assert_ne!(pair[0], pair[1]);
            }
        }
    }

    /// A pull that already matches the cell applies nothing.
    #[test]
    fn reconcile_stops_when_the_cell_matches_the_pull() {
        let size = size_of(WindowExtent::new(
            PhysicalSize { w: 1280, h: 720 },
            Scale(2.0),
        ));
        let cell = AtomicCell::new(Some(size));
        let mut applied: Vec<CefViewSize> = Vec::new();
        reconcile(&cell, || Some(size), |s| applied.push(s));
        assert!(applied.is_empty());
        assert_eq!(cell.load(), Some(size));
    }

    /// A wakeup whose first pull is stale — a racing wakeup already applied
    /// the newer size — ends with the newer size applied last and held by
    /// the cell.
    #[test]
    fn reconcile_repairs_an_apply_that_lost_to_a_newer_racer() {
        let old = size_of(WindowExtent::new(
            PhysicalSize { w: 1280, h: 720 },
            Scale(2.0),
        ));
        let new = size_of(WindowExtent::new(
            PhysicalSize { w: 2400, h: 1350 },
            Scale(1.5),
        ));
        // The racer already applied `new`; this wakeup's first pull is `old`.
        let cell = AtomicCell::new(Some(new));
        let pulls = std::cell::Cell::new(0u32);
        let mut applied: Vec<CefViewSize> = Vec::new();
        reconcile(
            &cell,
            || {
                pulls.set(pulls.get() + 1);
                Some(if pulls.get() == 1 { old } else { new })
            },
            |s| applied.push(s),
        );
        assert_eq!(applied, vec![old, new]);
        assert_eq!(cell.load(), Some(new));
    }

    /// A pull yielding no size leaves the cell untouched and applies nothing.
    #[test]
    fn reconcile_ignores_a_degenerate_pull() {
        let size = size_of(WindowExtent::new(
            PhysicalSize { w: 1280, h: 720 },
            Scale(2.0),
        ));
        let cell = AtomicCell::new(Some(size));
        let mut applied: Vec<CefViewSize> = Vec::new();
        reconcile(&cell, || None, |s| applied.push(s));
        assert!(applied.is_empty());
        assert_eq!(cell.load(), Some(size));
    }
}
