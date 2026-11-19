use jfn_platform_abi::{
    ExtentError, LogicalSize, PhysicalSize, SurfaceSize, WindowExtent, WindowState,
};

pub fn view_size(window: &WindowState) -> Result<Option<SurfaceSize>, ExtentError> {
    let extent = window.extent;
    let logical = extent.logical();
    let physical = extent.physical();
    let logical_top = window.web_top.clamp(0, logical.h);
    let physical_top = extent.scale().to_physical(logical_top)?.min(physical.h);
    let logical_h = logical.h - logical_top;
    let physical_h = physical.h - physical_top;
    if logical_h <= 0 || physical_h <= 0 {
        return Ok(None);
    }
    Ok(Some(SurfaceSize {
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
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use jfn_platform_abi::Scale;
    use std::ffi::c_int;

    fn window(extent: WindowExtent, web_top: c_int) -> WindowState {
        WindowState {
            extent,
            position: None,
            maximized: false,
            fullscreen: false,
            web_top,
            basis: jfn_platform_abi::WindowBasis::FIXED,
        }
    }

    fn sized(
        scale: f64,
        physical: PhysicalSize,
        logical: LogicalSize,
        web_top: c_int,
    ) -> Option<Result<Option<(LogicalSize, PhysicalSize)>, ExtentError>> {
        Scale::from_f64(scale).map(|s| {
            let extent = WindowExtent::new(physical, s, logical)?;
            let size = view_size(&window(extent, web_top))?;
            Ok(size.map(|size| (size.extent.logical(), size.extent.physical())))
        })
    }

    #[test]
    fn exact_logical_wins_over_division() {
        // 1497 / 2.5 rounds to 599 — the compositor's exact 598 must win
        // over re-derivation.
        let physical = PhysicalSize { w: 1497, h: 843 };
        let logical = LogicalSize { w: 598, h: 337 };
        assert_eq!(
            sized(2.5, physical, logical, 0),
            Some(Ok(Some((logical, physical))))
        );
    }

    #[test]
    fn derived_logical_divides_by_extent_scale() {
        let physical = PhysicalSize { w: 1196, h: 636 };
        let logical = LogicalSize { w: 598, h: 318 };
        assert_eq!(
            sized(2.0, physical, logical, 0),
            Some(Ok(Some((logical, physical))))
        );
    }

    #[test]
    fn a_degenerate_size_is_no_extent() {
        let physical = PhysicalSize { w: 0, h: 720 };
        let logical = LogicalSize { w: 0, h: 720 };
        assert_eq!(
            WindowExtent::new(physical, Scale::ONE, logical),
            Err(ExtentError::BelowTwoPixels { physical, logical })
        );
    }

    #[test]
    fn the_web_offset_comes_off_the_top_in_both_spaces() {
        assert_eq!(
            sized(
                2.0,
                PhysicalSize { w: 1280, h: 720 },
                LogicalSize { w: 640, h: 360 },
                32
            ),
            Some(Ok(Some((
                LogicalSize { w: 640, h: 328 },
                PhysicalSize { w: 1280, h: 656 }
            ))))
        );
    }

    #[test]
    fn the_offset_converts_through_the_reported_scale_at_every_covered_scale() {
        const TOP: c_int = 37;
        let logical = LogicalSize { w: 1280, h: 720 };
        let strips: Vec<Result<Option<c_int>, ExtentError>> = jfn_platform_abi::COVERED_SCALES
            .into_iter()
            .map(|s| {
                let physical = logical.to_physical(s)?;
                let extent = WindowExtent::new(physical, s, logical)?;
                let size = view_size(&window(extent, TOP))?;
                Ok(size.map(|size| physical.h - size.extent.physical().h))
            })
            .collect();
        let expected: Vec<Result<Option<c_int>, ExtentError>> = jfn_platform_abi::COVERED_SCALES
            .into_iter()
            .map(|s| Ok(Some(s.to_physical(TOP)?)))
            .collect();
        assert_eq!(strips, expected);
    }

    #[test]
    fn a_strip_that_leaves_no_content_is_none() -> Result<(), ExtentError> {
        let extent = WindowExtent::new(
            PhysicalSize { w: 640, h: 32 },
            Scale::ONE,
            LogicalSize { w: 640, h: 32 },
        )?;
        assert_eq!(view_size(&window(extent, 32)), Ok(None));
        assert_eq!(view_size(&window(extent, 64)), Ok(None));
        Ok(())
    }
}
