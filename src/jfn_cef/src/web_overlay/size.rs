//! What the web overlay is sized to.
//!
//! A pure function of the window snapshot and the strip the shell overlay
//! reserves above it: no display server, no GPU, no CEF process.

use jfn_platform_abi::{LogicalSize, PhysicalSize, WindowSnapshot};
use std::ffi::c_int;

/// The size handed to CEF, in both coordinate spaces, and the offset of its top
/// edge from the window's — the strip the shell overlay reserves.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ViewSize {
    pub logical: LogicalSize,
    pub physical: PhysicalSize,
    pub logical_top: c_int,
    pub physical_top: c_int,
}

/// `None` when the snapshot has no extent, when either extent is non-positive,
/// or when the reserved strip leaves no content height.
pub fn view_size(snapshot: &WindowSnapshot, reserved_strip: c_int) -> Option<ViewSize> {
    let extent = snapshot.extent?;
    let logical = extent.logical();
    let physical = extent.physical();
    if logical.w <= 0 || logical.h <= 0 || physical.w <= 0 || physical.h <= 0 {
        return None;
    }
    let logical_top = reserved_strip.clamp(0, logical.h);
    let physical_top =
        ((i64::from(logical_top) * i64::from(physical.h)) / i64::from(logical.h)) as c_int;
    let logical_h = logical.h - logical_top;
    let physical_h = physical.h - physical_top;
    if logical_h <= 0 || physical_h <= 0 {
        return None;
    }
    Some(ViewSize {
        logical: LogicalSize {
            w: logical.w,
            h: logical_h,
        },
        physical: PhysicalSize {
            w: physical.w,
            h: physical_h,
        },
        logical_top,
        physical_top,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use jfn_platform_abi::{Scale, WindowExtent};

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
        let Some(size) = view_size(&snap(Some(extent)), 0) else {
            panic!("expected size");
        };
        assert_eq!(size.logical, LogicalSize { w: 598, h: 337 });
        assert_eq!(size.physical, PhysicalSize { w: 1497, h: 843 });
    }

    #[test]
    fn derived_logical_divides_by_extent_scale() {
        let extent = WindowExtent::new(PhysicalSize { w: 1196, h: 636 }, Scale(2.0));
        let Some(size) = view_size(&snap(Some(extent)), 0) else {
            panic!("expected size");
        };
        assert_eq!(size.logical, LogicalSize { w: 598, h: 318 });
    }

    #[test]
    fn missing_or_degenerate_extent_is_none() {
        assert!(view_size(&snap(None), 0).is_none());
        let zero = WindowExtent::new(PhysicalSize { w: 0, h: 720 }, Scale(1.0));
        assert!(view_size(&snap(Some(zero)), 0).is_none());
    }

    #[test]
    fn the_reserved_strip_comes_off_the_top_in_both_spaces() {
        let extent = WindowExtent::new(PhysicalSize { w: 1280, h: 720 }, Scale(2.0));
        let Some(size) = view_size(&snap(Some(extent)), 32) else {
            panic!("expected size");
        };
        assert_eq!(size.logical_top, 32);
        assert_eq!(size.physical_top, 64);
        assert_eq!(size.logical, LogicalSize { w: 640, h: 328 });
        assert_eq!(size.physical, PhysicalSize { w: 1280, h: 656 });
    }

    #[test]
    fn a_strip_that_leaves_no_content_is_none() {
        let extent = WindowExtent::new(PhysicalSize { w: 640, h: 32 }, Scale(1.0));
        assert!(view_size(&snap(Some(extent)), 32).is_none());
        assert!(view_size(&snap(Some(extent)), 64).is_none());
    }
}
