//! Windows menus — native Win32 popups tracked on the input thread.
//!
//! `TrackPopupMenuEx` blocks its caller for the whole life of the menu, so a
//! request is parked here and handed to the input thread through a posted
//! message; the CEF UI thread that opened it returns immediately.

use std::ffi::{OsStr, c_int};
use std::os::windows::ffi::OsStrExt;
use std::sync::atomic::{AtomicBool, Ordering};

use jfn_platform_abi::{
    MENU_DISMISSED, MenuHandle, MenuHost, MenuItem, MenuOpenError, MenuRequest, MenuSelection,
    MenuShutdownError, Unrepresentable, WindowStateError, menu_has_selectable,
};
use parking_lot::Mutex;
use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, EndMenu, HMENU, MENU_ITEM_FLAGS, MF_GRAYED,
    MF_SEPARATOR, MF_STRING, PostMessageW, SetForegroundWindow, TPM_LEFTALIGN, TPM_NONOTIFY,
    TPM_RETURNCMD, TPM_RIGHTBUTTON, TPM_TOPALIGN, TrackPopupMenuEx, WM_APP, WM_NULL,
};
use windows::core::PCWSTR;

use crate::input::input_hwnd;
use crate::platform::win_get_scale;

/// Asks the input thread to track the parked request.
pub(crate) const WM_JFN_MENU_TRACK: u32 = WM_APP + 0x100;

/// Asks the input thread to end the menu it is tracking.
pub(crate) const WM_JFN_MENU_END: u32 = WM_APP + 0x101;

const NO_COMMAND: c_int = 0;

struct Pending {
    items: Vec<MenuItem>,
    /// Anchor in logical (view) coordinates.
    x: c_int,
    y: c_int,
    on_selected: MenuSelection,
}

static PENDING: Mutex<Option<Pending>> = Mutex::new(None);

static TRACKING: AtomicBool = AtomicBool::new(false);

static CANCELLED: AtomicBool = AtomicBool::new(false);

static CURRENT: Mutex<Option<MenuHandle>> = Mutex::new(None);

static NEXT_HANDLE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn next_handle() -> MenuHandle {
    let id = NEXT_HANDLE.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
    MenuHandle::new(std::num::NonZeroU64::new(id).unwrap_or(std::num::NonZeroU64::MIN))
}

pub(crate) struct WinMenuHost;

impl MenuHost for WinMenuHost {
    fn open(&self, req: MenuRequest) -> Result<MenuHandle, MenuOpenError> {
        if !menu_has_selectable(&req.items) {
            req.on_selected.resolve(MENU_DISMISSED);
            return Err(MenuOpenError::NothingSelectable);
        }
        let Some(hwnd) = input_hwnd() else {
            req.on_selected.resolve(MENU_DISMISSED);
            return Err(MenuOpenError::NoWindow);
        };
        let handle = next_handle();
        *CURRENT.lock() = Some(handle);
        let (x, y) = req.anchor.floor();
        let displaced = PENDING.lock().replace(Pending {
            items: req.items,
            x,
            y,
            on_selected: req.on_selected,
        });
        if let Some(prev) = displaced {
            prev.on_selected.resolve(MENU_DISMISSED);
        }
        let _ = unsafe { PostMessageW(Some(hwnd), WM_JFN_MENU_TRACK, WPARAM(0), LPARAM(0)) };
        Ok(handle)
    }

    fn hide(&self, handle: MenuHandle) {
        if *CURRENT.lock() != Some(handle) {
            return;
        }
        CANCELLED.store(true, Ordering::Release);
        let pending = PENDING.lock().take();
        if let Some(pending) = pending {
            pending.on_selected.resolve(MENU_DISMISSED);
        }
        post_end();
    }

    fn shutdown(&self) -> Result<(), MenuShutdownError> {
        let pending = PENDING.lock().take();
        if let Some(pending) = pending {
            pending.on_selected.resolve(MENU_DISMISSED);
        }
        post_end();
        Ok(())
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

#[derive(Debug, thiserror::Error)]
pub(crate) enum MenuTrackError {
    #[error("menu scale: {0}")]
    Scale(#[from] WindowStateError),
    #[error("menu anchor: {0}")]
    Anchor(#[from] Unrepresentable),
    #[error("{operation}: {source}")]
    Win32 {
        operation: &'static str,
        source: windows::core::Error,
    },
    #[error("{tracked}; DestroyMenu: {destroy}")]
    Unwound {
        tracked: Box<MenuTrackError>,
        destroy: windows::core::Error,
    },
}

impl MenuTrackError {
    fn win32(operation: &'static str) -> impl FnOnce(windows::core::Error) -> Self {
        move |source| Self::Win32 { operation, source }
    }
}

pub(crate) fn on_input_message(hwnd: HWND, msg: u32) -> Result<(), MenuTrackError> {
    let tracking = TRACKING.load(Ordering::Acquire);
    if msg == WM_JFN_MENU_END {
        if tracking {
            unsafe { EndMenu() }.map_err(MenuTrackError::win32("EndMenu"))?;
        }
        return Ok(());
    }
    if msg != WM_JFN_MENU_TRACK {
        return Ok(());
    }
    if tracking {
        unsafe { EndMenu() }.map_err(MenuTrackError::win32("EndMenu"))?;
        unsafe { PostMessageW(Some(hwnd), WM_JFN_MENU_TRACK, WPARAM(0), LPARAM(0)) }
            .map_err(MenuTrackError::win32("PostMessageW(WM_JFN_MENU_TRACK)"))?;
        return Ok(());
    }
    let Some(pending) = PENDING.lock().take() else {
        return Ok(());
    };
    let menu = match unsafe { CreatePopupMenu() } {
        Ok(menu) => menu,
        Err(source) => {
            pending.on_selected.resolve(MENU_DISMISSED);
            return Err(MenuTrackError::win32("CreatePopupMenu")(source));
        }
    };

    let tracked = track(hwnd, menu, &pending);
    let destroyed = unsafe { DestroyMenu(menu) };
    let cancelled = CANCELLED.swap(false, Ordering::AcqRel);
    let picked = match tracked.as_ref() {
        Ok(Some(picked)) if !cancelled => *picked,
        _ => MENU_DISMISSED,
    };
    pending.on_selected.resolve(picked);
    match (tracked, destroyed) {
        (Ok(_), Ok(())) => Ok(()),
        (Ok(_), Err(source)) => Err(MenuTrackError::win32("DestroyMenu")(source)),
        (Err(e), Ok(())) => Err(e),
        (Err(e), Err(destroy)) => Err(MenuTrackError::Unwound {
            tracked: Box::new(e),
            destroy,
        }),
    }
}

fn track(hwnd: HWND, menu: HMENU, pending: &Pending) -> Result<Option<c_int>, MenuTrackError> {
    for item in &pending.items {
        if item.separator {
            unsafe { AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null()) }
                .map_err(MenuTrackError::win32("AppendMenuW(MF_SEPARATOR)"))?;
            continue;
        }
        if item.id <= 0 {
            continue;
        }
        let flags = if item.enabled {
            MF_STRING
        } else {
            MENU_ITEM_FLAGS(MF_STRING.0 | MF_GRAYED.0)
        };
        let label = wide(&item.label);
        unsafe {
            AppendMenuW(
                menu,
                flags,
                item.id as usize,
                PCWSTR::from_raw(label.as_ptr()),
            )
        }
        .map_err(MenuTrackError::win32("AppendMenuW(MF_STRING)"))?;
    }

    let scale = win_get_scale()?;
    let mut pt = POINT {
        x: scale.to_physical(pending.x)?,
        y: scale.to_physical(pending.y)?,
    };
    unsafe { ClientToScreen(hwnd, &mut pt) }
        .ok()
        .map_err(MenuTrackError::win32("ClientToScreen"))?;

    if let Some(toplevel) = crate::platform::win_hwnd() {
        let _ = unsafe { SetForegroundWindow(toplevel) };
    }

    CANCELLED.store(false, Ordering::Release);
    TRACKING.store(true, Ordering::Release);
    let flags =
        TPM_RETURNCMD.0 | TPM_NONOTIFY.0 | TPM_LEFTALIGN.0 | TPM_TOPALIGN.0 | TPM_RIGHTBUTTON.0;
    let picked = unsafe { TrackPopupMenuEx(menu, flags, pt.x, pt.y, hwnd, None) };
    TRACKING.store(false, Ordering::Release);
    unsafe { PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0)) }
        .map_err(MenuTrackError::win32("PostMessageW(WM_NULL)"))?;
    Ok((picked.0 != NO_COMMAND).then_some(picked.0))
}
