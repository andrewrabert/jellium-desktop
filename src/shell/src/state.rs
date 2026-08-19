use jfn_platform_abi::TITLEBAR_LOGICAL_HEIGHT;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ChromeInputs {
    pub client_side_decorations: bool,
    pub fullscreen: bool,
    pub video_active: bool,
    pub osd_visible: bool,
}

pub fn titlebar_shown(inputs: ChromeInputs) -> bool {
    inputs.client_side_decorations
        && !inputs.fullscreen
        && (!inputs.video_active || inputs.osd_visible)
}

pub fn overlay_visible(modal_occupied: bool, titlebar_shown: bool) -> bool {
    modal_occupied || titlebar_shown
}

/// The strip reserved above the web overlay, in logical pixels.
///
/// Reserved whenever decorations are client-side and the window is not
/// fullscreen, so the strip is held constant across every video and OSD
/// transition and Chromium is never resized by one.
pub fn reserved_strip(inputs: ChromeInputs) -> i32 {
    if inputs.client_side_decorations && !inputs.fullscreen {
        TITLEBAR_LOGICAL_HEIGHT
    } else {
        0
    }
}

/// The routing state a window's extent and the chrome over it name.
///
/// The size published is `extent`'s own logical size, never a re-derivation
/// of it; a window with no extent publishes zero.
pub fn shell_state(
    extent: Option<jfn_platform_abi::WindowExtent>,
    inputs: ChromeInputs,
    modal_open: bool,
) -> jfn_input::ShellState {
    let logical = extent.map(|e| e.logical());
    jfn_input::ShellState {
        modal_open,
        titlebar_shown: titlebar_shown(inputs),
        window_w: logical.map_or(0, |l| l.w),
        window_h: logical.map_or(0, |l| l.h),
        titlebar_h: TITLEBAR_LOGICAL_HEIGHT,
        controls_w: crate::chrome::CONTROLS_LOGICAL_WIDTH,
        reserved_strip: reserved_strip(inputs),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jfn_platform_abi::{COVERED_SCALES, LogicalSize, Scale, WindowExtent};

    const CSD: ChromeInputs = ChromeInputs {
        client_side_decorations: true,
        fullscreen: false,
        video_active: false,
        osd_visible: false,
    };

    #[test]
    fn titlebar_needs_client_side_decorations() {
        assert!(titlebar_shown(CSD));
        assert!(!titlebar_shown(ChromeInputs {
            client_side_decorations: false,
            ..CSD
        }));
    }

    #[test]
    fn fullscreen_hides_titlebar() {
        assert!(!titlebar_shown(ChromeInputs {
            fullscreen: true,
            ..CSD
        }));
    }

    #[test]
    fn video_hides_titlebar_unless_osd_is_up() {
        assert!(!titlebar_shown(ChromeInputs {
            video_active: true,
            ..CSD
        }));
        assert!(titlebar_shown(ChromeInputs {
            video_active: true,
            osd_visible: true,
            ..CSD
        }));
    }

    #[test]
    fn modal_shows_overlay_without_titlebar() {
        assert!(overlay_visible(true, false));
        assert!(overlay_visible(false, true));
        assert!(!overlay_visible(false, false));
    }

    #[test]
    fn inset_follows_decorations_and_fullscreen_only() {
        assert_eq!(reserved_strip(CSD), TITLEBAR_LOGICAL_HEIGHT);
        assert_eq!(
            reserved_strip(ChromeInputs {
                fullscreen: true,
                ..CSD
            }),
            0
        );
        assert_eq!(
            reserved_strip(ChromeInputs {
                client_side_decorations: false,
                ..CSD
            }),
            0
        );
    }

    #[test]
    fn the_published_size_is_the_extent_s_exact_logical_size_at_every_covered_scale() {
        const LOGICAL: LogicalSize = LogicalSize { w: 1280, h: 720 };
        let published: Vec<Option<(i32, i32)>> = COVERED_SCALES
            .into_iter()
            .map(|s| {
                let extent = WindowExtent::new(LOGICAL.to_physical(s)?, s, LOGICAL)?;
                let state = shell_state(Some(extent), CSD, false);
                Some((state.window_w, state.window_h))
            })
            .collect();
        assert_eq!(
            published,
            vec![Some((LOGICAL.w, LOGICAL.h)); COVERED_SCALES.len()]
        );
        let empty = shell_state(None, CSD, false);
        assert_eq!((empty.window_w, empty.window_h), (0, 0));
    }

    #[test]
    fn a_logical_size_division_cannot_reproduce_is_published_verbatim() {
        // 1497 / 2.5 rounds to 599; the producer's own 598 must survive.
        let Some(scale) = Scale::from_f64(2.5) else {
            return;
        };
        let extent = WindowExtent::new(
            jfn_platform_abi::PhysicalSize { w: 1497, h: 843 },
            scale,
            LogicalSize { w: 598, h: 337 },
        );
        let state = shell_state(extent, CSD, false);
        assert_eq!((state.window_w, state.window_h), (598, 337));
    }

    #[test]
    fn the_reserved_strip_survives_video_and_osd_transitions() {
        for video_active in [false, true] {
            for osd_visible in [false, true] {
                assert_eq!(
                    reserved_strip(ChromeInputs {
                        video_active,
                        osd_visible,
                        ..CSD
                    }),
                    TITLEBAR_LOGICAL_HEIGHT
                );
            }
        }
    }
}
