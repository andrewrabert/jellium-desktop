//! Windows menus — native Win32 popups tracked on the input thread.
//!
//! `TrackPopupMenuEx` blocks its caller for the whole life of the menu, so a
//! request is parked here and handed to the input thread through a posted
//! message; the CEF UI thread that opened it returns immediately.

use std::ffi::{OsStr, c_int};
use std::os::windows::ffi::OsStrExt;
use std::sync::atomic::{AtomicBool, Ordering};

use jfn_platform_abi::{
    MENU_DISMISSED, MenuHost, MenuItem, MenuRequest, MenuSelection, menu_has_selectable,
};
use parking_lot::Mutex;
use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, EndMenu, MENU_ITEM_FLAGS, MF_GRAYED, MF_SEPARATOR,
    MF_STRING, PostMessageW, SetForegroundWindow, TPM_LEFTALIGN, TPM_NONOTIFY, TPM_RETURNCMD,
    TPM_RIGHTBUTTON, TPM_TOPALIGN, TrackPopupMenuEx, WM_APP, WM_NULL,
};
use windows::core::PCWSTR;

use crate::input::input_hwnd;
use crate::platform::{jfn_win_get_hwnd, win_get_scale};

/// Asks the input thread to track the parked request.
pub(crate) const WM_JFN_MENU_TRACK: u32 = WM_APP + 0x100;

/// Asks the input thread to end the menu it is tracking.
pub(crate) const WM_JFN_MENU_END: u32 = WM_APP + 0x101;

struct Pending {
    items: Vec<MenuItem>,
    /// Anchor in logical (view) coordinates.
    x: c_int,
    y: c_int,
    on_selected: MenuSelection,
}

// The request awaiting its turn on the input thread.
static PENDING: Mutex<Option<Pending>> = Mutex::new(None);

// Set while `TrackPopupMenuEx` sits on the input thread's stack.
static TRACKING: AtomicBool = AtomicBool::new(false);

// Set by `hide`; makes the tracked menu resolve with MENU_DISMISSED. Cleared
// when tracking begins.
static CANCELLED: AtomicBool = AtomicBool::new(false);

pub(crate) struct WinMenuHost;

impl MenuHost for WinMenuHost {
    fn open(&self, req: MenuRequest) {
        let (Some(hwnd), true) = (input_hwnd(), menu_has_selectable(&req.items)) else {
            req.on_selected.resolve(MENU_DISMISSED);
            return;
        };
        let displaced = PENDING.lock().replace(Pending {
            items: req.items,
            x: req.x,
            y: req.y,
            on_selected: req.on_selected,
        });
        if let Some(prev) = displaced {
            prev.on_selected.resolve(MENU_DISMISSED);
        }
        let _ = unsafe { PostMessageW(Some(hwnd), WM_JFN_MENU_TRACK, WPARAM(0), LPARAM(0)) };
    }

    fn hide(&self) {
        CANCELLED.store(true, Ordering::Release);
        let pending = PENDING.lock().take();
        if let Some(pending) = pending {
            pending.on_selected.resolve(MENU_DISMISSED);
        }
        post_end();
    }

    fn shutdown(&self) {
        let pending = PENDING.lock().take();
        if let Some(pending) = pending {
            pending.on_selected.resolve(MENU_DISMISSED);
        }
        post_end();
    }
}

fn post_end() {
    if let Some(hwnd) = input_hwnd() {
        let _ = unsafe { PostMessageW(Some(hwnd), WM_JFN_MENU_END, WPARAM(0), LPARAM(0)) };
    }
}

fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Runs on the input thread, from `input_wndproc`.
pub(crate) fn on_input_message(hwnd: HWND, msg: u32) {
    if msg == WM_JFN_MENU_END {
        let _ = unsafe { EndMenu() };
        return;
    }
    if msg != WM_JFN_MENU_TRACK {
        return;
    }
    // Re-entered from inside the running menu's own message loop: end that
    // menu and let the newer request land once its track call unwinds.
    if TRACKING.load(Ordering::Acquire) {
        let _ = unsafe { EndMenu() };
        let _ = unsafe { PostMessageW(Some(hwnd), WM_JFN_MENU_TRACK, WPARAM(0), LPARAM(0)) };
        return;
    }
    let Some(pending) = PENDING.lock().take() else {
        return;
    };
    let Ok(menu) = (unsafe { CreatePopupMenu() }) else {
        pending.on_selected.resolve(MENU_DISMISSED);
        return;
    };

    for item in &pending.items {
        if item.separator {
            let _ = unsafe { AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null()) };
            continue;
        }
        // TPM_RETURNCMD reports 0 for dismissal, so an id of 0 or less could
        // never be told apart from "nothing was chosen".
        if item.id <= 0 {
            continue;
        }
        let flags = if item.enabled {
            MF_STRING
        } else {
            MENU_ITEM_FLAGS(MF_STRING.0 | MF_GRAYED.0)
        };
        let label = wide(&item.label);
        let _ = unsafe {
            AppendMenuW(
                menu,
                flags,
                item.id as usize,
                PCWSTR::from_raw(label.as_ptr()),
            )
        };
    }

    let scale = win_get_scale();
    let mut pt = POINT {
        x: (pending.x as f32 * scale).round() as i32,
        y: (pending.y as f32 * scale).round() as i32,
    };
    unsafe {
        let _ = ClientToScreen(hwnd, &mut pt);
    }

    let toplevel = jfn_win_get_hwnd();
    if !toplevel.is_null() {
        let _ = unsafe { SetForegroundWindow(HWND(toplevel)) };
    }

    CANCELLED.store(false, Ordering::Release);
    TRACKING.store(true, Ordering::Release);
    let flags =
        TPM_RETURNCMD.0 | TPM_NONOTIFY.0 | TPM_LEFTALIGN.0 | TPM_TOPALIGN.0 | TPM_RIGHTBUTTON.0;
    let picked = unsafe { TrackPopupMenuEx(menu, flags, pt.x, pt.y, hwnd, None) };
    TRACKING.store(false, Ordering::Release);
    // Per MSDN: without this the first click outside the menu is swallowed.
    let _ = unsafe { PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0)) };
    let _ = unsafe { DestroyMenu(menu) };

    if CANCELLED.swap(false, Ordering::AcqRel) {
        pending.on_selected.resolve(MENU_DISMISSED);
        return;
    }
    let picked = picked.0;
    pending
        .on_selected
        .resolve(if picked == 0 { MENU_DISMISSED } else { picked });
}
