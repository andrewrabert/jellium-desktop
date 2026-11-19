//! The macOS scale rule: the `backingScaleFactor` AppKit reports, turned into
//! an exact [`Scale`], and this backend's own decision when it cannot be.

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
}
