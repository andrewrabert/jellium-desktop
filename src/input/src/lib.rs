use jfn_platform_abi::LogicalPoint;
use jfn_platform_abi::event_flags::EVENTFLAG_PRECISION_SCROLLING_DELTA;
use jfn_playback::hotkey::jfn_hotkey_classify_keydown;
use jfn_playback::shutdown::jfn_shutdown_initiate;
use std::os::raw::c_int;

pub mod buttons;
pub mod cursor;
pub mod key;
pub mod route;
pub mod scroll;
pub mod sink;
pub mod text;

pub use route::{ShellHit, ShellState, Target};
pub use sink::{
    EditCommand, FieldEdit, ShellInput, ShellStateSubscription, WebInput, field_edit,
    install_shell, install_web, on_shell_state_scoped, publish_field_edit, publish_shell_state,
    shell_state, web_became_live,
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

fn target_for_pointer(p: LogicalPoint) -> (Target, Option<ShellState>) {
    let Some(state) = sink::shell_state() else {
        cursor::set_owner(Target::None);
        return (Target::None, None);
    };
    let target = route_pointer(state, p);
    cursor::set_owner(target);
    (target, Some(state))
}

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
    if pressed != 0 && btn == MBT_MIDDLE {
        with_shell(|s| s.primary_paste(p));
        return;
    }
    if pressed != 0 && btn == MBT_LEFT && matches!(hit, ShellHit::Drag | ShellHit::Grip(_)) {
        with_shell(|s| s.window_gesture(hit));
        return;
    }
    with_shell(|s| s.send_mouse_click(p, mods, btn, pressed == 0, 1));
}

pub fn jfn_input_dispatch_scroll(x: i32, y: i32, dx: i32, dy: i32, mods: u32) {
    dispatch_scroll(x, y, dx, dy, mods);
}

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

pub fn jfn_input_dispatch_history_nav(forward: c_int) {
    if target_for_key() == Target::Web {
        with_web(|b| b.navigate_history(forward != 0));
    }
}

pub fn jfn_input_dispatch_keyboard_focus(gained: c_int) {
    with_shell(|s| s.set_focus(gained != 0));
    sink::set_window_focused(gained != 0);
}

static TYPED: parking_lot::Mutex<text::Utf16> = parking_lot::Mutex::new(text::Utf16::new());

pub fn jfn_input_dispatch_utf16(unit: u16, modifiers: u32, native_code: u32, is_system_key: bool) {
    if unit == 0 {
        return;
    }
    let paired = TYPED.lock().feed(unit);
    match target_for_key() {
        Target::Shell => {
            if let Some(ch) = paired {
                shell_text(ch, modifiers, is_system_key);
            }
        }
        Target::Web => with_web(|b| {
            b.send_key_event(
                KEYEVENT_CHAR,
                modifiers,
                c_int::from(unit),
                native_code as c_int,
                is_system_key,
                unit,
                unit,
            );
        }),
        Target::None => {}
    }
}

fn shell_text(ch: char, mods: u32, is_system_key: bool) {
    if !is_text(ch, mods, is_system_key) {
        return;
    }
    let mut utf8 = [0u8; 4];
    with_shell(|s| s.send_text(ch.encode_utf8(&mut utf8)));
}

pub fn jfn_input_dispatch_char(codepoint: u32, mods: u32, native_code: u32) {
    let Some(ch) = char::from_u32(codepoint) else {
        return;
    };
    match target_for_key() {
        Target::Shell => shell_text(ch, mods, false),
        Target::Web => {
            let mut units = [0u16; 2];
            for unit in ch.encode_utf16(&mut units) {
                let unit = *unit;
                with_web(|b| {
                    b.send_key_event(
                        KEYEVENT_CHAR,
                        mods,
                        c_int::from(unit),
                        native_code as c_int,
                        false,
                        unit,
                        unit,
                    );
                });
            }
        }
        Target::None => {}
    }
}

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

pub fn jfn_input_dispatch_key(report: key::KeyReport) {
    if report.pressed {
        match jfn_hotkey_classify_keydown(report.windows_key_code, report.modifiers) {
            1 => {
                jfn_shutdown_initiate();
                return;
            }
            2 => {
                if let Some(p) = jfn_platform_abi::try_lease() {
                    p.platform().toggle_fullscreen();
                }
                return;
            }
            _ => {}
        }
    }
    match target_for_key() {
        Target::Shell => with_shell(|s| s.send_key(report.shell_key())),
        Target::Web => with_web(|b| {
            b.send_key_event(
                if report.pressed {
                    KEYEVENT_RAWKEYDOWN
                } else {
                    KEYEVENT_KEYUP
                },
                report.modifiers,
                report.windows_key_code,
                report.native_key_code,
                report.is_system_key,
                report.character,
                report.unmodified_character,
            );
        }),
        Target::None => {}
    }
}

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
