//! Window-geometry value types + on-screen clamping.
//!
//! The clamp algorithm was byte-identical in `macos_clamp_window_geometry`
//! and `win_clamp_window_geometry`; only the OS bounds query differed
//! (`NSScreen.visibleFrame * scale` vs `SPI_GETWORKAREA`). That query stays
//! platform-side and hands the resolved [`Bounds`] in, so the shared logic
//! is testable on any host.

use std::cmp::Ordering;
use std::ffi::c_int;
use std::num::NonZeroU64;

/// Physical pixels per logical pixel, exact.
///
/// Held as a reduced rational so a backend's own unit — 120ths, half-steps,
/// DPI/96, a backing factor — survives with no error, and so the conversions
/// below are integer arithmetic.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Scale {
    numerator: NonZeroU64,
    denominator: NonZeroU64,
}

/// `floor(num / den)` for a strictly positive `den`.
fn floor_div(num: i128, den: i128) -> i128 {
    let q = num / den;
    if num % den != 0 && num < 0 { q - 1 } else { q }
}

/// `floor(num / den + 1/2)` for a strictly positive `den`: round-half-up,
/// monotone over the whole input range.
fn div_round_half_up(num: i128, den: i128) -> i128 {
    floor_div(num * 2 + den, den * 2)
}

const fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a
}

/// `value / divisor`, where `divisor` divides `value`.
const fn reduce(value: NonZeroU64, divisor: u64) -> NonZeroU64 {
    match NonZeroU64::new(value.get() / divisor) {
        Some(reduced) => reduced,
        None => unreachable!(),
    }
}

impl Scale {
    pub const ONE: Scale = Scale {
        numerator: NonZeroU64::MIN,
        denominator: NonZeroU64::MIN,
    };

    /// `numerator / denominator`, reduced by their greatest common divisor.
    pub const fn from_nonzero_ratio(numerator: NonZeroU64, denominator: NonZeroU64) -> Scale {
        let g = gcd(numerator.get(), denominator.get());
        Scale {
            numerator: reduce(numerator, g),
            denominator: reduce(denominator, g),
        }
    }

    /// `numerator / denominator`, reduced by their greatest common divisor.
    /// `None` for a zero numerator.
    pub fn from_ratio(numerator: u64, denominator: NonZeroU64) -> Option<Scale> {
        Some(Scale::from_nonzero_ratio(
            NonZeroU64::new(numerator)?,
            denominator,
        ))
    }

    /// The exact value of `value`, which is a dyadic rational. `None` when
    /// `value` is not finite and greater than zero, or when its exact
    /// numerator or denominator exceeds `u64`.
    pub fn from_f64(value: f64) -> Option<Scale> {
        if !value.is_finite() || value <= 0.0 {
            return None;
        }
        let bits = value.to_bits();
        let biased = ((bits >> 52) & 0x7ff) as i32;
        let fraction = bits & ((1u64 << 52) - 1);
        let (mut mantissa, mut exponent) = if biased == 0 {
            (fraction, -1074i32)
        } else {
            (fraction | (1u64 << 52), biased - 1075)
        };
        let shift = mantissa.trailing_zeros();
        mantissa >>= shift;
        exponent += shift as i32;
        if exponent >= 0 {
            let factor = 1u64.checked_shl(u32::try_from(exponent).ok()?)?;
            Scale::from_ratio(mantissa.checked_mul(factor)?, NonZeroU64::MIN)
        } else {
            let spread = u32::try_from(-exponent).ok()?;
            let denominator = NonZeroU64::new(1u64.checked_shl(spread)?)?;
            Scale::from_ratio(mantissa, denominator)
        }
    }

    pub fn as_f32(self) -> f32 {
        self.as_f64() as f32
    }

    pub fn as_f64(self) -> f64 {
        self.numerator.get() as f64 / self.denominator.get() as f64
    }

    /// `logical * scale`, integer round-half-up. `None` when the result does
    /// not fit `c_int`.
    pub fn to_physical(self, logical: c_int) -> Option<c_int> {
        let num = i128::from(logical) * i128::from(self.numerator.get());
        c_int::try_from(div_round_half_up(num, i128::from(self.denominator.get()))).ok()
    }

    /// `physical / scale`, integer round-half-up. `None` when the result does
    /// not fit `c_int`.
    pub fn to_logical(self, physical: c_int) -> Option<c_int> {
        let num = i128::from(physical) * i128::from(self.denominator.get());
        c_int::try_from(div_round_half_up(num, i128::from(self.numerator.get()))).ok()
    }
}

/// `value`, which is not zero.
const fn nz(value: u64) -> NonZeroU64 {
    match NonZeroU64::new(value) {
        Some(v) => v,
        None => unreachable!(),
    }
}

/// The display scales this project covers end to end, as
/// `dev/requirements/the-display-scale-every-consumer-overrules.md` records
/// them: 0.5, 0.75, 1.0, 1.25, 1.5, 2.0.
pub const COVERED_SCALES: [Scale; 6] = [
    Scale::from_nonzero_ratio(nz(1), nz(2)),
    Scale::from_nonzero_ratio(nz(3), nz(4)),
    Scale::from_nonzero_ratio(nz(1), nz(1)),
    Scale::from_nonzero_ratio(nz(5), nz(4)),
    Scale::from_nonzero_ratio(nz(3), nz(2)),
    Scale::from_nonzero_ratio(nz(2), nz(1)),
];

impl PartialOrd for Scale {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Scale {
    fn cmp(&self, other: &Self) -> Ordering {
        let lhs = u128::from(self.numerator.get()) * u128::from(other.denominator.get());
        let rhs = u128::from(other.numerator.get()) * u128::from(self.denominator.get());
        lhs.cmp(&rhs)
    }
}

impl std::fmt::Display for Scale {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_f64())
    }
}

/// Window size in logical (DIP) pixels — the coordinate space the compositor
/// uses for the toplevel; the display scale maps it to physical pixels.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct LogicalSize {
    pub w: c_int,
    pub h: c_int,
}

/// Window size in physical (backing) pixels — what mpv's `--geometry` takes and
/// what gets persisted as `windowWidth/Height`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PhysicalSize {
    pub w: c_int,
    pub h: c_int,
}

impl LogicalSize {
    /// `None` when either axis of the result does not fit `c_int`.
    pub fn to_physical(self, scale: Scale) -> Option<PhysicalSize> {
        Some(PhysicalSize {
            w: scale.to_physical(self.w)?,
            h: scale.to_physical(self.h)?,
        })
    }
}

impl PhysicalSize {
    /// `None` when either axis of the result does not fit `c_int`.
    pub fn to_logical(self, scale: Scale) -> Option<LogicalSize> {
        Some(LogicalSize {
            w: scale.to_logical(self.w)?,
            h: scale.to_logical(self.h)?,
        })
    }
}

/// A point in physical (backing) pixels, relative to the window's client
/// origin — what Win32 mouse messages and X11 pointer events carry.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PhysicalPoint {
    pub x: c_int,
    pub y: c_int,
}

/// A point in logical (DIP) pixels — the space [`WindowExtent::logical`]
/// names, and the space CEF's view coordinates are in.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct LogicalPoint {
    pub x: c_int,
    pub y: c_int,
}

impl LogicalPoint {
    /// The point a view reports in its own logical coordinate space.
    ///
    /// Each axis is truncated toward zero and saturates at the [`c_int`]
    /// bounds; a non-finite axis names zero.
    pub fn from_view(x: f64, y: f64) -> LogicalPoint {
        LogicalPoint {
            x: view_axis(x),
            y: view_axis(y),
        }
    }
}

/// `v` truncated toward zero, saturating at the [`c_int`] bounds. A
/// non-finite `v` names zero.
fn view_axis(v: f64) -> c_int {
    if !v.is_finite() {
        return 0;
    }
    let truncated = v.trunc();
    if truncated <= f64::from(c_int::MIN) {
        return c_int::MIN;
    }
    if truncated >= f64::from(c_int::MAX) {
        return c_int::MAX;
    }
    truncated as c_int
}

/// A coherent (logical, physical, scale) triple.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct WindowExtent {
    logical: LogicalSize,
    physical: PhysicalSize,
    scale: Scale,
}

impl WindowExtent {
    /// The extent `physical`, `scale` and `logical` name.
    ///
    /// `logical` is the size its producer supplied; nothing here re-derives
    /// it.
    ///
    /// `None` when either axis of either size is below two pixels: a
    /// one-pixel axis has no second endpoint for
    /// [`WindowExtent::to_logical_point`] to map onto.
    pub fn new(physical: PhysicalSize, scale: Scale, logical: LogicalSize) -> Option<Self> {
        if physical.w < 2 || physical.h < 2 || logical.w < 2 || logical.h < 2 {
            return None;
        }
        Some(Self {
            logical,
            physical,
            scale,
        })
    }

    pub fn logical(&self) -> LogicalSize {
        self.logical
    }

    pub fn physical(&self) -> PhysicalSize {
        self.physical
    }

    pub fn scale(&self) -> Scale {
        self.scale
    }

    /// Map a pointer position into the space this extent's logical size names.
    ///
    /// Maps each axis endpoint-to-endpoint through this extent's own
    /// logical:physical pair, so the last physical row or column is the last
    /// logical one at every scale — including a producer's exact logical
    /// size, which division by [`WindowExtent::scale`] cannot reproduce.
    pub fn to_logical_point(&self, p: PhysicalPoint) -> LogicalPoint {
        LogicalPoint {
            x: map_axis(p.x, self.logical.w, self.physical.w),
            y: map_axis(p.y, self.logical.h, self.physical.h),
        }
    }
}

/// `v * (logical - 1) / (physical - 1)`, integer round-half-up, saturating
/// at the `c_int` bounds.
///
/// Total: [`WindowExtent`] admits no axis below two pixels, so neither
/// difference is zero. Monotone over the whole `c_int` range, so a point
/// dragged past the client origin stays monotone.
fn map_axis(v: c_int, logical: c_int, physical: c_int) -> c_int {
    let num = i128::from(v) * i128::from(logical - 1);
    let mapped = div_round_half_up(num, i128::from(physical - 1));
    mapped.clamp(i128::from(c_int::MIN), i128::from(c_int::MAX)) as c_int
}

/// Fully-resolved boot geometry: one typed value computed once from saved
/// config, consumed by `WindowOwner::apply_boot_geometry`: it seeds an
/// app-created window (logical) or hands mpv its `--geometry` (physical).
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct BootGeometry {
    logical: LogicalSize,
    physical: PhysicalSize,
    scale: Scale,
    /// `None` ⇒ let the window center (Wayland ignores position entirely).
    position: Option<WindowPos>,
    maximized: bool,
}

impl BootGeometry {
    /// The one constructor: `physical` and `position` are both taken from a
    /// single already-clamped [`WindowGeometry`], so they cannot disagree with
    /// each other or be set independently of the clamp. `scale` is the factor
    /// that produced `clamped` from `logical`.
    pub fn from_clamped(
        logical: LogicalSize,
        scale: Scale,
        clamped: WindowGeometry,
        maximized: bool,
    ) -> Self {
        Self {
            logical,
            physical: PhysicalSize {
                w: clamped.w,
                h: clamped.h,
            },
            scale,
            position: clamped.position,
            maximized,
        }
    }

    pub fn logical(&self) -> LogicalSize {
        self.logical
    }

    pub fn physical(&self) -> PhysicalSize {
        self.physical
    }

    pub fn scale(&self) -> Scale {
        self.scale
    }

    pub fn position(&self) -> Option<WindowPos> {
        self.position
    }

    pub fn maximized(&self) -> bool {
        self.maximized
    }

    /// mpv `--geometry`: `"<W>x<H>"` or `"<W>x<H>+<X>+<Y>"`, physical pixels.
    pub fn mpv_geometry_string(&self) -> String {
        let mut s = format!("{}x{}", self.physical.w, self.physical.h);
        if let Some(p) = self.position {
            s.push_str(&format!("+{}+{}", p.x, p.y));
        }
        s
    }

    pub fn force_position(&self) -> bool {
        self.position.is_some()
    }
}

/// Working-area dimensions — excludes the menu bar / dock / taskbar — in the
/// same pixel space (backing pixels) as the geometry being clamped.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Bounds {
    pub w: c_int,
    pub h: c_int,
}

/// Saved window geometry: size plus an optional top-left position. `None`
/// position asks [`clamp_to_bounds`] to center the window (mpv's own centering
/// misbehaves when only the width/height are overridden).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct WindowGeometry {
    pub w: c_int,
    pub h: c_int,
    pub position: Option<WindowPos>,
}

impl WindowGeometry {
    /// Build from raw coordinates where a negative `x` or `y` means "unset".
    /// The single home for that OS/config-facing sentinel convention.
    pub fn from_raw(w: c_int, h: c_int, x: c_int, y: c_int) -> Self {
        Self {
            w,
            h,
            position: (x >= 0 && y >= 0).then_some(WindowPos { x, y }),
        }
    }

    /// Raw coordinates for OS APIs that take a sentinel; `(-1, -1)` when unset.
    pub fn raw_position(&self) -> (c_int, c_int) {
        self.position.map_or((-1, -1), |p| (p.x, p.y))
    }
}

/// A window's top-left position, in the coordinate space the backend
/// reports (backing pixels relative to the working area). Returned by
/// `Platform::query_window_position`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct WindowPos {
    pub x: c_int,
    pub y: c_int,
}

/// A surface's own coherent size, the scale it is presented at, and the strip
/// of the window above it.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct SurfaceSize {
    pub extent: WindowExtent,
    /// Offset of the surface's top edge from the window's top edge.
    pub logical_top: c_int,
    pub physical_top: c_int,
}

/// The physical size mpv's window is resized to at boot.
///
/// `None` when `locked`, when the saved logical size does not map to a
/// representable physical one, and when the size it maps to is the one the
/// saved geometry already records.
pub(crate) fn mpv_reconcile_size(
    reported: Scale,
    saved_logical: LogicalSize,
    saved_physical: PhysicalSize,
    locked: bool,
) -> Option<PhysicalSize> {
    if locked {
        return None;
    }
    let physical = saved_logical.to_physical(reported)?;
    (physical != saved_physical).then_some(physical)
}

/// `g`'s size shrunk to fit `bounds`; its position is untouched.
///
/// An axis whose bound is not positive is left alone.
pub fn clamp_size_to_bounds(g: WindowGeometry, bounds: Bounds) -> WindowGeometry {
    WindowGeometry {
        w: if bounds.w > 0 { g.w.min(bounds.w) } else { g.w },
        h: if bounds.h > 0 { g.h.min(bounds.h) } else { g.h },
        position: g.position,
    }
}

/// Clamp `g` so the window stays fully within `bounds`: shrink oversized
/// dimensions, center any unset (negative) axis, pull a past-the-edge window
/// back in-bounds, then floor at the origin. Byte-for-byte the former
/// per-platform clamp.
pub fn clamp_to_bounds(g: &mut WindowGeometry, bounds: Bounds) {
    let vw = bounds.w;
    let vh = bounds.h;
    if g.w > vw {
        g.w = vw;
    }
    if g.h > vh {
        g.h = vh;
    }
    // Center an unset position; otherwise start from the requested one.
    let (mut x, mut y) = match g.position {
        Some(p) => (p.x, p.y),
        None => ((vw - g.w) / 2, (vh - g.h) / 2),
    };
    if x + g.w > vw {
        x = vw - g.w;
    }
    if y + g.h > vh {
        y = vh - g.h;
    }
    if x < 0 {
        x = 0;
    }
    if y < 0 {
        y = 0;
    }
    g.position = Some(WindowPos { x, y });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn covered() -> Vec<Scale> {
        COVERED_SCALES.to_vec()
    }

    fn ratio(numerator: u64, denominator: u64) -> Option<Scale> {
        Scale::from_ratio(numerator, NonZeroU64::new(denominator)?)
    }

    #[test]
    fn covered_scales_are_exact_reduced_rationals() {
        let ratios: Vec<Option<Scale>> = covered().into_iter().map(Some).collect();
        assert_eq!(
            ratios,
            vec![
                ratio(1, 2),
                ratio(3, 4),
                ratio(1, 1),
                ratio(5, 4),
                ratio(3, 2),
                ratio(2, 1),
            ]
        );
    }

    #[test]
    fn logical_physical_round_trips_at_every_covered_scale() {
        let logical = LogicalSize { w: 1280, h: 720 };
        for scale in covered() {
            let physical = logical.to_physical(scale);
            assert_eq!(
                physical.and_then(|p| p.to_logical(scale)),
                Some(logical),
                "at {scale}"
            );
        }
    }

    #[test]
    fn covered_scales_map_to_their_known_physical_sizes() {
        let logical = LogicalSize { w: 1280, h: 720 };
        let sizes: Vec<Option<PhysicalSize>> = covered()
            .into_iter()
            .map(|s| logical.to_physical(s))
            .collect();
        assert_eq!(
            sizes,
            vec![
                Some(PhysicalSize { w: 640, h: 360 }),
                Some(PhysicalSize { w: 960, h: 540 }),
                Some(PhysicalSize { w: 1280, h: 720 }),
                Some(PhysicalSize { w: 1600, h: 900 }),
                Some(PhysicalSize { w: 1920, h: 1080 }),
                Some(PhysicalSize { w: 2560, h: 1440 }),
            ]
        );
    }

    #[test]
    fn scale_rejects_non_positive_and_non_finite_values() {
        assert_eq!(Scale::from_f64(0.0), None);
        assert_eq!(Scale::from_f64(-2.0), None);
        assert_eq!(Scale::from_f64(f64::NAN), None);
        assert_eq!(Scale::from_f64(f64::INFINITY), None);
        assert_eq!(Scale::from_ratio(0, NonZeroU64::MIN), None);
    }

    #[test]
    fn equal_ratios_reduce_to_one_value_and_order_by_magnitude() {
        assert_eq!(ratio(6, 4), Scale::from_f64(1.5));
        assert_eq!(ratio(7, 7), Some(Scale::ONE));
        let mut sorted = covered();
        sorted.sort();
        assert_eq!(sorted, covered());
        assert!(ratio(1, 2) < ratio(2, 1));
    }

    #[test]
    fn an_axis_below_two_pixels_names_no_extent() {
        let one = PhysicalSize { w: 1, h: 720 };
        let ok = LogicalSize { w: 1280, h: 720 };
        assert_eq!(WindowExtent::new(one, Scale::ONE, ok), None);
        assert_eq!(
            WindowExtent::new(
                PhysicalSize { w: 1280, h: 720 },
                Scale::ONE,
                LogicalSize { w: 1280, h: 1 },
            ),
            None
        );
    }

    #[test]
    fn the_last_physical_pixel_maps_to_the_last_logical_pixel_at_every_covered_scale() {
        let logical = LogicalSize { w: 1280, h: 720 };
        let corners: Vec<Option<(LogicalPoint, LogicalPoint)>> = covered()
            .into_iter()
            .map(|scale| {
                let extent = WindowExtent::new(logical.to_physical(scale)?, scale, logical)?;
                let physical = extent.physical();
                Some((
                    extent.to_logical_point(PhysicalPoint { x: 0, y: 0 }),
                    extent.to_logical_point(PhysicalPoint {
                        x: physical.w - 1,
                        y: physical.h - 1,
                    }),
                ))
            })
            .collect();
        let expected = Some((
            LogicalPoint { x: 0, y: 0 },
            LogicalPoint {
                x: logical.w - 1,
                y: logical.h - 1,
            },
        ));
        assert_eq!(corners, vec![expected; COVERED_SCALES.len()]);
    }

    #[test]
    fn pointer_map_stays_monotone_past_the_client_origin() {
        let logical = LogicalSize { w: 1280, h: 720 };
        let monotone: Vec<Option<bool>> = covered()
            .into_iter()
            .map(|scale| {
                let extent = WindowExtent::new(logical.to_physical(scale)?, scale, logical)?;
                let mut previous = c_int::MIN;
                let mut ok = true;
                for x in -16..(extent.physical().w + 16) {
                    let mapped = extent.to_logical_point(PhysicalPoint { x, y: 0 }).x;
                    ok &= mapped >= previous;
                    previous = mapped;
                }
                Some(ok)
            })
            .collect();
        assert_eq!(monotone, vec![Some(true); COVERED_SCALES.len()]);
    }

    #[test]
    fn mpv_reconcile_declines_a_scale_that_maps_to_the_stored_size() {
        let logical = LogicalSize { w: 1280, h: 720 };
        let physical = PhysicalSize { w: 1280, h: 720 };
        assert_eq!(
            mpv_reconcile_size(Scale::ONE, logical, physical, false),
            None
        );
        assert_eq!(
            ratio(5, 4).and_then(|s| mpv_reconcile_size(s, logical, physical, true)),
            None
        );
    }

    #[test]
    fn mpv_reconcile_resizes_when_the_reported_scale_maps_elsewhere() {
        let logical = LogicalSize { w: 1280, h: 720 };
        let physical = PhysicalSize { w: 1280, h: 720 };
        assert_eq!(
            ratio(5, 4).and_then(|s| mpv_reconcile_size(s, logical, physical, false)),
            Some(PhysicalSize { w: 1600, h: 900 })
        );
    }

    #[test]
    fn mpv_geometry_string_with_and_without_position() {
        let logical = LogicalSize { w: 1280, h: 720 };
        let scale = Scale::from_f64(1.25);
        assert_eq!(scale, ratio(5, 4));
        let describe = |x: c_int, y: c_int| {
            scale.map(|s| {
                let g = BootGeometry::from_clamped(
                    logical,
                    s,
                    WindowGeometry::from_raw(1600, 900, x, y),
                    false,
                );
                (g.mpv_geometry_string(), g.force_position())
            })
        };
        assert_eq!(describe(-1, -1), Some(("1600x900".to_owned(), false)));
        assert_eq!(
            describe(100, 50),
            Some(("1600x900+100+50".to_owned(), true))
        );
    }

    #[test]
    fn a_view_position_truncates_toward_zero_and_saturates() {
        assert_eq!(
            LogicalPoint::from_view(3.9, -3.9),
            LogicalPoint { x: 3, y: -3 }
        );
        assert_eq!(
            LogicalPoint::from_view(0.0, -0.5),
            LogicalPoint { x: 0, y: 0 }
        );
        assert_eq!(
            LogicalPoint::from_view(f64::MAX, f64::MIN),
            LogicalPoint {
                x: c_int::MAX,
                y: c_int::MIN
            }
        );
    }

    #[test]
    fn a_non_finite_view_axis_names_zero() {
        assert_eq!(
            LogicalPoint::from_view(f64::NAN, f64::INFINITY),
            LogicalPoint { x: 0, y: 0 }
        );
        assert_eq!(
            LogicalPoint::from_view(f64::NEG_INFINITY, 7.5),
            LogicalPoint { x: 0, y: 7 }
        );
    }

    #[test]
    fn the_size_clamp_shrinks_without_moving_the_window() {
        let g = WindowGeometry::from_raw(3000, 2000, 1500, 900);
        assert_eq!(
            clamp_size_to_bounds(g, Bounds { w: 1920, h: 1080 }),
            WindowGeometry::from_raw(1920, 1080, 1500, 900)
        );
    }

    #[test]
    fn a_non_positive_bound_leaves_its_axis_alone() {
        let g = WindowGeometry::from_raw(3000, 2000, -1, -1);
        assert_eq!(clamp_size_to_bounds(g, Bounds { w: 0, h: -5 }), g);
        assert_eq!(
            clamp_size_to_bounds(g, Bounds { w: 1920, h: 0 }),
            WindowGeometry::from_raw(1920, 2000, -1, -1)
        );
    }

    const SCREEN: Bounds = Bounds { w: 1920, h: 1080 };

    fn pos(g: &WindowGeometry) -> (c_int, c_int) {
        g.raw_position()
    }

    #[test]
    fn fits_unchanged() {
        let mut g = WindowGeometry::from_raw(800, 600, 100, 50);
        clamp_to_bounds(&mut g, SCREEN);
        assert_eq!(g, WindowGeometry::from_raw(800, 600, 100, 50));
    }

    #[test]
    fn oversized_shrinks_to_bounds() {
        let mut g = WindowGeometry::from_raw(3000, 2000, 0, 0);
        clamp_to_bounds(&mut g, SCREEN);
        assert_eq!(g.w, 1920);
        assert_eq!(g.h, 1080);
    }

    #[test]
    fn unset_axes_center() {
        let mut g = WindowGeometry::from_raw(800, 600, -1, -1);
        clamp_to_bounds(&mut g, SCREEN);
        assert_eq!(pos(&g), ((1920 - 800) / 2, (1080 - 600) / 2));
    }

    #[test]
    fn past_edge_pulls_back() {
        let mut g = WindowGeometry::from_raw(800, 600, 1500, 900);
        clamp_to_bounds(&mut g, SCREEN);
        assert_eq!(pos(&g), (1920 - 800, 1080 - 600));
    }

    #[test]
    fn oversized_then_floored_at_origin() {
        let mut g = WindowGeometry::from_raw(3000, 2000, -1, -1);
        clamp_to_bounds(&mut g, SCREEN);
        assert_eq!(g, WindowGeometry::from_raw(1920, 1080, 0, 0));
    }
}
