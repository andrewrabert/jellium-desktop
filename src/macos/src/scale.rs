//! The macOS scale rule: the `backingScaleFactor` AppKit reports, turned into
//! an exact [`Scale`], and this backend's own decision when it cannot be.

use std::ffi::c_int;

use jfn_platform_abi::Scale;
#[cfg(target_os = "macos")]
use jfn_platform_abi::WindowPos;

/// The exact value of a `backingScaleFactor`. `None` for a non-positive one.
pub fn scale_from_backing(factor: f64) -> Option<Scale> {
    Scale::from_f64(factor)
}

/// The scale macOS reports for the `backingScaleFactor` `source` gave.
///
/// `None` names a source that gave no factor at all. Logs the raw `CGFloat`
/// beside the value reported whenever the exact conversion rejects it, and
/// names the silent source when there is none; [`Scale::ONE`] is what this
/// backend reports in both cases.
pub fn report_backing(source: &str, factor: Option<f64>) -> Scale {
    if let Some(scale) = factor.and_then(scale_from_backing) {
        return scale;
    }
    let reported = Scale::ONE;
    match factor {
        Some(factor) => tracing::warn!(
            target: "Main",
            "{source} backingScaleFactor {factor} is not a scale; macOS reports {reported}"
        ),
        None => tracing::warn!(
            target: "Main",
            "{source} reported no backingScaleFactor; macOS reports {reported}"
        ),
    }
    reported
}

/// `points * scale`, integer round-half-up: the backing-pixel value a
/// `CGFloat` measured in points names.
///
/// `None` when `points` is not finite, or when the result does not fit
/// `c_int`.
pub fn to_backing(scale: Scale, points: f64) -> Option<c_int> {
    if !points.is_finite() {
        return None;
    }
    let exact = points * scale.as_f64();
    let rounded = (exact + 0.5).floor();
    if !rounded.is_finite() || rounded < f64::from(c_int::MIN) || rounded > f64::from(c_int::MAX) {
        return None;
    }
    Some(rounded as c_int)
}

/// The scale macOS reports for `at`.
///
/// A position in backing pixels names no `NSScreen` without screen-identity
/// persistence, so every position names the main screen's scale.
#[cfg(target_os = "macos")]
pub fn display_scale(at: Option<WindowPos>) -> Scale {
    tracing::trace!(
        target: "Main",
        "a backing-pixel position names no NSScreen, so {at:?} names the main screen's scale"
    );
    crate::backend::macos_display_scale()
}

#[cfg(test)]
mod tests {
    use super::*;
    use jfn_platform_abi::COVERED_SCALES;

    #[test]
    fn every_covered_scale_is_an_exact_backing_factor() {
        assert_eq!(
            COVERED_SCALES
                .map(|s| scale_from_backing(s.as_f64()))
                .to_vec(),
            COVERED_SCALES.map(Some).to_vec()
        );
    }

    #[test]
    fn a_non_positive_backing_factor_names_no_scale() {
        assert_eq!(scale_from_backing(0.0), None);
        assert_eq!(scale_from_backing(-2.0), None);
        assert_eq!(scale_from_backing(f64::NAN), None);
    }

    #[test]
    fn a_silent_source_reports_one() {
        assert_eq!(report_backing("test", None), Scale::ONE);
        assert_eq!(report_backing("test", Some(0.0)), Scale::ONE);
    }

    #[test]
    fn a_point_value_converts_by_round_half_up_at_every_covered_scale() {
        const POINTS: f64 = 337.5;
        let converted: Vec<Option<c_int>> = COVERED_SCALES
            .into_iter()
            .map(|s| to_backing(s, POINTS))
            .collect();
        let oracle: Vec<Option<c_int>> = COVERED_SCALES
            .into_iter()
            .map(|s| Some((POINTS * s.as_f64() + 0.5).floor() as c_int))
            .collect();
        assert_eq!(converted, oracle);
    }

    #[test]
    fn a_non_finite_point_value_names_no_backing_pixel() {
        assert_eq!(to_backing(Scale::ONE, f64::NAN), None);
        assert_eq!(to_backing(Scale::ONE, f64::INFINITY), None);
        assert_eq!(to_backing(Scale::ONE, f64::from(c_int::MAX) * 2.0), None);
    }
}
