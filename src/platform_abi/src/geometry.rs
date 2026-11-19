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

    pub fn to_physical(self, logical: c_int) -> Result<c_int, Unrepresentable> {
        let num = i128::from(logical) * i128::from(self.numerator.get());
        c_int::try_from(div_round_half_up(num, i128::from(self.denominator.get()))).map_err(|_| {
            Unrepresentable {
                value: logical,
                scale: self,
            }
        })
    }

    pub fn to_logical(self, physical: c_int) -> Result<c_int, Unrepresentable> {
        let num = i128::from(physical) * i128::from(self.denominator.get());
        c_int::try_from(div_round_half_up(num, i128::from(self.numerator.get()))).map_err(|_| {
            Unrepresentable {
                value: physical,
                scale: self,
            }
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, thiserror::Error)]
pub enum PointsToPixelsError {
    #[error("{points} points is not a finite length (scale {scale})")]
    NonFinite { points: f64, scale: Scale },
    #[error("{points} points at scale {scale} does not fit a pixel count")]
    DoesNotFit { points: f64, scale: Scale },
}

pub fn points_to_pixels(scale: Scale, points: f64) -> Result<c_int, PointsToPixelsError> {
    if !points.is_finite() {
        return Err(PointsToPixelsError::NonFinite { points, scale });
    }
    let exact = points * scale.as_f64();
    let rounded = (exact + 0.5).floor();
    if !rounded.is_finite() || rounded < f64::from(c_int::MIN) || rounded > f64::from(c_int::MAX) {
        return Err(PointsToPixelsError::DoesNotFit { points, scale });
    }
    Ok(rounded as c_int)
}

#[derive(Copy, Clone, Debug, PartialEq, thiserror::Error)]
#[error("{value} px does not convert at scale {scale}")]
pub struct Unrepresentable {
    pub value: c_int,
    pub scale: Scale,
}

#[derive(Copy, Clone, Debug, PartialEq, thiserror::Error)]
pub enum ExtentError {
    #[error(transparent)]
    Unrepresentable(#[from] Unrepresentable),
    #[error(
        "physical {}x{} or logical {}x{} has an axis below two pixels",
        physical.w, physical.h, logical.w, logical.h
    )]
    BelowTwoPixels {
        physical: PhysicalSize,
        logical: LogicalSize,
    },
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
    pub fn to_physical(self, scale: Scale) -> Result<PhysicalSize, Unrepresentable> {
        Ok(PhysicalSize {
            w: scale.to_physical(self.w)?,
            h: scale.to_physical(self.h)?,
        })
    }
}

impl PhysicalSize {
    pub fn to_logical(self, scale: Scale) -> Result<LogicalSize, Unrepresentable> {
        Ok(LogicalSize {
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

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct WindowPoint {
    x: f64,
    y: f64,
}

impl WindowPoint {
    pub fn from_window(x: f64, y: f64) -> WindowPoint {
        WindowPoint { x, y }
    }

    pub fn x(self) -> f64 {
        self.x
    }

    pub fn y(self) -> f64 {
        self.y
    }

    pub fn floor(self) -> (c_int, c_int) {
        (floor_axis(self.x), floor_axis(self.y))
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct WebPoint {
    x: f64,
    y: f64,
}

impl WebPoint {
    pub fn from_view(x: f64, y: f64) -> WebPoint {
        WebPoint { x, y }
    }

    pub fn x(self) -> f64 {
        self.x
    }

    pub fn y(self) -> f64 {
        self.y
    }

    pub fn floor(self) -> (c_int, c_int) {
        (floor_axis(self.x), floor_axis(self.y))
    }
}

fn floor_axis(v: f64) -> c_int {
    view_axis(v.floor())
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct WindowBasis(u64);

impl WindowBasis {
    pub const FIXED: WindowBasis = WindowBasis(0);

    pub fn next(self) -> WindowBasis {
        WindowBasis(self.0.wrapping_add(1))
    }
}

pub fn web_top(
    decorations: crate::EffectiveDecorations,
    fullscreen: bool,
    titlebar_present: bool,
) -> c_int {
    if titlebar_present && decorations == crate::EffectiveDecorations::ClientSide && !fullscreen {
        crate::TITLEBAR_LOGICAL_HEIGHT
    } else {
        0
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
    pub fn new(
        physical: PhysicalSize,
        scale: Scale,
        logical: LogicalSize,
    ) -> Result<Self, ExtentError> {
        if physical.w < 2 || physical.h < 2 || logical.w < 2 || logical.h < 2 {
            return Err(ExtentError::BelowTwoPixels { physical, logical });
        }
        Ok(Self {
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
#[derive(Clone, Debug, PartialEq)]
pub struct BootGeometry {
    logical: LogicalSize,
    physical: PhysicalSize,
    scale: Scale,
    /// `None` ⇒ let the window center (Wayland ignores position entirely).
    placement: Option<MpvPlacement>,
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
        clamped: PlacedGeometry,
        maximized: bool,
    ) -> Self {
        Self {
            logical,
            physical: PhysicalSize {
                w: clamped.w,
                h: clamped.h,
            },
            scale,
            placement: clamped.placement,
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
        self.placement.as_ref().map(MpvPlacement::position)
    }

    pub fn mpv_screen(&self) -> Option<&MpvScreen> {
        self.placement.as_ref().and_then(MpvPlacement::screen)
    }

    pub fn maximized(&self) -> bool {
        self.maximized
    }

    /// mpv `--geometry`: `"<W>x<H>"` or `"<W>x<H>+<X>+<Y>"`, physical pixels.
    pub fn mpv_geometry_string(&self) -> String {
        let mut s = format!("{}x{}", self.physical.w, self.physical.h);
        if let Some(p) = self.position() {
            s.push_str(&format!("+{}+{}", p.x, p.y));
        }
        s
    }

    pub fn force_position(&self) -> bool {
        self.placement.is_some()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ScreenName(String);

impl ScreenName {
    pub fn new(name: String) -> Option<ScreenName> {
        (!name.is_empty()).then_some(ScreenName(name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowPlacement {
    screen: Option<ScreenName>,
    position: WindowPos,
}

impl WindowPlacement {
    pub fn new(screen: Option<ScreenName>, position: WindowPos) -> Self {
        Self { screen, position }
    }

    pub fn screen(&self) -> Option<&ScreenName> {
        self.screen.as_ref()
    }

    pub fn position(&self) -> WindowPos {
        self.position
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MpvScreen {
    Index(usize),
    Name(ScreenName),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MpvPlacement {
    screen: Option<MpvScreen>,
    position: WindowPos,
}

impl MpvPlacement {
    pub fn screen(&self) -> Option<&MpvScreen> {
        self.screen.as_ref()
    }

    pub fn position(&self) -> WindowPos {
        self.position
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetScreen {
    pub mpv: MpvScreen,
    pub bounds: Bounds,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlacedGeometry {
    pub w: c_int,
    pub h: c_int,
    pub placement: Option<MpvPlacement>,
}

/// Working-area dimensions — excludes the menu bar / dock / taskbar — in the
/// same pixel space (backing pixels) as the geometry being clamped.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Bounds {
    pub w: c_int,
    pub h: c_int,
}

#[derive(Copy, Clone, Debug, PartialEq, thiserror::Error)]
pub enum ClampError {
    #[error("the visible frame names no pixel bounds: {0}")]
    VisibleFrame(#[from] PointsToPixelsError),
    #[error("the working area could not be read")]
    WorkArea,
    #[error("the monitors could not be enumerated")]
    Monitors,
    #[error("a monitor's info could not be read")]
    MonitorInfo,
}

/// Saved window geometry: size plus an optional top-left position. `None`
/// position asks [`clamp_to_bounds`] to center the window (mpv's own centering
/// misbehaves when only the width/height are overridden).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowGeometry {
    pub w: c_int,
    pub h: c_int,
    pub placement: Option<WindowPlacement>,
}

impl WindowGeometry {
    pub fn on_primary(&self) -> PlacedGeometry {
        PlacedGeometry {
            w: self.w,
            h: self.h,
            placement: self.placement.as_ref().map(|p| MpvPlacement {
                screen: None,
                position: p.position,
            }),
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct WindowPos {
    pub x: c_int,
    pub y: c_int,
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct SurfaceSize {
    pub extent: WindowExtent,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SurfacePlacement {
    Window,
    BelowTitlebar,
}

pub(crate) fn mpv_reconcile_size(
    reported: Scale,
    saved_logical: LogicalSize,
    saved_physical: PhysicalSize,
    locked: bool,
) -> Result<Option<PhysicalSize>, Unrepresentable> {
    if locked {
        return Ok(None);
    }
    let physical = saved_logical.to_physical(reported)?;
    Ok((physical != saved_physical).then_some(physical))
}

/// `g`'s size shrunk to fit `bounds`; its position is untouched.
///
/// An axis whose bound is not positive is left alone.
pub fn clamp_size_to_bounds(g: &WindowGeometry, bounds: Bounds) -> PlacedGeometry {
    PlacedGeometry {
        w: if bounds.w > 0 { g.w.min(bounds.w) } else { g.w },
        h: if bounds.h > 0 { g.h.min(bounds.h) } else { g.h },
        placement: g.on_primary().placement,
    }
}

pub fn clamp_to_screen(
    g: &WindowGeometry,
    find: impl FnOnce(&ScreenName) -> Result<Option<TargetScreen>, ClampError>,
    primary: impl FnOnce() -> Result<Option<Bounds>, ClampError>,
) -> Result<PlacedGeometry, ClampError> {
    if let Some(name) = g.placement.as_ref().and_then(WindowPlacement::screen)
        && let Some(target) = find(name)?
    {
        return Ok(clamp_to_bounds(g, Some(target.mpv), target.bounds));
    }
    Ok(match primary()? {
        Some(bounds) => clamp_to_bounds(g, None, bounds),
        None => g.on_primary(),
    })
}

fn clamp_to_bounds(
    g: &WindowGeometry,
    screen: Option<MpvScreen>,
    bounds: Bounds,
) -> PlacedGeometry {
    let vw = bounds.w;
    let vh = bounds.h;
    let w = g.w.min(vw);
    let h = g.h.min(vh);
    // Center an unset position; otherwise start from the requested one.
    let mut position = match &g.placement {
        Some(p) => p.position,
        None => WindowPos {
            x: (vw - w) / 2,
            y: (vh - h) / 2,
        },
    };
    if position.x + w > vw {
        position.x = vw - w;
    }
    if position.y + h > vh {
        position.y = vh - h;
    }
    if position.x < 0 {
        position.x = 0;
    }
    if position.y < 0 {
        position.y = 0;
    }
    PlacedGeometry {
        w,
        h,
        placement: Some(MpvPlacement { screen, position }),
    }
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
                Ok(logical),
                "at {scale}"
            );
        }
    }

    #[test]
    fn covered_scales_map_to_their_known_physical_sizes() {
        let logical = LogicalSize { w: 1280, h: 720 };
        let sizes: Vec<Result<PhysicalSize, Unrepresentable>> = covered()
            .into_iter()
            .map(|s| logical.to_physical(s))
            .collect();
        assert_eq!(
            sizes,
            vec![
                Ok(PhysicalSize { w: 640, h: 360 }),
                Ok(PhysicalSize { w: 960, h: 540 }),
                Ok(PhysicalSize { w: 1280, h: 720 }),
                Ok(PhysicalSize { w: 1600, h: 900 }),
                Ok(PhysicalSize { w: 1920, h: 1080 }),
                Ok(PhysicalSize { w: 2560, h: 1440 }),
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
        assert_eq!(
            WindowExtent::new(one, Scale::ONE, ok),
            Err(ExtentError::BelowTwoPixels {
                physical: one,
                logical: ok,
            })
        );
        let physical = PhysicalSize { w: 1280, h: 720 };
        let logical = LogicalSize { w: 1280, h: 1 };
        assert_eq!(
            WindowExtent::new(physical, Scale::ONE, logical),
            Err(ExtentError::BelowTwoPixels { physical, logical })
        );
    }

    #[test]
    fn the_last_physical_pixel_maps_to_the_last_logical_pixel_at_every_covered_scale() {
        let logical = LogicalSize { w: 1280, h: 720 };
        let corners: Vec<Result<(LogicalPoint, LogicalPoint), ExtentError>> = covered()
            .into_iter()
            .map(|scale| {
                let extent = WindowExtent::new(logical.to_physical(scale)?, scale, logical)?;
                let physical = extent.physical();
                Ok((
                    extent.to_logical_point(PhysicalPoint { x: 0, y: 0 }),
                    extent.to_logical_point(PhysicalPoint {
                        x: physical.w - 1,
                        y: physical.h - 1,
                    }),
                ))
            })
            .collect();
        let expected = Ok((
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
        let monotone: Vec<Result<bool, ExtentError>> = covered()
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
                Ok(ok)
            })
            .collect();
        assert_eq!(monotone, vec![Ok(true); COVERED_SCALES.len()]);
    }

    #[test]
    fn mpv_reconcile_declines_a_scale_that_maps_to_the_stored_size() {
        let logical = LogicalSize { w: 1280, h: 720 };
        let physical = PhysicalSize { w: 1280, h: 720 };
        assert_eq!(
            mpv_reconcile_size(Scale::ONE, logical, physical, false),
            Ok(None)
        );
        assert_eq!(
            ratio(5, 4).map(|s| mpv_reconcile_size(s, logical, physical, true)),
            Some(Ok(None))
        );
    }

    #[test]
    fn mpv_reconcile_resizes_when_the_reported_scale_maps_elsewhere() {
        let logical = LogicalSize { w: 1280, h: 720 };
        let physical = PhysicalSize { w: 1280, h: 720 };
        assert_eq!(
            ratio(5, 4).map(|s| mpv_reconcile_size(s, logical, physical, false)),
            Some(Ok(Some(PhysicalSize { w: 1600, h: 900 })))
        );
    }

    fn placed(w: c_int, h: c_int, x: c_int, y: c_int) -> WindowGeometry {
        WindowGeometry {
            w,
            h,
            placement: Some(WindowPlacement::new(None, WindowPos { x, y })),
        }
    }

    fn unplaced(w: c_int, h: c_int) -> WindowGeometry {
        WindowGeometry {
            w,
            h,
            placement: None,
        }
    }

    fn named(screen: &str, w: c_int, h: c_int, x: c_int, y: c_int) -> WindowGeometry {
        WindowGeometry {
            w,
            h,
            placement: Some(WindowPlacement::new(
                ScreenName::new(screen.to_owned()),
                WindowPos { x, y },
            )),
        }
    }

    #[test]
    fn mpv_geometry_string_with_and_without_position() {
        let logical = LogicalSize { w: 1280, h: 720 };
        let scale = Scale::from_f64(1.25);
        assert_eq!(scale, ratio(5, 4));
        let describe = |clamped: PlacedGeometry| {
            scale.map(|s| {
                let g = BootGeometry::from_clamped(logical, s, clamped, false);
                (g.mpv_geometry_string(), g.force_position())
            })
        };
        assert_eq!(
            describe(unplaced(1600, 900).on_primary()),
            Some(("1600x900".to_owned(), false))
        );
        assert_eq!(
            describe(placed(1600, 900, 100, 50).on_primary()),
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
        let g = placed(3000, 2000, 1500, 900);
        assert_eq!(
            clamp_size_to_bounds(&g, Bounds { w: 1920, h: 1080 }),
            placed(1920, 1080, 1500, 900).on_primary()
        );
    }

    #[test]
    fn the_size_clamp_keeps_a_negative_position() {
        let g = placed(3000, 2000, -1920, -40);
        assert_eq!(
            clamp_size_to_bounds(&g, Bounds { w: 1920, h: 1080 }),
            placed(1920, 1080, -1920, -40).on_primary()
        );
    }

    #[test]
    fn a_non_positive_bound_leaves_its_axis_alone() {
        let g = unplaced(3000, 2000);
        assert_eq!(
            clamp_size_to_bounds(&g, Bounds { w: 0, h: -5 }),
            g.on_primary()
        );
        assert_eq!(
            clamp_size_to_bounds(&g, Bounds { w: 1920, h: 0 }),
            unplaced(1920, 2000).on_primary()
        );
    }

    #[test]
    fn the_size_clamp_names_no_mpv_screen() {
        let g = named("DISPLAY2", 3000, 2000, 100, 50);
        assert_eq!(
            clamp_size_to_bounds(&g, Bounds { w: 1920, h: 1080 }),
            placed(1920, 1080, 100, 50).on_primary()
        );
    }

    const SCREEN: Bounds = Bounds { w: 1920, h: 1080 };

    #[test]
    fn fits_unchanged() {
        let g = placed(800, 600, 100, 50);
        assert_eq!(clamp_to_bounds(&g, None, SCREEN), g.on_primary());
    }

    #[test]
    fn oversized_shrinks_to_bounds() {
        let g = clamp_to_bounds(&placed(3000, 2000, 0, 0), None, SCREEN);
        assert_eq!(g.w, 1920);
        assert_eq!(g.h, 1080);
    }

    #[test]
    fn an_unset_position_centers() {
        let g = clamp_to_bounds(&unplaced(800, 600), None, SCREEN);
        assert_eq!(
            g.placement.map(|p| p.position()),
            Some(WindowPos {
                x: (1920 - 800) / 2,
                y: (1080 - 600) / 2
            })
        );
    }

    #[test]
    fn past_edge_pulls_back() {
        let g = clamp_to_bounds(&placed(800, 600, 1500, 900), None, SCREEN);
        assert_eq!(
            g.placement.map(|p| p.position()),
            Some(WindowPos {
                x: 1920 - 800,
                y: 1080 - 600
            })
        );
    }

    #[test]
    fn oversized_then_floored_at_origin() {
        let g = clamp_to_bounds(&unplaced(3000, 2000), None, SCREEN);
        assert_eq!(g, placed(1920, 1080, 0, 0).on_primary());
    }

    #[test]
    fn an_empty_screen_name_names_no_screen() {
        assert_eq!(ScreenName::new(String::new()), None);
        assert_eq!(
            ScreenName::new("DISPLAY2".to_owned()).map(|n| n.as_str().to_owned()),
            Some("DISPLAY2".to_owned())
        );
    }

    const SECOND: Bounds = Bounds { w: 2560, h: 1440 };

    fn second_screen(name: &ScreenName) -> Result<Option<TargetScreen>, ClampError> {
        Ok((name.as_str() == "DISPLAY2").then_some(TargetScreen {
            mpv: MpvScreen::Index(1),
            bounds: SECOND,
        }))
    }

    fn primary() -> Result<Option<Bounds>, ClampError> {
        Ok(Some(SCREEN))
    }

    fn refuse(_: &ScreenName) -> Result<Option<TargetScreen>, ClampError> {
        Err(ClampError::Monitors)
    }

    fn placed_on(
        screen: Option<MpvScreen>,
        w: c_int,
        h: c_int,
        x: c_int,
        y: c_int,
    ) -> PlacedGeometry {
        PlacedGeometry {
            w,
            h,
            placement: Some(MpvPlacement {
                screen,
                position: WindowPos { x, y },
            }),
        }
    }

    #[test]
    fn a_named_screen_bounds_the_clamp_and_names_the_mpv_screen() {
        let g = named("DISPLAY2", 800, 600, 1700, 800);
        assert_eq!(
            clamp_to_screen(&g, second_screen, primary),
            Ok(placed_on(Some(MpvScreen::Index(1)), 800, 600, 1700, 800))
        );
        let oversized = named("DISPLAY2", 3000, 2000, 100, 50);
        assert_eq!(
            clamp_to_screen(&oversized, second_screen, primary),
            Ok(placed_on(Some(MpvScreen::Index(1)), 2560, 1440, 0, 0))
        );
    }

    #[test]
    fn a_vanished_screen_clamps_on_the_primary_with_no_mpv_screen() {
        let g = named("DISPLAY3", 800, 600, 1700, 800);
        assert_eq!(
            clamp_to_screen(&g, second_screen, primary),
            Ok(placed_on(None, 800, 600, 1920 - 800, 1080 - 600))
        );
    }

    #[test]
    fn a_position_with_no_screen_looks_none_up() {
        assert_eq!(
            clamp_to_screen(&placed(800, 600, 1700, 800), refuse, primary),
            Ok(placed_on(None, 800, 600, 1920 - 800, 1080 - 600))
        );
        assert_eq!(
            clamp_to_screen(&unplaced(800, 600), refuse, primary),
            Ok(placed_on(
                None,
                800,
                600,
                (1920 - 800) / 2,
                (1080 - 600) / 2
            ))
        );
    }

    #[test]
    fn a_failed_screen_lookup_fails_the_clamp() {
        assert_eq!(
            clamp_to_screen(&named("DISPLAY2", 800, 600, 100, 50), refuse, primary),
            Err(ClampError::Monitors)
        );
    }

    #[test]
    fn no_primary_leaves_an_unmatched_geometry_as_saved() {
        let g = named("DISPLAY3", 3000, 2000, 1700, 800);
        assert_eq!(
            clamp_to_screen(&g, second_screen, || Ok(None)),
            Ok(g.on_primary())
        );
    }

    #[test]
    fn a_point_value_converts_by_round_half_up_at_every_covered_scale() {
        const POINTS: f64 = 337.5;
        let converted: Vec<Result<c_int, PointsToPixelsError>> = COVERED_SCALES
            .into_iter()
            .map(|s| points_to_pixels(s, POINTS))
            .collect();
        let oracle: Vec<Result<c_int, PointsToPixelsError>> = COVERED_SCALES
            .into_iter()
            .map(|s| Ok((POINTS * s.as_f64() + 0.5).floor() as c_int))
            .collect();
        assert_eq!(converted, oracle);
    }

    #[test]
    fn a_non_finite_point_value_is_non_finite() {
        assert!(matches!(
            points_to_pixels(Scale::ONE, f64::NAN),
            Err(PointsToPixelsError::NonFinite { points, scale: Scale::ONE }) if points.is_nan()
        ));
        assert_eq!(
            points_to_pixels(Scale::ONE, f64::INFINITY),
            Err(PointsToPixelsError::NonFinite {
                points: f64::INFINITY,
                scale: Scale::ONE
            })
        );
    }

    #[test]
    fn a_point_value_beyond_c_int_does_not_fit() {
        let points = f64::from(c_int::MAX) * 2.0;
        assert_eq!(
            points_to_pixels(Scale::ONE, points),
            Err(PointsToPixelsError::DoesNotFit {
                points,
                scale: Scale::ONE
            })
        );
        assert_eq!(
            points_to_pixels(Scale::ONE, -points),
            Err(PointsToPixelsError::DoesNotFit {
                points: -points,
                scale: Scale::ONE
            })
        );
    }
}
