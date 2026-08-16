//! What the web overlay is sized to.
//!
//! A pure function of the window snapshot and the strip the shell overlay
//! reserves above it: no display server, no GPU, no CEF process.

use jfn_platform_abi::{LogicalSize, PhysicalSize, SurfaceSize, WindowExtent, WindowSnapshot};
use std::ffi::c_int;

/// The size handed to CEF: one coherent extent carrying the scale CEF is told
/// about, and the offset of its top edge from the window's — the strip the
/// shell overlay reserves.
///
/// `None` when the snapshot has no extent, when either extent is non-positive,
/// when the reserved strip leaves no content height, or when what is left
/// names no extent.
pub fn view_size(snapshot: &WindowSnapshot, reserved_strip: c_int) -> Option<SurfaceSize> {
    let extent = snapshot.extent?;
    let logical = extent.logical();
    let physical = extent.physical();
    if logical.w <= 0 || logical.h <= 0 || physical.w <= 0 || physical.h <= 0 {
        return None;
    }
    let logical_top = reserved_strip.clamp(0, logical.h);
    let physical_top = extent.scale().to_physical(logical_top)?.min(physical.h);
    let logical_h = logical.h - logical_top;
    let physical_h = physical.h - physical_top;
    if logical_h <= 0 || physical_h <= 0 {
        return None;
    }
    Some(SurfaceSize {
        extent: WindowExtent::new(
            PhysicalSize {
                w: physical.w,
                h: physical_h,
            },
            extent.scale(),
            LogicalSize {
                w: logical.w,
                h: logical_h,
            },
        )?,
        logical_top,
        physical_top,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use jfn_platform_abi::Scale;

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
        let extent = Scale::from_f64(2.5).and_then(|s| {
            WindowExtent::new(
                PhysicalSize { w: 1497, h: 843 },
                s,
                LogicalSize { w: 598, h: 337 },
            )
        });
        assert_eq!(
            view_size(&snap(extent), 0).map(|size| (size.extent.logical(), size.extent.physical())),
            Some((
                LogicalSize { w: 598, h: 337 },
                PhysicalSize { w: 1497, h: 843 }
            ))
        );
    }

    #[test]
    fn derived_logical_divides_by_extent_scale() {
        let extent = Scale::from_f64(2.0).and_then(|s| {
            WindowExtent::new(
                PhysicalSize { w: 1196, h: 636 },
                s,
                LogicalSize { w: 598, h: 318 },
            )
        });
        assert_eq!(
            view_size(&snap(extent), 0).map(|size| size.extent.logical()),
            Some(LogicalSize { w: 598, h: 318 })
        );
    }

    #[test]
    fn missing_or_degenerate_extent_is_none() {
        assert!(view_size(&snap(None), 0).is_none());
        let zero = WindowExtent::new(
            PhysicalSize { w: 0, h: 720 },
            Scale::ONE,
            LogicalSize { w: 0, h: 720 },
        );
        assert!(view_size(&snap(zero), 0).is_none());
    }

    #[test]
    fn the_reserved_strip_comes_off_the_top_in_both_spaces() {
        let extent = Scale::from_f64(2.0).and_then(|s| {
            WindowExtent::new(
                PhysicalSize { w: 1280, h: 720 },
                s,
                LogicalSize { w: 640, h: 360 },
            )
        });
        assert_eq!(
            view_size(&snap(extent), 32).map(|size| (
                size.logical_top,
                size.physical_top,
                size.extent.logical(),
                size.extent.physical()
            )),
            Some((
                32,
                64,
                LogicalSize { w: 640, h: 328 },
                PhysicalSize { w: 1280, h: 656 }
            ))
        );
    }

    #[test]
    fn the_strip_converts_through_the_reported_scale_at_every_covered_scale() {
        const STRIP: c_int = 37;
        let logical = LogicalSize { w: 1280, h: 720 };
        let tops: Vec<Option<(c_int, c_int)>> = jfn_platform_abi::COVERED_SCALES
            .into_iter()
            .map(|s| {
                let physical = logical.to_physical(s)?;
                let extent = WindowExtent::new(physical, s, logical)?;
                let size = view_size(&snap(Some(extent)), STRIP)?;
                Some((size.physical_top, s.to_physical(STRIP)?))
            })
            .collect();
        let agrees: Vec<Option<bool>> = tops
            .into_iter()
            .map(|pair| pair.map(|(got, want)| got == want))
            .collect();
        assert_eq!(
            agrees,
            vec![Some(true); jfn_platform_abi::COVERED_SCALES.len()]
        );
    }

    #[test]
    fn a_strip_that_leaves_no_content_is_none() {
        let extent = WindowExtent::new(
            PhysicalSize { w: 640, h: 32 },
            Scale::ONE,
            LogicalSize { w: 640, h: 32 },
        );
        assert!(view_size(&snap(extent), 32).is_none());
        assert!(view_size(&snap(extent), 64).is_none());
    }
}
