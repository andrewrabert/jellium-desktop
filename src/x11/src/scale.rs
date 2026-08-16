//! X11 display scale probe: the app's scale authority.
//!
//! The app owns geometry and scale on X11 (mpv is embedded and passive), so
//! this probe defines the logical ↔ physical conversion everywhere: boot
//! restore, persist, CEF device scale, and input mapping. The Xft.dpi
//! half-step quantization matches mpv's historical behavior
//! (`third_party/mpv/video/out/x11_common.c`) so saved logical sizes
//! round-trip across the ownership change; the tests pin it.

use std::num::NonZeroU64;
use std::sync::atomic::{AtomicBool, Ordering};

use jfn_platform_abi::{LogicalPoint, PhysicalPoint, PhysicalSize, Scale, WindowExtent, WindowPos};
use x11rb::connection::Connection;
use x11rb::resource_manager::new_from_resource_manager;
use x11rb::rust_connection::RustConnection;

const BASE_DPI: f64 = 96.0;

/// The two half-steps of [`BASE_DPI`] that make up one whole scale step.
const HALF_STEPS_PER_UNIT: NonZeroU64 = match NonZeroU64::new(2) {
    Some(d) => d,
    None => unreachable!(),
};

static UNANSWERED_LOGGED: AtomicBool = AtomicBool::new(false);

/// The scale X11 reports when neither DPI source answered. Logged once, as
/// this backend's own decision.
fn unanswered() -> Scale {
    let reported = Scale::ONE;
    if !UNANSWERED_LOGGED.swap(true, Ordering::Relaxed) {
        tracing::info!(
            target: "x11::scale",
            "neither Xft.dpi nor the screen DPI stated a scale; X11 reports {reported}"
        );
    }
    reported
}

/// The scale X11 reports.
///
/// `Xft.dpi` first, then the screen's own DPI. [`Scale::ONE`] when neither
/// answered, logged once as this backend's decision.
pub(crate) fn query_display_scale() -> Scale {
    probe().unwrap_or_else(unanswered)
}

fn probe() -> Option<Scale> {
    // Explicitly target the real server: while the mpv proxy has DISPLAY
    // repointed, env-based connect would route through it.
    let display = crate::mpv_proxy::real_display();
    let (conn, screen_num) = RustConnection::connect(display.as_deref()).ok()?;
    query_xft_dpi_scale(&conn).or_else(|| query_screen_dpi_scale(&conn, screen_num))
}

/// The scale X11 reports for the app window: the geometry thread's published
/// scale once it has published one, else [`query_display_scale`].
pub(crate) fn window_scale() -> Scale {
    crate::x11_state::parent_snapshot()
        .map(|s| s.scale)
        .unwrap_or_else(query_display_scale)
}

/// The scale X11 reports for `at`.
///
/// X11's DPI sources are per-server, not per-display, so every position
/// names the same scale as [`query_display_scale`].
pub(crate) fn display_scale(at: Option<WindowPos>) -> Scale {
    tracing::trace!(
        target: "x11::scale",
        "X11 DPI is per-server, so {at:?} names the scale X11 reports"
    );
    query_display_scale()
}

/// Logs `raw` beside the quantized value whenever the two differ: the
/// half-step quantization is this backend's own decision, not the DPI the
/// server reported.
fn report(source: &str, raw: f64, scale: Scale) -> Scale {
    let unquantized = raw / BASE_DPI;
    if unquantized == scale.as_f64() {
        tracing::debug!(target: "x11::scale", "{source} {raw} DPI: scale {scale}");
    } else {
        tracing::debug!(
            target: "x11::scale",
            "{source} {raw} DPI is {unquantized}; X11 quantizes to the nearest half step: scale {scale}"
        );
    }
    scale
}

fn query_xft_dpi_scale(conn: &impl Connection) -> Option<Scale> {
    let db = new_from_resource_manager(conn).ok().flatten()?;
    let value: i64 = db.get_value("Xft.dpi", "").ok().flatten()?;
    let dpi = value as f64;
    Some(report("Xft.dpi", dpi, quantize_dpi(dpi)?))
}

fn query_screen_dpi_scale(conn: &impl Connection, screen_num: usize) -> Option<Scale> {
    let screen = conn.setup().roots.get(screen_num)?;
    screen_dpi_scale(
        screen.width_in_pixels,
        screen.height_in_pixels,
        screen.width_in_millimeters,
        screen.height_in_millimeters,
    )
}

/// Millimetres per inch, the unit the X11 setup reports a screen's physical
/// extent in.
const MM_PER_INCH: f64 = 25.4;

// the screen's DPI from its pixel and millimetre extents, quantized
// rejects a zero millimetre extent, a non-finite DPI, and axes that
// quantize to different half-steps
pub(crate) fn screen_dpi_scale(
    width_px: u16,
    height_px: u16,
    width_mm: u16,
    height_mm: u16,
) -> Option<Scale> {
    if width_mm == 0 || height_mm == 0 {
        return None;
    }
    let dpi_x = f64::from(width_px) * MM_PER_INCH / f64::from(width_mm);
    let dpi_y = f64::from(height_px) * MM_PER_INCH / f64::from(height_mm);
    if !dpi_x.is_finite() || !dpi_y.is_finite() {
        return None;
    }
    let sx = quantize_dpi_steps(dpi_x)?;
    let sy = quantize_dpi_steps(dpi_y)?;
    if sx != sy {
        return None;
    }
    Some(report("X11 screen", dpi_x, half_steps_to_scale(sx, dpi_x)?))
}

/// The extent a published parent geometry names.
///
/// X11 publishes no logical size, so this backend divides the physical size
/// by the scale it reports to name one.
///
/// `None` when the division does not name a logical size, or when either
/// axis is below two pixels.
pub(crate) fn extent(physical: PhysicalSize, scale: Scale) -> Option<WindowExtent> {
    WindowExtent::new(physical, scale, physical.to_logical(scale)?)
}

/// The pointer position in the space `extent`'s logical size names.
///
/// The identity when no extent has been published.
pub(crate) fn view_point(extent: Option<WindowExtent>, p: PhysicalPoint) -> LogicalPoint {
    extent.map_or(LogicalPoint { x: p.x, y: p.y }, |e| e.to_logical_point(p))
}

/// Half-steps of 96 DPI, matching mpv's `x11_common.c`, so saved logical
/// sizes round-trip across the ownership change. Rejects a result at or below
/// 1.0.
pub(crate) fn quantize_dpi(dpi: f64) -> Option<Scale> {
    half_steps_to_scale(quantize_dpi_steps(dpi)?, dpi)
}

fn half_steps_to_scale(half_steps: i32, dpi: f64) -> Option<Scale> {
    Scale::from_ratio(u64::try_from(half_steps).ok()?, HALF_STEPS_PER_UNIT).filter(|scale| {
        if *scale > Scale::ONE {
            return true;
        }
        tracing::debug!(
            target: "x11::scale",
            "{dpi} DPI quantizes to {scale}; X11 reports no scale at or below 1"
        );
        false
    })
}

fn quantize_dpi_steps(dpi: f64) -> Option<i32> {
    if !dpi.is_finite() {
        return None;
    }
    let s = (2.0 * dpi / BASE_DPI).clamp(0.0, 20.0).round_ties_even() as i32;
    if s > 2 && s < 20 { Some(s) } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jfn_platform_abi::{COVERED_SCALES, LogicalSize};

    fn scale(value: f64) -> Option<Scale> {
        Scale::from_f64(value)
    }

    #[test]
    fn xft_dpi_uses_mpv_half_step_quantization() {
        assert_eq!(quantize_dpi(144.0), scale(1.5));
        assert_eq!(quantize_dpi(168.0), scale(2.0));
        assert_eq!(quantize_dpi(192.0), scale(2.0));
        assert_eq!(quantize_dpi(288.0), scale(3.0));
    }

    #[test]
    fn unscaled_or_invalid_dpi_is_ignored() {
        assert_eq!(quantize_dpi(96.0), None);
        assert_eq!(quantize_dpi(120.0), None);
        assert_eq!(quantize_dpi(0.0), None);
        assert_eq!(quantize_dpi(f64::NAN), None);
    }

    #[test]
    fn screen_dpi_fallback_requires_matching_axes() {
        assert_eq!(quantize_dpi_steps(144.0), Some(3));
        assert_ne!(quantize_dpi_steps(144.0), quantize_dpi_steps(192.0));
    }

    /// A 1920x1080 panel measuring 338x190 mm reads 144 DPI on both axes.
    const PANEL_PX: (u16, u16) = (1920, 1080);
    const PANEL_MM: (u16, u16) = (338, 190);

    #[test]
    fn the_screen_dpi_fallback_rejects_disagreeing_axes() {
        assert_eq!(
            screen_dpi_scale(PANEL_PX.0, PANEL_PX.1, PANEL_MM.0, PANEL_MM.1),
            scale(1.5)
        );
        // Halving the height in millimetres doubles the vertical DPI.
        assert_eq!(
            screen_dpi_scale(PANEL_PX.0, PANEL_PX.1, PANEL_MM.0, PANEL_MM.1 / 2),
            None
        );
        assert_eq!(
            screen_dpi_scale(PANEL_PX.0, PANEL_PX.1, 0, PANEL_MM.1),
            None
        );
    }

    const LOGICAL: LogicalSize = LogicalSize { w: 1280, h: 720 };

    #[test]
    fn the_extent_carries_the_reported_scale_at_every_covered_scale() {
        let scales: Vec<Option<Scale>> = COVERED_SCALES
            .into_iter()
            .map(|s| Some(extent(LOGICAL.to_physical(s)?, s)?.scale()))
            .collect();
        assert_eq!(scales, COVERED_SCALES.map(Some).to_vec());
    }

    #[test]
    fn the_last_physical_pixel_maps_to_the_last_logical_pixel_at_every_covered_scale() {
        let agrees: Vec<Option<bool>> = COVERED_SCALES
            .into_iter()
            .map(|s| {
                let physical = LOGICAL.to_physical(s)?;
                let e = extent(physical, s)?;
                let logical = e.logical();
                let last = view_point(
                    Some(e),
                    PhysicalPoint {
                        x: physical.w - 1,
                        y: physical.h - 1,
                    },
                );
                Some(
                    last == LogicalPoint {
                        x: logical.w - 1,
                        y: logical.h - 1,
                    },
                )
            })
            .collect();
        assert_eq!(agrees, vec![Some(true); COVERED_SCALES.len()]);
    }

    #[test]
    fn a_pointer_maps_by_the_identity_before_the_first_published_extent() {
        assert_eq!(
            view_point(None, PhysicalPoint { x: 42, y: 7 }),
            LogicalPoint { x: 42, y: 7 }
        );
        assert_eq!(extent(PhysicalSize { w: 0, h: 720 }, Scale::ONE), None);
    }

    #[test]
    fn an_unanswered_probe_reports_one() {
        assert_eq!(unanswered(), Scale::ONE);
    }

    #[test]
    fn every_position_names_the_same_scale() {
        let reported = query_display_scale();
        let positions = [
            None,
            Some(WindowPos { x: 0, y: 0 }),
            Some(WindowPos { x: 1920, y: 1080 }),
            Some(WindowPos { x: -100, y: -100 }),
        ];
        assert_eq!(
            positions.map(display_scale).to_vec(),
            vec![reported; positions.len()]
        );
    }
}
