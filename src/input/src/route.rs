//! Where an input event goes.
//!
//! A pure function of the shell overlay's state and the pointer position:
//! no display server, no GPU, no CEF process.

use jfn_platform_abi::{LogicalSize, WebPoint, WindowPoint};
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
    pub titlebar_interactive: bool,
    /// Width of the minimize/maximize/close strip at the titlebar's right edge.
    pub controls_w: c_int,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Routing {
    pub shell: ShellState,
    pub window: LogicalSize,
    pub web_top: c_int,
}

impl Routing {
    fn titlebar(self) -> bool {
        self.web_top > 0 && self.shell.titlebar_interactive
    }
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
pub fn hit(routing: Routing, p: WindowPoint) -> ShellHit {
    if routing.shell.modal_open {
        return ShellHit::Modal;
    }
    if !routing.titlebar() {
        return ShellHit::Miss;
    }
    if let Some(edge) = resize_edge(routing, p) {
        return ShellHit::Grip(edge);
    }
    let (x, y) = p.floor();
    if y < 0 || y >= routing.web_top {
        return ShellHit::Miss;
    }
    if routing.shell.controls_w > 0 && x >= routing.window.w - routing.shell.controls_w {
        return ShellHit::Controls;
    }
    ShellHit::Drag
}

pub fn route_pointer(routing: Routing, p: WindowPoint) -> Target {
    match hit(routing, p) {
        ShellHit::Miss => Target::Web,
        _ => Target::Shell,
    }
}

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
pub fn resize_edge(routing: Routing, p: WindowPoint) -> Option<c_int> {
    if routing.shell.modal_open || !routing.titlebar() {
        return None;
    }
    let LogicalSize { w, h } = routing.window;
    let (x, y) = p.floor();
    if w <= 0 || h <= 0 || x < 0 || y < 0 || x >= w || y >= h {
        return None;
    }
    let mut mask = 0;
    if y < EDGE_LOGICAL {
        mask |= EDGE_TOP;
    }
    if y >= h - EDGE_LOGICAL {
        mask |= EDGE_BOTTOM;
    }
    if x < EDGE_LOGICAL {
        mask |= EDGE_LEFT;
    }
    if x >= w - EDGE_LOGICAL {
        mask |= EDGE_RIGHT;
    }
    if mask == EDGE_TOP || mask == EDGE_BOTTOM {
        if x < CORNER_LOGICAL {
            mask |= EDGE_LEFT;
        } else if x >= w - CORNER_LOGICAL {
            mask |= EDGE_RIGHT;
        }
    } else if mask == EDGE_LEFT || mask == EDGE_RIGHT {
        if y < CORNER_LOGICAL {
            mask |= EDGE_TOP;
        } else if y >= h - CORNER_LOGICAL {
            mask |= EDGE_BOTTOM;
        }
    }
    if mask == 0 { None } else { Some(mask) }
}

pub fn to_web_point(p: WindowPoint, web_top: c_int) -> WebPoint {
    WebPoint::from_view(p.x(), p.y() - f64::from(web_top))
}

pub fn from_web_point(p: WebPoint, web_top: c_int) -> WindowPoint {
    WindowPoint::from_window(p.x(), p.y() + f64::from(web_top))
}

#[cfg(test)]
mod tests {
    use super::*;
    use jfn_platform_abi::event_flags::{
        EVENTFLAG_ALT_DOWN, EVENTFLAG_COMMAND_DOWN, EVENTFLAG_CONTROL_DOWN, EVENTFLAG_SHIFT_DOWN,
    };

    fn chrome() -> Routing {
        Routing {
            shell: ShellState {
                modal_open: false,
                titlebar_interactive: true,
                controls_w: 138,
            },
            window: LogicalSize { w: 800, h: 600 },
            web_top: 32,
        }
    }

    fn modal() -> Routing {
        Routing {
            shell: ShellState {
                modal_open: true,
                ..chrome().shell
            },
            ..chrome()
        }
    }

    fn at(x: c_int, y: c_int) -> WindowPoint {
        WindowPoint::from_window(f64::from(x), f64::from(y))
    }

    #[test]
    fn modal_takes_everything() {
        let s = modal();
        assert_eq!(route_pointer(s, at(400, 400)), Target::Shell);
        assert_eq!(route_key(s.shell), Target::Shell);
        assert_eq!(resize_edge(s, at(0, 0)), None);
    }

    #[test]
    fn bare_window_gives_everything_to_web() {
        let s = Routing {
            shell: ShellState::default(),
            window: LogicalSize { w: 800, h: 600 },
            web_top: 0,
        };
        assert_eq!(route_pointer(s, at(0, 0)), Target::Web);
        assert_eq!(route_key(s.shell), Target::Web);
    }

    #[test]
    fn titlebar_strip_goes_to_shell_and_the_page_below_does_not() {
        assert_eq!(route_pointer(chrome(), at(400, 10)), Target::Shell);
        assert_eq!(route_pointer(chrome(), at(400, 31)), Target::Shell);
        assert_eq!(route_pointer(chrome(), at(400, 32)), Target::Web);
    }

    #[test]
    fn keys_always_reach_the_page_without_a_modal() {
        assert_eq!(route_key(chrome().shell), Target::Web);
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
        for s in [
            Routing {
                web_top: 0,
                ..chrome()
            },
            Routing {
                shell: ShellState {
                    titlebar_interactive: false,
                    ..chrome().shell
                },
                ..chrome()
            },
        ] {
            assert_eq!(resize_edge(s, at(0, 0)), None);
            assert_eq!(route_pointer(s, at(0, 0)), Target::Web);
        }
    }

    #[test]
    fn a_fractional_point_lies_in_the_pixel_it_is_inside() {
        let p = WindowPoint::from_window(400.0, 31.9);
        assert_eq!(route_pointer(chrome(), p), Target::Shell);
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
        let s = modal();
        assert_eq!(hit(s, at(400, 400)), ShellHit::Modal);
        assert_eq!(hit(s, at(0, 0)), ShellHit::Modal);
    }

    #[test]
    fn web_points_lose_the_web_offset() {
        assert_eq!(to_web_point(at(10, 40), 32), WebPoint::from_view(10.0, 8.0));
        assert_eq!(to_web_point(at(10, 40), 0), WebPoint::from_view(10.0, 40.0));
    }

    #[test]
    fn a_web_point_returns_to_the_window_point_it_came_from() {
        for web_top in [32, 0] {
            assert_eq!(
                from_web_point(to_web_point(at(10, 40), web_top), web_top),
                at(10, 40)
            );
        }
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
