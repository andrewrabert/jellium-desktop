//! Where an input event goes.
//!
//! A pure function of the shell overlay's state and the pointer position:
//! no display server, no GPU, no CEF process.

use jfn_platform_abi::LogicalPoint;
use std::ffi::c_int;

/// Resize-grip thickness on an edge, logical pixels.
pub const EDGE_LOGICAL: c_int = 8;

/// Resize-grip box at a corner, logical pixels.
pub const CORNER_LOGICAL: c_int = 20;

const EDGE_TOP: c_int = 1;
const EDGE_BOTTOM: c_int = 2;
const EDGE_LEFT: c_int = 4;
const EDGE_RIGHT: c_int = 8;

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Target {
    Shell,
    Web,
    None,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct ShellState {
    pub modal_open: bool,
    pub titlebar_shown: bool,
    pub window_w: c_int,
    pub window_h: c_int,
    pub titlebar_h: c_int,
    /// Width of the minimize/maximize/close strip at the titlebar's right edge.
    pub controls_w: c_int,
    /// Logical height of the strip the shell overlay reserves above the web
    /// overlay. Held across video and OSD transitions.
    pub reserved_strip: c_int,
}

/// What the shell overlay owns at `p`. Grips win over the bar at overlaps.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ShellHit {
    /// A modal view owns the whole window.
    Modal,
    /// A resize grip; the payload is the xdg_toplevel resize-edge mask,
    /// top=1 bottom=2 left=4 right=8, corners the ORs.
    Grip(c_int),
    /// The titlebar's drag region.
    Drag,
    /// The titlebar's window-control buttons.
    Controls,
    /// jellyfin-web owns it.
    Miss,
}

/// A modal takes everything. Otherwise the resize grips take the pointer
/// first, then the titlebar strip splits into its controls and its drag
/// region; with no titlebar the shell takes nothing.
pub fn hit(state: ShellState, p: LogicalPoint) -> ShellHit {
    if state.modal_open {
        return ShellHit::Modal;
    }
    if !state.titlebar_shown {
        return ShellHit::Miss;
    }
    if let Some(edge) = resize_edge(state, p) {
        return ShellHit::Grip(edge);
    }
    if p.y < 0 || p.y >= state.titlebar_h {
        return ShellHit::Miss;
    }
    if state.controls_w > 0 && p.x >= state.window_w - state.controls_w {
        return ShellHit::Controls;
    }
    ShellHit::Drag
}

/// `Target::Shell` for every hit but [`ShellHit::Miss`].
pub fn route_pointer(state: ShellState, p: LogicalPoint) -> Target {
    match hit(state, p) {
        ShellHit::Miss => Target::Web,
        _ => Target::Shell,
    }
}

/// A modal takes every key; otherwise every key goes to jellyfin-web.
pub fn route_key(state: ShellState) -> Target {
    if state.modal_open {
        Target::Shell
    } else {
        Target::Web
    }
}

/// The window's own focus while no modal owns input, never while one does.
pub fn web_focus(window_focused: bool, modal_open: bool) -> bool {
    window_focused && !modal_open
}

// text the shell overlay's focused widget inserts
// a control codepoint restates the named key the key event already carried
// a system char is Windows' WM_SYSCHAR, an Alt-modified accelerator
// Command, or Ctrl without Alt, is a shortcut; Ctrl with Alt is AltGr and types
pub fn is_text(ch: char, modifiers: u32, is_system_key: bool) -> bool {
    use jfn_platform_abi::event_flags as ef;
    let held = |flag: u32| modifiers & flag != 0;
    !ch.is_control()
        && !is_system_key
        && !held(ef::EVENTFLAG_COMMAND_DOWN)
        && !(held(ef::EVENTFLAG_CONTROL_DOWN) && !held(ef::EVENTFLAG_ALT_DOWN))
}

/// The xdg_toplevel resize-edge mask under `p`, or `None`.
/// top=1 bottom=2 left=4 right=8; corners are the ORs.
pub fn resize_edge(state: ShellState, p: LogicalPoint) -> Option<c_int> {
    if state.modal_open || !state.titlebar_shown {
        return None;
    }
    let (w, h) = (state.window_w, state.window_h);
    if w <= 0 || h <= 0 || p.x < 0 || p.y < 0 || p.x >= w || p.y >= h {
        return None;
    }
    let mut mask = 0;
    if p.y < EDGE_LOGICAL {
        mask |= EDGE_TOP;
    }
    if p.y >= h - EDGE_LOGICAL {
        mask |= EDGE_BOTTOM;
    }
    if p.x < EDGE_LOGICAL {
        mask |= EDGE_LEFT;
    }
    if p.x >= w - EDGE_LOGICAL {
        mask |= EDGE_RIGHT;
    }
    if mask == EDGE_TOP || mask == EDGE_BOTTOM {
        if p.x < CORNER_LOGICAL {
            mask |= EDGE_LEFT;
        } else if p.x >= w - CORNER_LOGICAL {
            mask |= EDGE_RIGHT;
        }
    } else if mask == EDGE_LEFT || mask == EDGE_RIGHT {
        if p.y < CORNER_LOGICAL {
            mask |= EDGE_TOP;
        } else if p.y >= h - CORNER_LOGICAL {
            mask |= EDGE_BOTTOM;
        }
    }
    if mask == 0 { None } else { Some(mask) }
}

/// Window-space `p` translated into the web overlay's own space.
pub fn to_web_point(p: LogicalPoint, state: ShellState) -> LogicalPoint {
    LogicalPoint {
        x: p.x,
        y: p.y - state.reserved_strip,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jfn_platform_abi::event_flags::{
        EVENTFLAG_ALT_DOWN, EVENTFLAG_COMMAND_DOWN, EVENTFLAG_CONTROL_DOWN, EVENTFLAG_SHIFT_DOWN,
    };

    fn chrome() -> ShellState {
        ShellState {
            modal_open: false,
            titlebar_shown: true,
            window_w: 800,
            window_h: 600,
            titlebar_h: 32,
            controls_w: 138,
            reserved_strip: 32,
        }
    }

    fn at(x: c_int, y: c_int) -> LogicalPoint {
        LogicalPoint { x, y }
    }

    #[test]
    fn modal_takes_everything() {
        let s = ShellState {
            modal_open: true,
            ..chrome()
        };
        assert_eq!(route_pointer(s, at(400, 400)), Target::Shell);
        assert_eq!(route_key(s), Target::Shell);
        assert_eq!(resize_edge(s, at(0, 0)), None);
    }

    #[test]
    fn bare_window_gives_everything_to_web() {
        let s = ShellState::default();
        assert_eq!(route_pointer(s, at(0, 0)), Target::Web);
        assert_eq!(route_key(s), Target::Web);
    }

    #[test]
    fn titlebar_strip_goes_to_shell_and_the_page_below_does_not() {
        assert_eq!(route_pointer(chrome(), at(400, 10)), Target::Shell);
        assert_eq!(route_pointer(chrome(), at(400, 31)), Target::Shell);
        assert_eq!(route_pointer(chrome(), at(400, 32)), Target::Web);
    }

    #[test]
    fn keys_always_reach_the_page_without_a_modal() {
        assert_eq!(route_key(chrome()), Target::Web);
    }

    #[test]
    fn edges_are_eight_logical_pixels() {
        assert_eq!(resize_edge(chrome(), at(400, 599)), Some(EDGE_BOTTOM));
        assert_eq!(resize_edge(chrome(), at(400, 592)), Some(EDGE_BOTTOM));
        assert_eq!(resize_edge(chrome(), at(400, 591)), None);
        assert_eq!(route_pointer(chrome(), at(400, 599)), Target::Shell);
        assert_eq!(route_pointer(chrome(), at(400, 591)), Target::Web);
    }

    #[test]
    fn corners_are_twenty_logical_pixels() {
        assert_eq!(resize_edge(chrome(), at(2, 2)), Some(EDGE_TOP | EDGE_LEFT));
        assert_eq!(
            resize_edge(chrome(), at(799, 599)),
            Some(EDGE_BOTTOM | EDGE_RIGHT)
        );
        assert_eq!(resize_edge(chrome(), at(19, 2)), Some(EDGE_TOP | EDGE_LEFT));
        assert_eq!(resize_edge(chrome(), at(20, 2)), Some(EDGE_TOP));
        assert_eq!(resize_edge(chrome(), at(2, 19)), Some(EDGE_TOP | EDGE_LEFT));
        assert_eq!(resize_edge(chrome(), at(2, 20)), Some(EDGE_LEFT));
    }

    #[test]
    fn no_titlebar_means_no_grips() {
        let s = ShellState {
            titlebar_shown: false,
            ..chrome()
        };
        assert_eq!(resize_edge(s, at(0, 0)), None);
        assert_eq!(route_pointer(s, at(0, 0)), Target::Web);
    }

    #[test]
    fn outside_the_window_has_no_grip() {
        assert_eq!(resize_edge(chrome(), at(-1, 0)), None);
        assert_eq!(resize_edge(chrome(), at(800, 600)), None);
    }

    #[test]
    fn the_bar_splits_into_a_drag_region_and_its_controls() {
        assert_eq!(hit(chrome(), at(400, 10)), ShellHit::Drag);
        assert_eq!(hit(chrome(), at(661, 10)), ShellHit::Drag);
        assert_eq!(hit(chrome(), at(662, 10)), ShellHit::Controls);
        assert_eq!(hit(chrome(), at(400, 40)), ShellHit::Miss);
    }

    #[test]
    fn grips_win_over_the_bar() {
        assert_eq!(
            hit(chrome(), at(799, 2)),
            ShellHit::Grip(EDGE_TOP | EDGE_RIGHT)
        );
        assert_eq!(hit(chrome(), at(400, 2)), ShellHit::Grip(EDGE_TOP));
    }

    #[test]
    fn a_modal_owns_the_whole_window() {
        let s = ShellState {
            modal_open: true,
            ..chrome()
        };
        assert_eq!(hit(s, at(400, 400)), ShellHit::Modal);
        assert_eq!(hit(s, at(0, 0)), ShellHit::Modal);
    }

    #[test]
    fn web_points_lose_the_reserved_strip() {
        assert_eq!(to_web_point(at(10, 40), chrome()), at(10, 8));
        assert_eq!(
            to_web_point(
                at(10, 40),
                ShellState {
                    reserved_strip: 0,
                    ..chrome()
                }
            ),
            at(10, 40)
        );
    }

    #[test]
    fn a_modal_takes_the_window_focus_from_the_page() {
        assert!(web_focus(true, false));
        assert!(!web_focus(true, true));
        assert!(!web_focus(false, false));
    }

    #[test]
    fn printable_characters_are_text() {
        assert!(is_text('a', 0, false));
        assert!(is_text('a', EVENTFLAG_SHIFT_DOWN, false));
    }

    #[test]
    fn control_codepoints_are_not_text() {
        assert!(!is_text('\r', 0, false));
        assert!(!is_text('\u{8}', 0, false));
        assert!(!is_text('\u{7f}', 0, false));
    }

    #[test]
    fn shortcut_modifiers_are_not_text() {
        assert!(!is_text('a', EVENTFLAG_CONTROL_DOWN, false));
        assert!(!is_text('a', EVENTFLAG_COMMAND_DOWN, false));
    }

    #[test]
    fn altgr_is_text() {
        assert!(is_text(
            '€',
            EVENTFLAG_CONTROL_DOWN | EVENTFLAG_ALT_DOWN,
            false
        ));
        assert!(is_text('∞', EVENTFLAG_ALT_DOWN, false));
    }

    #[test]
    fn system_chars_are_not_text() {
        assert!(!is_text('f', EVENTFLAG_ALT_DOWN, true));
    }
}
