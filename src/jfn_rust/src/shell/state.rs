use std::ffi::c_int;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ChromeInputs {
    pub video_active: bool,
    pub osd_visible: bool,
}

pub fn titlebar_interactive(inputs: ChromeInputs) -> bool {
    !inputs.video_active || inputs.osd_visible
}

pub fn titlebar_shown(web_top: c_int, inputs: ChromeInputs) -> bool {
    web_top > 0 && titlebar_interactive(inputs)
}

pub fn overlay_visible(modal_occupied: bool, titlebar_shown: bool) -> bool {
    modal_occupied || titlebar_shown
}

pub fn shell_state(inputs: ChromeInputs, modal_open: bool) -> jfn_input::ShellState {
    jfn_input::ShellState {
        modal_open,
        titlebar_interactive: titlebar_interactive(inputs),
        controls_w: crate::shell::chrome::CONTROLS_LOGICAL_WIDTH,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jfn_platform_abi::TITLEBAR_LOGICAL_HEIGHT;

    const IDLE: ChromeInputs = ChromeInputs {
        video_active: false,
        osd_visible: false,
    };

    #[test]
    fn titlebar_needs_a_reserved_strip() {
        assert!(titlebar_shown(TITLEBAR_LOGICAL_HEIGHT, IDLE));
        assert!(!titlebar_shown(0, IDLE));
    }

    #[test]
    fn video_hides_titlebar_unless_osd_is_up() {
        let video = ChromeInputs {
            video_active: true,
            ..IDLE
        };
        assert!(!titlebar_shown(TITLEBAR_LOGICAL_HEIGHT, video));
        assert!(titlebar_shown(
            TITLEBAR_LOGICAL_HEIGHT,
            ChromeInputs {
                osd_visible: true,
                ..video
            }
        ));
    }

    #[test]
    fn modal_shows_overlay_without_titlebar() {
        assert!(overlay_visible(true, false));
        assert!(overlay_visible(false, true));
        assert!(!overlay_visible(false, false));
    }

    #[test]
    fn the_published_state_carries_the_modal_and_the_video_state_only() {
        let state = shell_state(
            ChromeInputs {
                video_active: true,
                ..IDLE
            },
            true,
        );
        assert!(state.modal_open);
        assert!(!state.titlebar_interactive);
        assert_eq!(
            state.controls_w,
            crate::shell::chrome::CONTROLS_LOGICAL_WIDTH
        );
    }
}
