use std::num::NonZeroU64;

#[cfg(target_os = "windows")]
use jfn_platform_abi::WindowPos;
use jfn_platform_abi::{LogicalPoint, PhysicalPoint, PhysicalSize, Scale, WindowExtent};

pub const BASE_DPI: NonZeroU64 = match NonZeroU64::new(96) {
    Some(d) => d,
    None => unreachable!(),
};

pub fn scale_from_dpi(dpi: u32) -> Option<Scale> {
    Scale::from_ratio(u64::from(dpi), BASE_DPI)
}

pub fn report_dpi(source: &str, dpi: u32) -> Scale {
    if let Some(scale) = scale_from_dpi(dpi) {
        return scale;
    }
    let reported = Scale::ONE;
    tracing::warn!(
        target: "platform",
        "{source} reported {dpi} DPI; Windows reports {reported}"
    );
    reported
}

#[cfg(target_os = "windows")]
pub fn display_scale(at: Option<WindowPos>) -> Scale {
    tracing::trace!(
        target: "platform",
        "the Windows system DPI is per-process, so {at:?} names the scale Windows reports"
    );
    crate::platform::win_display_scale()
}

pub fn extent(client: PhysicalSize, dpi: u32) -> Option<WindowExtent> {
    let scale = scale_from_dpi(dpi)?;
    WindowExtent::new(client, scale, client.to_logical(scale)?)
}

pub fn view_point(extent: Option<WindowExtent>, p: PhysicalPoint) -> LogicalPoint {
    extent.map_or(LogicalPoint { x: p.x, y: p.y }, |e| e.to_logical_point(p))
}

#[cfg(test)]
mod tests {
    use super::*;
    use jfn_platform_abi::COVERED_SCALES;

    const COVERED_DPI: [u32; 6] = [48, 72, 96, 120, 144, 192];

    const CLIENT: PhysicalSize = PhysicalSize { w: 1280, h: 720 };

    #[test]
    fn every_covered_scale_has_its_windows_dpi() {
        assert_eq!(
            COVERED_DPI.map(scale_from_dpi).to_vec(),
            COVERED_SCALES.map(Some).to_vec()
        );
    }

    #[test]
    fn the_extent_carries_the_reported_scale_at_every_covered_scale() {
        let scales: Vec<Option<Scale>> = COVERED_DPI
            .into_iter()
            .map(|dpi| Some(extent(CLIENT, dpi)?.scale()))
            .collect();
        assert_eq!(scales, COVERED_SCALES.map(Some).to_vec());
    }

    #[test]
    fn the_last_physical_pixel_maps_to_the_last_logical_pixel_at_every_covered_scale() {
        let corners: Vec<Option<(LogicalPoint, LogicalPoint)>> = COVERED_DPI
            .into_iter()
            .map(|dpi| {
                let e = extent(CLIENT, dpi)?;
                let logical = e.logical();
                let last = view_point(
                    Some(e),
                    PhysicalPoint {
                        x: CLIENT.w - 1,
                        y: CLIENT.h - 1,
                    },
                );
                Some((
                    last,
                    LogicalPoint {
                        x: logical.w - 1,
                        y: logical.h - 1,
                    },
                ))
            })
            .collect();
        let agrees: Vec<Option<bool>> = corners
            .into_iter()
            .map(|pair| pair.map(|(got, want)| got == want))
            .collect();
        assert_eq!(agrees, vec![Some(true); COVERED_DPI.len()]);
    }

    #[test]
    fn a_zero_dpi_reports_one() {
        assert_eq!(scale_from_dpi(0), None);
        assert_eq!(report_dpi("test", 0), Scale::ONE);
        assert_eq!(extent(CLIENT, 0), None);
        assert_eq!(extent(PhysicalSize { w: 0, h: 720 }, 96), None);
    }

    #[test]
    fn a_pointer_maps_by_the_identity_before_the_first_sample() {
        assert_eq!(
            view_point(None, PhysicalPoint { x: 37, y: 11 }),
            LogicalPoint { x: 37, y: 11 }
        );
    }
}
