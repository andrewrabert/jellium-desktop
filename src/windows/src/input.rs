//! Windows input — Win32 child window owning all keyboard/mouse for CEF.
//!
//! Runs on a dedicated thread (spawned by `platform.rs::win_init`);
//! registers a `JellyfinCefInput` window class, creates a child of mpv's
//! HWND covering the client area, and translates `WM_*` messages into
//! the platform-agnostic `jfn_input_dispatch_*` entry points exposed by
//! `src/input/src/lib.rs`.

#![allow(non_snake_case)]

use parking_lot::Mutex;
use std::cell::RefCell;
use std::ffi::c_int;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemServices::{
    APPCOMMAND_BROWSER_BACKWARD, APPCOMMAND_BROWSER_FORWARD, MK_CONTROL, MK_LBUTTON, MK_MBUTTON,
    MK_RBUTTON, MK_SHIFT,
};
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::HiDpi::{
    GetAwarenessFromDpiAwarenessContext, GetThreadDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, MAPVK_VK_TO_CHAR, MapVirtualKeyW, SetFocus, VK_ADD, VK_BROWSER_BACK,
    VK_BROWSER_FORWARD, VK_CAPITAL, VK_CLEAR, VK_CONTROL, VK_DECIMAL, VK_DELETE, VK_DIVIDE,
    VK_DOWN, VK_END, VK_F4, VK_HOME, VK_INSERT, VK_LCONTROL, VK_LEFT, VK_LMENU, VK_LSHIFT, VK_LWIN,
    VK_MENU, VK_MULTIPLY, VK_NEXT, VK_NUMLOCK, VK_NUMPAD0, VK_NUMPAD1, VK_NUMPAD2, VK_NUMPAD3,
    VK_NUMPAD4, VK_NUMPAD5, VK_NUMPAD6, VK_NUMPAD7, VK_NUMPAD8, VK_NUMPAD9, VK_PRIOR, VK_RCONTROL,
    VK_RETURN, VK_RIGHT, VK_RMENU, VK_RSHIFT, VK_RWIN, VK_SHIFT, VK_SUBTRACT, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GA_ROOT,
    GWLP_USERDATA, GetAncestor, GetClientRect, GetCursorPos, GetForegroundWindow, GetMessageW,
    GetWindowLongPtrW, GetWindowThreadProcessId, HCURSOR, HICON, HMENU, HTCLIENT, IDC_APPSTARTING,
    IDC_ARROW, IDC_CROSS, IDC_HAND, IDC_HELP, IDC_IBEAM, IDC_NO, IDC_SIZEALL, IDC_SIZENESW,
    IDC_SIZENS, IDC_SIZENWSE, IDC_SIZEWE, IDC_WAIT, KF_EXTENDED, LoadCursorW, MSG, PostMessageW,
    PostThreadMessageW, RegisterClassExW, SET_WINDOW_POS_FLAGS, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOZORDER, SetCursor, SetWindowLongPtrW, SetWindowPos, TranslateMessage, UnregisterClassW,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_APPCOMMAND, WM_CHAR, WM_KEYDOWN, WM_KEYUP, WM_KILLFOCUS,
    WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDBLCLK, WM_MBUTTONDOWN, WM_MBUTTONUP,
    WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_NCCREATE, WM_NCDESTROY, WM_QUIT,
    WM_RBUTTONDBLCLK, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SETCURSOR, WM_SETFOCUS, WM_SYSCHAR,
    WM_SYSKEYDOWN, WM_SYSKEYUP, WM_XBUTTONDOWN, WM_XBUTTONUP, WNDCLASSEXW, WS_CHILD, WS_VISIBLE,
    XBUTTON2,
};
use windows::core::{PCWSTR, w};

const WM_JFN_RECREATE_WINDOW: u32 = windows::Win32::UI::WindowsAndMessaging::WM_USER + 2;

const WM_MOUSELEAVE: u32 = 0x02A3;

use jfn_input::buttons::{BTN_LEFT, BTN_MIDDLE, BTN_RIGHT};
use jfn_platform_abi::cursor::CursorShape;
use jfn_platform_abi::event_flags::{
    EVENTFLAG_ALT_DOWN, EVENTFLAG_CAPS_LOCK_ON, EVENTFLAG_CONTROL_DOWN, EVENTFLAG_IS_KEY_PAD,
    EVENTFLAG_IS_LEFT, EVENTFLAG_IS_RIGHT, EVENTFLAG_LEFT_MOUSE_BUTTON,
    EVENTFLAG_MIDDLE_MOUSE_BUTTON, EVENTFLAG_NUM_LOCK_ON, EVENTFLAG_RIGHT_MOUSE_BUTTON,
    EVENTFLAG_SHIFT_DOWN,
};
use jfn_platform_abi::{LogicalPoint, PhysicalPoint, WindowExtent, WindowStateError};

use jfn_input::key::{KeyReport, PhysicalKey};
use jfn_input::{
    HistoryDirection, PointerFocus, jfn_input_dispatch_history_nav, jfn_input_dispatch_key,
    jfn_input_dispatch_keyboard_focus, jfn_input_dispatch_mouse_button,
    jfn_input_dispatch_mouse_leave, jfn_input_dispatch_mouse_move, jfn_input_dispatch_scroll,
    jfn_input_dispatch_utf16,
};
use jfn_playback::shutdown::jfn_shutdown_initiate;

use crate::menu::{WM_JFN_MENU_END, WM_JFN_MENU_TRACK};

struct State {
    input_hwnd_raw: usize,
    thread_id: u32,
    cursor_type: i32,
}

static STATE: Mutex<State> = Mutex::new(State {
    input_hwnd_raw: 0,
    thread_id: 0,
    cursor_type: CursorShape::Pointer.as_raw(),
});

struct InputWindowState {
    pointer: RefCell<PointerFocus>,
}

unsafe fn input_window_state<'a>(hwnd: HWND) -> Option<&'a InputWindowState> {
    let raw = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const InputWindowState;
    unsafe { raw.as_ref() }
}

fn destroy_input_window(hwnd: HWND, state: *const InputWindowState) {
    if unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } == state as isize {
        let _ = unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, NO_USER_DATA) };
    }
    let _ = unsafe { DestroyWindow(hwnd) };
}

/// The input child window, or `None` before the input thread creates it and
/// after it tears it down.
pub(crate) fn input_hwnd() -> Option<HWND> {
    let raw = STATE.lock().input_hwnd_raw;
    (raw != 0).then_some(HWND(raw as *mut _))
}

#[inline]
fn loword_u32(v: u32) -> u16 {
    (v & 0xFFFF) as u16
}

#[inline]
fn hiword_i16(v: u32) -> i16 {
    ((v >> 16) & 0xFFFF) as i16
}

#[inline]
fn get_x_lparam(lp: LPARAM) -> i32 {
    (lp.0 as i16) as i32
}
#[inline]
fn get_y_lparam(lp: LPARAM) -> i32 {
    ((lp.0 >> 16) as i16) as i32
}

#[inline]
fn get_xbutton_wparam(wp: WPARAM) -> u16 {
    hiword_i16(wp.0 as u32) as u16
}

#[inline]
fn get_appcommand_lparam(lp: LPARAM) -> u16 {
    (hiword_i16(lp.0 as u32) as u16) & 0x7FFF
}

/// The physical key `lparam` names: the scancode, `0xE0`-prefixed when the
/// extended bit is set.
fn physical_key(lparam: LPARAM) -> PhysicalKey {
    let scancode = ((lparam.0 >> 16) as u32 & 0xff) as u16;
    let extended = ((lparam.0 >> 16) as u32 & KF_EXTENDED) != 0;
    PhysicalKey::Windows(if extended {
        0xE000 | scancode
    } else {
        scancode
    })
}

/// The character `vk` produces with no modifier applied, through
/// `MapVirtualKeyW(MAPVK_VK_TO_CHAR)` with the dead-key bit cleared.
fn logical_char(vk: u16) -> Option<char> {
    let mapped = unsafe { MapVirtualKeyW(u32::from(vk), MAPVK_VK_TO_CHAR) };
    // The high bit marks a dead key; the character it names is still the one
    // the key produces.
    jfn_input::key::logical_char(mapped & 0x7fff_ffff)
}

/// Whether the input window's top-level ancestor is the foreground window.
/// A menu's tracking loop leaves it unchanged.
fn toplevel_active(hwnd: HWND) -> bool {
    let toplevel = unsafe { GetAncestor(hwnd, GA_ROOT) };
    !toplevel.is_invalid() && toplevel == unsafe { GetForegroundWindow() }
}

#[inline]
fn is_key_down(vk: u16) -> bool {
    let s = unsafe { GetKeyState(vk as i32) };
    (s as u16 & 0x8000) != 0
}

fn mouse_modifiers(wp: WPARAM) -> u32 {
    let mut m = 0u32;
    let w = wp.0 as u32;
    if w & MK_CONTROL.0 != 0 {
        m |= EVENTFLAG_CONTROL_DOWN;
    }
    if w & MK_SHIFT.0 != 0 {
        m |= EVENTFLAG_SHIFT_DOWN;
    }
    if is_key_down(VK_MENU.0) {
        m |= EVENTFLAG_ALT_DOWN;
    }
    if w & MK_LBUTTON.0 != 0 {
        m |= EVENTFLAG_LEFT_MOUSE_BUTTON;
    }
    if w & MK_RBUTTON.0 != 0 {
        m |= EVENTFLAG_RIGHT_MOUSE_BUTTON;
    }
    if w & MK_MBUTTON.0 != 0 {
        m |= EVENTFLAG_MIDDLE_MOUSE_BUTTON;
    }
    m
}

fn keyboard_modifiers(wp: WPARAM, lp: LPARAM) -> u32 {
    let mut m = 0u32;
    if is_key_down(VK_SHIFT.0) {
        m |= EVENTFLAG_SHIFT_DOWN;
    }
    if is_key_down(VK_CONTROL.0) {
        m |= EVENTFLAG_CONTROL_DOWN;
    }
    if is_key_down(VK_MENU.0) {
        m |= EVENTFLAG_ALT_DOWN;
    }
    if (unsafe { GetKeyState(VK_NUMLOCK.0 as i32) } & 1) != 0 {
        m |= EVENTFLAG_NUM_LOCK_ON;
    }
    if (unsafe { GetKeyState(VK_CAPITAL.0 as i32) } & 1) != 0 {
        m |= EVENTFLAG_CAPS_LOCK_ON;
    }

    let extended = ((lp.0 >> 16) as u32 & KF_EXTENDED) != 0;
    let vk = wp.0 as u16;
    match vk {
        v if v == VK_RETURN.0 && extended => {
            m |= EVENTFLAG_IS_KEY_PAD;
        }
        v if !extended
            && (v == VK_INSERT.0
                || v == VK_DELETE.0
                || v == VK_HOME.0
                || v == VK_END.0
                || v == VK_PRIOR.0
                || v == VK_NEXT.0
                || v == VK_UP.0
                || v == VK_DOWN.0
                || v == VK_LEFT.0
                || v == VK_RIGHT.0) =>
        {
            m |= EVENTFLAG_IS_KEY_PAD;
        }
        v if v == VK_NUMLOCK.0
            || v == VK_NUMPAD0.0
            || v == VK_NUMPAD1.0
            || v == VK_NUMPAD2.0
            || v == VK_NUMPAD3.0
            || v == VK_NUMPAD4.0
            || v == VK_NUMPAD5.0
            || v == VK_NUMPAD6.0
            || v == VK_NUMPAD7.0
            || v == VK_NUMPAD8.0
            || v == VK_NUMPAD9.0
            || v == VK_DIVIDE.0
            || v == VK_MULTIPLY.0
            || v == VK_SUBTRACT.0
            || v == VK_ADD.0
            || v == VK_DECIMAL.0
            || v == VK_CLEAR.0 =>
        {
            m |= EVENTFLAG_IS_KEY_PAD;
        }
        v if v == VK_SHIFT.0 => {
            if is_key_down(VK_LSHIFT.0) {
                m |= EVENTFLAG_IS_LEFT;
            } else if is_key_down(VK_RSHIFT.0) {
                m |= EVENTFLAG_IS_RIGHT;
            }
        }
        v if v == VK_CONTROL.0 => {
            if is_key_down(VK_LCONTROL.0) {
                m |= EVENTFLAG_IS_LEFT;
            } else if is_key_down(VK_RCONTROL.0) {
                m |= EVENTFLAG_IS_RIGHT;
            }
        }
        v if v == VK_MENU.0 => {
            if is_key_down(VK_LMENU.0) {
                m |= EVENTFLAG_IS_LEFT;
            } else if is_key_down(VK_RMENU.0) {
                m |= EVENTFLAG_IS_RIGHT;
            }
        }
        v if v == VK_LWIN.0 => m |= EVENTFLAG_IS_LEFT,
        v if v == VK_RWIN.0 => m |= EVENTFLAG_IS_RIGHT,
        _ => {}
    }
    m
}

fn cef_cursor_to_win(shape: CursorShape) -> PCWSTR {
    use CursorShape::*;
    match shape {
        Cross => IDC_CROSS,
        Hand | Grab | Grabbing => IDC_HAND,
        IBeam => IDC_IBEAM,
        Wait => IDC_WAIT,
        Help => IDC_HELP,
        EastResize | WestResize | EastWestResize | ColumnResize => IDC_SIZEWE,
        NorthResize | SouthResize | NorthSouthResize | RowResize => IDC_SIZENS,
        NorthEastResize | SouthWestResize | NorthEastSouthWestResize => IDC_SIZENESW,
        NorthWestResize | SouthEastResize | NorthWestSouthEastResize => IDC_SIZENWSE,
        Move | MiddlePanning | MiddlePanningVertical | MiddlePanningHorizontal => IDC_SIZEALL,
        Progress => IDC_APPSTARTING,
        NoDrop | NotAllowed => IDC_NO,
        _ => IDC_ARROW,
    }
}

fn msg_to_button_code(msg: u32) -> u32 {
    match msg {
        WM_LBUTTONDOWN | WM_LBUTTONUP | WM_LBUTTONDBLCLK => BTN_LEFT,
        WM_RBUTTONDOWN | WM_RBUTTONUP | WM_RBUTTONDBLCLK => BTN_RIGHT,
        WM_MBUTTONDOWN | WM_MBUTTONUP | WM_MBUTTONDBLCLK => BTN_MIDDLE,
        _ => BTN_LEFT,
    }
}

#[inline]
fn is_button_down(msg: u32) -> bool {
    matches!(msg, WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN)
}

/// The pointer position in the space CEF's view was sized in, mapped through
/// the extent `crate::window` last sampled. The identity before the first
/// sample exists.
fn view_point(x: i32, y: i32) -> Result<LogicalPoint, WindowStateError> {
    crate::scale::view_point(crate::window::client_extent(), PhysicalPoint { x, y })
}

fn window_point(p: LogicalPoint) -> jfn_platform_abi::WindowPoint {
    jfn_platform_abi::WindowPoint::from_window(f64::from(p.x), f64::from(p.y))
}

#[derive(Debug, thiserror::Error)]
enum LeavePointError {
    #[error(transparent)]
    Win32(#[from] windows::core::Error),
    #[error(transparent)]
    Scale(#[from] WindowStateError),
}

fn leave_point(hwnd: HWND) -> Result<LogicalPoint, LeavePointError> {
    let mut pt = POINT::default();
    unsafe { GetCursorPos(&mut pt) }?;
    unsafe { ScreenToClient(hwnd, &mut pt) }.ok()?;
    Ok(view_point(pt.x, pt.y)?)
}

/// One debug line per press.
fn log_press(msg: u32, physical: PhysicalPoint, logical: LogicalPoint, extent: WindowExtent) {
    let logical_size = extent.logical();
    let physical_size = extent.physical();
    let scale = extent.scale();
    tracing::debug!(
        target: "platform",
        "press msg=0x{msg:04x} physical=({},{}) logical=({},{}) \
         extent=logical {}x{} physical {}x{} scale {}",
        physical.x, physical.y, logical.x, logical.y,
        logical_size.w, logical_size.h, physical_size.w, physical_size.h, scale,
    );
}

const PROCESSED: LRESULT = LRESULT(0);
const PROCESSED_TRUE: LRESULT = LRESULT(1);
const NO_USER_DATA: isize = 0;

unsafe extern "system" fn input_wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_SETCURSOR if u32::from(loword_u32(lp.0 as u32)) == HTCLIENT => {
            let shape =
                CursorShape::from_cef(STATE.lock().cursor_type).unwrap_or(CursorShape::Pointer);
            if shape == CursorShape::None {
                unsafe { SetCursor(None) };
                return PROCESSED_TRUE;
            }
            match unsafe { LoadCursorW(None, cef_cursor_to_win(shape)) } {
                Ok(cur) => {
                    unsafe { SetCursor(Some(cur)) };
                    return PROCESSED_TRUE;
                }
                Err(e) => tracing::error!(target: "platform", "cursor {shape:?}: {e}"),
            }
        }

        WM_NCCREATE => {
            let create = lp.0 as *const CREATESTRUCTW;
            if let Some(create) = unsafe { create.as_ref() } {
                let _ = unsafe {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize)
                };
            }
        }

        WM_NCDESTROY => {
            let _ = unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, NO_USER_DATA) };
        }

        WM_MOUSEMOVE => {
            let Some(state) = (unsafe { input_window_state(hwnd) }) else {
                return PROCESSED;
            };
            match view_point(get_x_lparam(lp), get_y_lparam(lp)) {
                Ok(p) => match state.pointer.try_borrow_mut() {
                    Ok(mut focus) => {
                        jfn_input_dispatch_mouse_move(
                            &mut focus,
                            window_point(p),
                            mouse_modifiers(wp),
                        );
                    }
                    Err(e) => tracing::error!(target: "platform", "mouse move dropped: {e}"),
                },
                Err(e) => tracing::error!(target: "platform", "mouse move dropped: {e}"),
            }
            return PROCESSED;
        }

        WM_MOUSELEAVE => {
            let Some(state) = (unsafe { input_window_state(hwnd) }) else {
                return PROCESSED;
            };
            match leave_point(hwnd) {
                Ok(p) => match state.pointer.try_borrow_mut() {
                    Ok(mut focus) => {
                        jfn_input_dispatch_mouse_leave(
                            &mut focus,
                            window_point(p),
                            mouse_modifiers(wp),
                        );
                    }
                    Err(e) => tracing::error!(target: "platform", "mouse leave dropped: {e}"),
                },
                Err(e) => tracing::error!(target: "platform", "mouse leave dropped: {e}"),
            }
            return PROCESSED;
        }

        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONUP
        | WM_MBUTTONUP => {
            let down = is_button_down(msg);
            if down {
                let _ = unsafe { SetFocus(Some(hwnd)) };
            }
            let physical = PhysicalPoint {
                x: get_x_lparam(lp),
                y: get_y_lparam(lp),
            };
            let sampled = crate::window::client_extent();
            let p = match crate::scale::view_point(sampled, physical) {
                Ok(p) => p,
                Err(e) => {
                    tracing::error!(target: "platform", "mouse button dropped: {e}");
                    return PROCESSED;
                }
            };
            if down && let Ok(extent) = sampled {
                log_press(msg, physical, p, extent);
            }
            jfn_input_dispatch_mouse_button(
                msg_to_button_code(msg),
                if down { 1 } else { 0 },
                window_point(p),
                mouse_modifiers(wp),
            );
            return PROCESSED;
        }

        WM_XBUTTONDOWN | WM_XBUTTONUP => {
            let btn = get_xbutton_wparam(wp);
            if msg == WM_XBUTTONDOWN {
                jfn_input_dispatch_history_nav(match btn {
                    XBUTTON2 => HistoryDirection::Forward,
                    _ => HistoryDirection::Back,
                });
            }
            return PROCESSED_TRUE;
        }

        WM_APPCOMMAND => {
            let cmd = get_appcommand_lparam(lp) as u32;
            if cmd == APPCOMMAND_BROWSER_BACKWARD.0 {
                jfn_input_dispatch_history_nav(HistoryDirection::Back);
                return PROCESSED_TRUE;
            }
            if cmd == APPCOMMAND_BROWSER_FORWARD.0 {
                jfn_input_dispatch_history_nav(HistoryDirection::Forward);
                return PROCESSED_TRUE;
            }
        }

        WM_MOUSEWHEEL => {
            let mut pt = POINT {
                x: get_x_lparam(lp),
                y: get_y_lparam(lp),
            };
            if let Err(e) = unsafe { ScreenToClient(hwnd, &mut pt) }.ok() {
                tracing::error!(target: "platform", "mouse wheel dropped: ScreenToClient: {e}");
                return PROCESSED;
            }
            let delta = hiword_i16(wp.0 as u32) as i32;
            match view_point(pt.x, pt.y) {
                Ok(p) => jfn_input_dispatch_scroll(window_point(p), 0, delta, mouse_modifiers(wp)),
                Err(e) => tracing::error!(target: "platform", "mouse wheel dropped: {e}"),
            }
            return PROCESSED;
        }

        WM_MOUSEHWHEEL => {
            let mut pt = POINT {
                x: get_x_lparam(lp),
                y: get_y_lparam(lp),
            };
            if let Err(e) = unsafe { ScreenToClient(hwnd, &mut pt) }.ok() {
                tracing::error!(target: "platform", "mouse wheel dropped: ScreenToClient: {e}");
                return PROCESSED;
            }
            let delta = hiword_i16(wp.0 as u32) as i32;
            match view_point(pt.x, pt.y) {
                Ok(p) => jfn_input_dispatch_scroll(window_point(p), delta, 0, mouse_modifiers(wp)),
                Err(e) => tracing::error!(target: "platform", "mouse wheel dropped: {e}"),
            }
            return PROCESSED;
        }

        WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP => {
            let vk = wp.0 as u16;
            if vk == VK_F4.0 && msg == WM_SYSKEYDOWN && is_key_down(VK_MENU.0) {
                jfn_shutdown_initiate();
                return PROCESSED;
            }
            if vk == VK_BROWSER_BACK.0 || vk == VK_BROWSER_FORWARD.0 {
                if msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN {
                    jfn_input_dispatch_history_nav(match vk {
                        v if v == VK_BROWSER_FORWARD.0 => HistoryDirection::Forward,
                        _ => HistoryDirection::Back,
                    });
                }
                return PROCESSED;
            }
            let pressed = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
            let is_sys = msg == WM_SYSKEYDOWN || msg == WM_SYSKEYUP;
            jfn_input_dispatch_key(KeyReport {
                pressed,
                modifiers: keyboard_modifiers(wp, lp),
                windows_key_code: vk as i32,
                native_key_code: lp.0 as i32,
                is_system_key: is_sys,
                character: 0,
                unmodified_character: 0,
                logical: logical_char(vk),
                physical: physical_key(lp),
            });
            return PROCESSED;
        }

        WM_CHAR | WM_SYSCHAR => {
            jfn_input_dispatch_utf16(
                wp.0 as u16,
                keyboard_modifiers(wp, lp),
                lp.0 as u32,
                msg == WM_SYSCHAR,
            );
            return PROCESSED;
        }

        WM_JFN_MENU_TRACK | WM_JFN_MENU_END => {
            if let Err(e) = crate::menu::on_input_message(hwnd, msg) {
                tracing::error!(target: "platform", "context menu: {e}");
            }
            return PROCESSED;
        }

        WM_SETFOCUS | WM_KILLFOCUS => {
            jfn_input_dispatch_keyboard_focus(c_int::from(toplevel_active(hwnd)));
            return PROCESSED;
        }

        _ => {}
    }
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

const CLASS_NAME: PCWSTR = w!("JellyfinCefInput");

pub(crate) fn jfn_input_windows_run_input_thread(mpv_hwnd: *mut std::ffi::c_void) {
    let mpv = HWND(mpv_hwnd);
    let tid = unsafe { GetCurrentThreadId() };

    {
        let mut s = STATE.lock();
        s.thread_id = tid;
    }

    let hinst = unsafe { GetModuleHandleW(None).unwrap_or_default() };

    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: Default::default(),
        lpfnWndProc: Some(input_wndproc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinst.into(),
        hIcon: HICON::default(),
        hCursor: HCURSOR::default(),
        hbrBackground: Default::default(),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: CLASS_NAME,
        hIconSm: HICON::default(),
    };
    unsafe { RegisterClassExW(&wc) };

    let window_state = Box::new(InputWindowState {
        pointer: RefCell::new(PointerFocus::new()),
    });
    let state_ptr: *const InputWindowState = std::ptr::from_ref(&*window_state);

    let mut rc = RECT::default();
    let _ = unsafe { GetClientRect(mpv, &mut rc) };

    let input_hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            CLASS_NAME,
            w!(""),
            WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0),
            0,
            0,
            rc.right - rc.left,
            rc.bottom - rc.top,
            Some(mpv),
            Some(HMENU(std::ptr::null_mut())),
            Some(hinst.into()),
            Some(state_ptr.cast::<std::ffi::c_void>()),
        )
    };

    let input_hwnd = match input_hwnd {
        Ok(h) => h,
        Err(e) => {
            tracing::error!("CreateWindowExW(JellyfinCefInput) failed: {e:?}");
            STATE.lock().thread_id = 0;
            return;
        }
    };
    STATE.lock().input_hwnd_raw = input_hwnd.0 as usize;

    {
        let mut ir = RECT::default();
        let _ = unsafe { GetClientRect(input_hwnd, &mut ir) };
        let thread_awareness =
            unsafe { GetAwarenessFromDpiAwarenessContext(GetThreadDpiAwarenessContext()) }.0;
        tracing::debug!(
            target: "platform",
            "input window: size={}x{} parent_client={}x{} thread_awareness={}",
            ir.right - ir.left, ir.bottom - ir.top,
            rc.right - rc.left, rc.bottom - rc.top,
            thread_awareness,
        );
    }

    let mpv_tid = unsafe { GetWindowThreadProcessId(mpv, None) };
    let _ = unsafe { AttachThreadInput(tid, mpv_tid, true) };
    let _ = unsafe { SetFocus(Some(input_hwnd)) };

    let mut m = MSG::default();
    while unsafe { GetMessageW(&mut m, None, 0, 0).0 } > 0 {
        if m.hwnd.is_invalid() && m.message == WM_JFN_RECREATE_WINDOW {
            let old = STATE.lock().input_hwnd_raw;
            if old != 0 {
                destroy_input_window(HWND(old as *mut std::ffi::c_void), state_ptr);
                STATE.lock().input_hwnd_raw = 0;
            }
            recreate_input_child_window(
                HWND(m.lParam.0 as *mut std::ffi::c_void),
                hinst,
                tid,
                state_ptr,
            );
            continue;
        }
        unsafe {
            let _ = TranslateMessage(&m);
            DispatchMessageW(&m);
        }
    }

    if STATE.lock().input_hwnd_raw != 0 {
        // The child may have been recreated since startup; destroy the current one.
        let raw = STATE.lock().input_hwnd_raw;
        destroy_input_window(HWND(raw as *mut std::ffi::c_void), state_ptr);
        STATE.lock().input_hwnd_raw = 0;
    }
    let _ = unsafe { UnregisterClassW(CLASS_NAME, Some(hinst.into())) };
    STATE.lock().thread_id = 0;
    drop(window_state);
}

/// Build a fresh input child window parented to `new_mpv_hwnd`, from the
/// input thread itself (`AttachThreadInput`/`SetFocus` require running
/// on the thread whose input queue attaches to the target). The old
/// child window need not be explicitly destroyed — Windows already did
/// so as a side effect of its parent (the old mpv HWND) being
/// destroyed. The window class is already registered process-wide from
/// `jfn_input_windows_run_input_thread`'s startup, so it isn't
/// re-registered here.
fn recreate_input_child_window(
    new_mpv_hwnd: HWND,
    hinst: windows::Win32::Foundation::HMODULE,
    own_tid: u32,
    state: *const InputWindowState,
) {
    let mut rc = RECT::default();
    let _ = unsafe { GetClientRect(new_mpv_hwnd, &mut rc) };

    let input_hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            CLASS_NAME,
            w!(""),
            WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0),
            0,
            0,
            rc.right - rc.left,
            rc.bottom - rc.top,
            Some(new_mpv_hwnd),
            Some(HMENU(std::ptr::null_mut())),
            Some(hinst.into()),
            Some(state.cast::<std::ffi::c_void>()),
        )
    };
    let input_hwnd = match input_hwnd {
        Ok(h) => h,
        Err(e) => {
            tracing::error!("CreateWindowExW(JellyfinCefInput) recreate failed: {e:?}");
            return;
        }
    };
    STATE.lock().input_hwnd_raw = input_hwnd.0 as usize;

    // Share input queue with the new mpv window so SetFocus across
    // windows works, same as the initial creation.
    let mpv_tid = unsafe { GetWindowThreadProcessId(new_mpv_hwnd, None) };
    let _ = unsafe { AttachThreadInput(own_tid, mpv_tid, true) };
    let _ = unsafe { SetFocus(Some(input_hwnd)) };
    tracing::info!(target: "platform", "input child window recreated against new mpv hwnd");
}

/// Post a request to recreate the input child window against
/// `new_mpv_hwnd_raw`, run on the input thread. See
/// `recreate_input_child_window` and `WM_JFN_RECREATE_WINDOW`.
pub(crate) fn jfn_input_windows_recreate(new_mpv_hwnd_raw: usize) {
    let tid = STATE.lock().thread_id;
    if tid != 0 {
        let _ = unsafe {
            PostThreadMessageW(
                tid,
                WM_JFN_RECREATE_WINDOW,
                WPARAM(0),
                LPARAM(new_mpv_hwnd_raw as isize),
            )
        };
    }
}

pub(crate) fn jfn_input_windows_stop_input_thread() {
    let tid = STATE.lock().thread_id;
    if tid != 0 {
        let _ = unsafe { PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0)) };
    }
}

pub(crate) fn jfn_input_windows_resize_to_parent(pw: c_int, ph: c_int) {
    let hwnd_raw = STATE.lock().input_hwnd_raw;
    if hwnd_raw == 0 {
        return;
    }
    let hwnd = HWND(hwnd_raw as *mut _);
    let flags: SET_WINDOW_POS_FLAGS =
        SET_WINDOW_POS_FLAGS(SWP_NOZORDER.0 | SWP_NOMOVE.0 | SWP_NOACTIVATE.0);
    let _ = unsafe { SetWindowPos(hwnd, None, 0, 0, pw, ph, flags) };
}

/// Platform::set_cursor — invoked from the CEF UI thread. Stores the
/// pending cursor type and posts a synthetic WM_SETCURSOR so the input
/// thread applies it via SetCursor (which is thread-affine).
pub(crate) fn jfn_input_windows_set_cursor(t: c_int) {
    let hwnd_raw = {
        let mut s = STATE.lock();
        s.cursor_type = t;
        s.input_hwnd_raw
    };
    if hwnd_raw == 0 {
        return;
    }
    let hwnd = HWND(hwnd_raw as *mut _);
    let _ = unsafe { PostMessageW(Some(hwnd), WM_SETCURSOR, WPARAM(hwnd_raw), LPARAM(1)) };
}
