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

#[cfg(test)]
mod tests {
    use super::*;

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
