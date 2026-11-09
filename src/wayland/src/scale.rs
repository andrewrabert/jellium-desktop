//! Fractional window scale in 120ths, the unit of `wp_fractional_scale_v1`
//! (120 = 1.0). [`Scale120`] owns protocol parsing, ratio conversion, and
//! checked dimension scaling, so a zero/negative/non-finite scale or an
//! unrepresentable physical extent cannot leave this module.

use std::fmt;
use std::num::{NonZeroU32, NonZeroU64};

use jfn_platform_abi::{LogicalSize, PhysicalSize, Scale, WindowExtent};

use crate::window_state::WindowSize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Scale120(NonZeroU32);

impl Scale120 {
    /// wp_fractional_scale reports scale in 120ths (120 = 1.0).
    pub(crate) const BASE: u32 = 120;

    /// Parse a `wp_fractional_scale_v1.preferred_scale` wire value (120ths;
    /// zero is invalid on the wire).
    pub(crate) fn from_wire(raw: u32) -> Option<Self> {
        NonZeroU32::new(raw).map(Self)
    }

    /// Exact rational physical/logical width, rounded to the nearest 120th —
    /// no float round-trip.
    pub(crate) fn from_physical_logical(physical: u32, logical: NonZeroU32) -> Option<Self> {
        let num = u64::from(physical).checked_mul(u64::from(Self::BASE))?;
        let den = u64::from(logical.get());
        let scaled = (num + den / 2) / den;
        Self::from_wire(u32::try_from(scaled).ok()?)
    }

    /// The exact 120ths this carries; infallible, the wire value is non-zero.
    pub(crate) fn scale(self) -> Scale {
        Scale::from_nonzero_ratio(NonZeroU64::from(self.0), BASE_NONZERO)
    }
}

/// [`Scale120::BASE`] as the denominator [`Scale::from_nonzero_ratio`] takes.
const BASE_NONZERO: NonZeroU64 = match NonZeroU64::new(Scale120::BASE as u64) {
    Some(d) => d,
    None => unreachable!(),
};

/// Whether this backend's unstated-scale decision has been logged.
///
/// Owned by the state whose absent scale it resolves, so `jfn-wayland` keeps
/// no module-level static and nothing reaches the flag ambiently.
pub(crate) struct UnstatedLog(bool);

impl UnstatedLog {
    pub(crate) fn new() -> UnstatedLog {
        UnstatedLog(false)
    }
}

/// 120 120ths. The one value `jfn-wayland` names for itself, spelled beside
/// the decision it belongs to.
const REPORTED: Scale120 = match NonZeroU32::new(Scale120::BASE) {
    Some(v) => Scale120(v),
    None => unreachable!(),
};

/// The scale Wayland reports when no compositor source has stated one.
///
/// The compositor states a scale through `wp_fractional_scale_v1` and, before
/// the surface exists, through the output probe. Logged the first time `log`
/// records the decision, whether the caller is a read that arrived before
/// either source spoke or the bring-up that found neither ever will.
pub(crate) fn unstated(log: &mut UnstatedLog) -> Scale120 {
    if !std::mem::replace(&mut log.0, true) {
        tracing::info!(
            target: "Main",
            "no compositor scale stated; Wayland reports {REPORTED}"
        );
    }
    REPORTED
}

/// The extent a compositor's own sizes and the scale it reported name.
///
/// The logical size is carried through verbatim.
///
/// `None` when the physical size and scale do not map to a logical one, or
/// when either axis is below two pixels.
pub(crate) fn extent(
    logical: WindowSize,
    physical: WindowSize,
    scale: Scale,
) -> Option<WindowExtent> {
    let physical = PhysicalSize {
        w: physical.w(),
        h: physical.h(),
    };
    physical.to_logical(scale)?;
    WindowExtent::new(
        physical,
        scale,
        LogicalSize {
            w: logical.w(),
            h: logical.h(),
        },
    )
}

impl fmt::Display for Scale120 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", f64::from(self.0.get()) / f64::from(Self::BASE))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire(raw: u32) -> Option<Scale120> {
        Scale120::from_wire(raw)
    }

    fn ratio(numerator: u64, denominator: u64) -> Option<Scale> {
        Scale::from_ratio(numerator, NonZeroU64::new(denominator)?)
    }

    #[test]
    fn wire_zero_rejected() {
        assert_eq!(Scale120::from_wire(0), None);
    }

    #[test]
    fn wire_value_reports_its_exact_120ths() {
        assert_eq!(wire(150).map(Scale120::scale), ratio(5, 4));
        assert_eq!(wire(120).map(Scale120::scale), Some(Scale::ONE));
        assert_eq!(wire(60).map(Scale120::scale), ratio(1, 2));
        assert_eq!(wire(90).map(Scale120::scale), ratio(3, 4));
        assert_eq!(wire(180).map(Scale120::scale), ratio(3, 2));
        assert_eq!(wire(240).map(Scale120::scale), ratio(2, 1));
    }

    #[test]
    fn rational_matches_exact_ratios() {
        let Some(logical) = NonZeroU32::new(1920) else {
            return;
        };
        assert_eq!(
            Scale120::from_physical_logical(1920, logical),
            Scale120::from_wire(Scale120::BASE)
        );
        assert_eq!(
            Scale120::from_physical_logical(2400, logical),
            Scale120::from_wire(150)
        );
        assert_eq!(
            Scale120::from_physical_logical(2880, logical),
            Scale120::from_wire(180)
        );
    }

    #[test]
    fn rational_rounds_half_up_and_rejects_zero() {
        assert_eq!(
            NonZeroU32::new(1).and_then(|l| Scale120::from_physical_logical(0, l)),
            None
        );
        // 1 physical / 240 logical = 0.5 in 120ths → rounds up to 1.
        assert_eq!(
            NonZeroU32::new(240).and_then(|l| Scale120::from_physical_logical(1, l)),
            Scale120::from_wire(1)
        );
    }

    #[test]
    fn rational_rejects_overflowing_result() {
        assert_eq!(
            NonZeroU32::new(1).and_then(|l| Scale120::from_physical_logical(u32::MAX, l)),
            None
        );
    }

    fn sizes(w: i32, h: i32) -> Option<WindowSize> {
        WindowSize::new(w, h)
    }

    #[test]
    fn the_extent_carries_the_reported_scale_at_every_covered_scale() {
        let logical = LogicalSize { w: 1280, h: 720 };
        let scales: Vec<Option<Scale>> = jfn_platform_abi::COVERED_SCALES
            .into_iter()
            .map(|s| {
                let physical = logical.to_physical(s)?;
                let e = extent(
                    sizes(logical.w, logical.h)?,
                    sizes(physical.w, physical.h)?,
                    s,
                )?;
                Some(e.scale())
            })
            .collect();
        assert_eq!(scales, jfn_platform_abi::COVERED_SCALES.map(Some).to_vec());
    }

    #[test]
    fn the_extent_carries_the_compositor_s_logical_size_verbatim() {
        // 1497 / 2.5 rounds to 599; the compositor's own 598 must survive.
        let Some(scale) = Scale::from_f64(2.5) else {
            return;
        };
        assert_eq!(
            sizes(598, 337)
                .zip(sizes(1497, 843))
                .and_then(|(l, p)| extent(l, p, scale))
                .map(|e| (e.logical(), e.physical())),
            Some((
                LogicalSize { w: 598, h: 337 },
                PhysicalSize { w: 1497, h: 843 }
            ))
        );
    }

    #[test]
    fn an_unstated_scale_reports_one_and_logs_on_the_first_read_only() {
        let mut log = UnstatedLog::new();
        assert_eq!(unstated(&mut log).scale(), Scale::ONE);
        assert!(log.0);
        assert_eq!(unstated(&mut log).scale(), Scale::ONE);
        assert!(log.0);
    }

    #[test]
    fn display_formats_as_ratio() {
        assert_eq!(
            Scale120::from_wire(Scale120::BASE).map(|s| s.to_string()),
            Some("1".to_owned())
        );
        assert_eq!(wire(150).map(|s| s.to_string()), Some("1.25".to_owned()));
    }

    fn lcg(seed: u64) -> impl FnMut() -> u64 {
        let mut s = seed;
        move || {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            s
        }
    }

    #[test]
    fn reported_scale_agrees_with_an_exact_rational_oracle() {
        let mut next = lcg(0x5CA1E);
        let check = |raw: u32, logical: i32| {
            let Some(scale) = Scale120::from_wire(raw) else {
                return;
            };
            let oracle = (i128::from(logical) * i128::from(raw) + 60) / 120;
            let physical = scale.scale().to_physical(logical);
            assert_eq!(
                physical.map(i128::from),
                i32::try_from(oracle).ok().map(i128::from),
                "raw={raw} logical={logical}"
            );
        };
        for raw in [1u32, 119, 120, 121, 240, u32::MAX] {
            for logical in [1i32, 2, 120, i32::MAX - 1, i32::MAX] {
                check(raw, logical);
            }
        }
        for _ in 0..10_000 {
            let raw = (next() % 1200 + 1) as u32;
            let logical = (next() % 20_000 + 1) as i32;
            check(raw, logical);
        }
    }

    #[test]
    fn from_physical_logical_agrees_with_exact_rational_oracle() {
        let mut next = lcg(0xF00D);
        let check = |physical: u32, logical: u32| {
            let Some(logical_nz) = NonZeroU32::new(logical) else {
                return;
            };
            let oracle =
                (u128::from(physical) * 120 + u128::from(logical) / 2) / u128::from(logical);
            match Scale120::from_physical_logical(physical, logical_nz) {
                Some(s) => assert_eq!(
                    u32::try_from(oracle).ok().and_then(Scale120::from_wire),
                    Some(s),
                    "{physical}/{logical}"
                ),
                None => assert!(
                    oracle == 0 || oracle > u128::from(u32::MAX),
                    "{physical}/{logical} rejected but oracle={oracle}"
                ),
            }
        };
        for physical in [0u32, 1, 119, 120, 1920, 3840, u32::MAX] {
            for logical in [1u32, 2, 120, 1920, u32::MAX] {
                check(physical, logical);
            }
        }
        for _ in 0..10_000 {
            let physical = (next() % 20_000) as u32;
            let logical = (next() % 20_000 + 1) as u32;
            check(physical, logical);
        }
    }

    #[test]
    fn scale_then_rederive_roundtrips_within_one_120th() {
        let mut next = lcg(0xB0BA);
        for _ in 0..10_000 {
            // Realistic display range: scales 0.5..=4.0, widths ≥ 120.
            let raw = (next() % 421 + 60) as u32;
            let w = (next() % 7_500 + 120) as i32;
            let (Some(scale), Some(logical_nz)) =
                (Scale120::from_wire(raw), NonZeroU32::new(w as u32))
            else {
                continue;
            };
            let Some(physical) = scale.scale().to_physical(w) else {
                continue;
            };
            let Ok(physical_u32) = u32::try_from(physical) else {
                continue;
            };
            let rederived = Scale120::from_physical_logical(physical_u32, logical_nz);
            // Rounding the physical size loses at most half a pixel, which for
            // widths ≥ 120 is at most one 120th of scale.
            assert!(
                [raw - 1, raw, raw + 1]
                    .into_iter()
                    .any(|cand| Scale120::from_wire(cand) == rederived),
                "raw={raw} w={w} rederived out of tolerance"
            );
        }
    }
}
