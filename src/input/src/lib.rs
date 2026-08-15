//! Input dispatch. Translates platform key/pointer events into events for
//! whichever consumer [`route`] names — jellyfin-web's CEF layer or the shell
//! overlay.

use jfn_platform_abi::LogicalPoint;
use jfn_platform_abi::event_flags::EVENTFLAG_PRECISION_SCROLLING_DELTA;
use jfn_playback::hotkey::jfn_hotkey_classify_keydown;
use jfn_playback::shutdown::jfn_shutdown_initiate;
use std::os::raw::c_int;

pub mod buttons;
pub mod cursor;
pub mod route;
pub mod scroll;
pub mod sink;

pub use route::{ShellHit, ShellState, Target};
pub use sink::{
    EditCommand, ShellInput, WebInput, install_shell, install_web, on_shell_state,
    publish_shell_state, shell_state,
};

use route::{is_text, route_key, route_pointer, to_web_point};
use sink::{with_shell, with_web};

const KEYEVENT_RAWKEYDOWN: c_int = 0;
const KEYEVENT_KEYUP: c_int = 2;
const KEYEVENT_CHAR: c_int = 3;
const MBT_LEFT: c_int = 0;
const MBT_MIDDLE: c_int = 1;
const MBT_RIGHT: c_int = 2;

fn cef_button(button_code: u32) -> Option<c_int> {
    match button_code {
        buttons::BTN_LEFT => Some(MBT_LEFT),
        buttons::BTN_RIGHT => Some(MBT_RIGHT),
        buttons::BTN_MIDDLE => Some(MBT_MIDDLE),
        _ => None,
    }
}

/// The published routing state, or `Target::None` when the shell overlay has
/// published none: with no state there is no window size and no reserved strip
/// to invent, so the event reaches nobody.
fn target_for_pointer(p: LogicalPoint) -> (Target, Option<ShellState>) {
    let Some(state) = sink::shell_state() else {
        cursor::set_owner(Target::None);
        return (Target::None, None);
    };
    let target = route_pointer(state, p);
    cursor::set_owner(target);
    (target, Some(state))
}

/// The routing target for a key, or `Target::None` with no published state.
fn target_for_key() -> Target {
    sink::shell_state().map_or(Target::None, route_key)
}

pub fn jfn_input_dispatch_mouse_move(x: i32, y: i32, mods: u32, leave: c_int) {
    let p = LogicalPoint { x, y };
    let (target, state) = target_for_pointer(p);
    match (target, state) {
        (Target::Shell, _) => with_shell(|s| s.send_mouse_move(p, mods, leave != 0)),
        (Target::Web, Some(state)) => {
            let w = to_web_point(p, state);
            with_web(|b| b.send_mouse_move(w.x, w.y, mods, leave != 0));
        }
        _ => {}
    }
}

/// A right press routed to [`Target::Shell`] calls
/// [`sink::ShellInput::context_menu`] for every [`ShellHit`] but
/// [`ShellHit::Miss`], so the app menu is reachable from a modal view as well
/// as from the titlebar.
pub fn jfn_input_dispatch_mouse_button(
    button_code: u32,
    pressed: c_int,
    x: i32,
    y: i32,
    mods: u32,
) {
    let Some(btn) = cef_button(button_code) else {
        return;
    };
    let p = LogicalPoint { x, y };
    let Some(state) = sink::shell_state() else {
        cursor::set_owner(Target::None);
        return;
    };
    let hit = route::hit(state, p);
    let target = match hit {
        ShellHit::Miss => Target::Web,
        _ => Target::Shell,
    };
    cursor::set_owner(target);
    if target == Target::Web {
        let w = to_web_point(p, state);
        with_web(|b| b.send_mouse_click(w.x, w.y, mods, btn, pressed == 0, 1));
        return;
    }
    if pressed != 0 && btn == MBT_RIGHT {
        with_shell(|s| s.context_menu(p));
        return;
    }
    // The window gestures are press gestures and never reach the widget tree;
    // the window controls are buttons and act on release like every other one.
    if pressed != 0 && btn == MBT_LEFT && matches!(hit, ShellHit::Drag | ShellHit::Grip(_)) {
        with_shell(|s| s.window_gesture(hit));
        return;
    }
    with_shell(|s| s.send_mouse_click(p, mods, btn, pressed == 0, 1));
}

pub fn jfn_input_dispatch_scroll(x: i32, y: i32, dx: i32, dy: i32, mods: u32) {
    dispatch_scroll(x, y, dx, dy, mods);
}

/// Variant that lets the caller flag a precision (trackpad) delta.
pub fn jfn_input_dispatch_scroll_precise(
    x: i32,
    y: i32,
    dx: i32,
    dy: i32,
    mods: u32,
    precise: c_int,
) {
    let mods = if precise != 0 {
        mods | EVENTFLAG_PRECISION_SCROLLING_DELTA
    } else {
        mods
    };
    dispatch_scroll(x, y, dx, dy, mods);
}

fn dispatch_scroll(x: i32, y: i32, dx: i32, dy: i32, mods: u32) {
    let p = LogicalPoint { x, y };
    let (target, state) = target_for_pointer(p);
    match (target, state) {
        (Target::Shell, _) => with_shell(|s| s.send_mouse_wheel(p, mods, dx, dy)),
        (Target::Web, Some(state)) => {
            let w = to_web_point(p, state);
            with_web(|b| b.send_mouse_wheel(w.x, w.y, mods, dx, dy));
        }
        _ => {}
    }
}

/// Routed like a key: dropped while a modal owns input.
pub fn jfn_input_dispatch_history_nav(forward: c_int) {
    if target_for_key() == Target::Web {
        with_web(|b| b.navigate_history(forward != 0));
    }
}

pub fn jfn_input_dispatch_keyboard_focus(gained: c_int) {
    match target_for_key() {
        Target::Shell => with_shell(|s| s.set_focus(gained != 0)),
        Target::Web => with_web(|b| b.set_focus(gained != 0)),
        Target::None => {}
    }
}

/// Char event with explicit is_system_key (for WM_SYSCHAR on Windows). The
/// 3-arg `jfn_input_dispatch_char` below is the wayland/x11 path which never
/// generates system chars. jellyfin-web takes CEF's char event; the shell
/// overlay takes text, and only what [`route::is_text`] admits.
pub fn jfn_input_dispatch_char_sys(
    codepoint: u32,
    mods: u32,
    native_code: u32,
    is_system_key: c_int,
) {
    if codepoint == 0 || codepoint >= 0x10_FFFF {
        return;
    }
    match target_for_key() {
        Target::Shell => shell_text(codepoint, mods, is_system_key != 0),
        Target::Web => {
            let cp16 = codepoint as u16;
            with_web(|b| {
                b.send_key_event(
                    KEYEVENT_CHAR,
                    mods,
                    codepoint as c_int,
                    native_code as c_int,
                    is_system_key != 0,
                    cp16,
                    cp16,
                );
            });
        }
        Target::None => {}
    }
}

/// The shell overlay's focused widget inserts whole characters; a lone
/// surrogate is half of one, and only CEF's UTF-16 char event carries it.
fn shell_text(codepoint: u32, mods: u32, is_system_key: bool) {
    let Some(ch) = char::from_u32(codepoint).filter(|c| is_text(*c, mods, is_system_key)) else {
        return;
    };
    let mut utf8 = [0u8; 4];
    with_shell(|s| s.send_text(ch.encode_utf8(&mut utf8)));
}

pub fn jfn_input_dispatch_char(codepoint: u32, mods: u32, native_code: u32) {
    jfn_input_dispatch_char_sys(codepoint, mods, native_code, 0);
}

/// Composed text from an xkb compose sequence or a dead key; routed as text,
/// never as a synthetic key.
pub fn jfn_input_dispatch_text(text: &str, mods: u32) {
    match target_for_key() {
        Target::Shell => with_shell(|s| s.send_text(text)),
        Target::Web => {
            for ch in text.chars() {
                jfn_input_dispatch_char(ch as u32, mods, 0);
            }
        }
        Target::None => {}
    }
}

/// Flat key dispatch used by macOS and Windows input shims. Linux paths use
/// `jfn_linux_util::input::jfn_input_dispatch_key_raw`, which routes through
/// the xkb keysym → VK mapping first.
pub fn jfn_input_dispatch_key_full(
    pressed: c_int,
    windows_key_code: i32,
    native_key_code: i32,
    modifiers: u32,
    character: u16,
    unmodified_character: u16,
    is_system_key: c_int,
) {
    if pressed != 0 {
        match jfn_hotkey_classify_keydown(windows_key_code, modifiers) {
            1 => {
                jfn_shutdown_initiate();
                return;
            }
            2 => {
                if let Some(p) = jfn_platform_abi::try_get() {
                    p.toggle_fullscreen();
                }
                return;
            }
            _ => {}
        }
    }
    key_event(
        pressed != 0,
        modifiers,
        windows_key_code,
        native_key_code,
        is_system_key != 0,
        character,
        unmodified_character,
    );
}

fn key_event(
    pressed: bool,
    modifiers: u32,
    windows_key_code: c_int,
    native_key_code: c_int,
    is_system_key: bool,
    character: u16,
    unmodified_character: u16,
) {
    match target_for_key() {
        Target::Shell => {
            with_shell(|s| s.send_key(pressed, modifiers, windows_key_code, character));
        }
        Target::Web => with_web(|b| {
            b.send_key_event(
                if pressed {
                    KEYEVENT_RAWKEYDOWN
                } else {
                    KEYEVENT_KEYUP
                },
                modifiers,
                windows_key_code,
                native_key_code,
                is_system_key,
                character,
                unmodified_character,
            );
        }),
        Target::None => {}
    }
}

/// Each routed by [`route_key`]: to the shell overlay's focused widget while a
/// modal owns input, to jellyfin-web otherwise.
fn edit(command: EditCommand) {
    match target_for_key() {
        Target::Shell => with_shell(|s| s.edit(command)),
        Target::Web => with_web(|b| match command {
            EditCommand::Undo => b.undo(),
            EditCommand::Redo => b.redo(),
            EditCommand::Cut => b.cut(),
            EditCommand::Copy => b.copy(),
            EditCommand::Paste => b.paste(),
            EditCommand::SelectAll => b.select_all(),
        }),
        Target::None => {}
    }
}

pub fn jfn_input_undo() {
    edit(EditCommand::Undo);
}

pub fn jfn_input_redo() {
    edit(EditCommand::Redo);
}

pub fn jfn_input_cut() {
    edit(EditCommand::Cut);
}

pub fn jfn_input_copy() {
    edit(EditCommand::Copy);
}

pub fn jfn_input_paste() {
    edit(EditCommand::Paste);
}

pub fn jfn_input_select_all() {
    edit(EditCommand::SelectAll);
}
