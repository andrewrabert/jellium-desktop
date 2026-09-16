use parking_lot::Mutex;
use std::ffi::CStr;
use std::os::raw::c_char;
use std::ptr;
use std::sync::{Arc, OnceLock};

use crate::handle::Handle;
use crate::sys;

#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DisplayBackend {
    Wayland = 0,
    X11 = 1,
    Other = 2,
}

impl DisplayBackend {
    fn from_raw(v: u8) -> Self {
        match v {
            0 => Self::Wayland,
            1 => Self::X11,
            _ => Self::Other,
        }
    }
}

#[repr(C)]
pub struct JfnMpvBoot {
    pub display_backend: u8,
    pub hwdec: *const c_char,
    pub user_agent: *const c_char,
    pub audio_passthrough: *const c_char,
    pub audio_exclusive: bool,
    pub audio_channels: *const c_char,
    pub geometry: *const c_char,
    pub wid: i64,
    pub force_window_position: bool,
    pub window_maximized_at_boot: bool,
    pub mpv_log_level: *const c_char,
    pub client_side_decorations: bool,
}

fn handle_slot() -> &'static Mutex<Option<Arc<Handle>>> {
    static SLOT: OnceLock<Mutex<Option<Arc<Handle>>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

unsafe fn cstr_opt(p: *const c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    Some(unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
}

fn set_option_or_skip(handle: &Handle, name: &str, value: &str) -> crate::error::Result<()> {
    match handle.set_option_string(name, value) {
        Ok(()) => Ok(()),
        Err(e) if e.code == sys::mpv_error::MPV_ERROR_OPTION_NOT_FOUND.0 => {
            tracing::warn!(target: "mpv", "option {} not supported by this libmpv build; skipping", name);
            Ok(())
        }
        Err(e) => {
            tracing::error!(target: "mpv", "set_option_string({}={}) failed: {:?}", name, value, e);
            Err(e)
        }
    }
}

fn set_option_flag_or_skip(handle: &Handle, name: &str, value: bool) -> crate::error::Result<()> {
    match handle.set_option_flag(name, value) {
        Ok(()) => Ok(()),
        Err(e) if e.code == sys::mpv_error::MPV_ERROR_OPTION_NOT_FOUND.0 => {
            tracing::warn!(target: "mpv", "option {} not supported by this libmpv build; skipping", name);
            Ok(())
        }
        Err(e) => {
            tracing::error!(target: "mpv", "set_option_flag({}={}) failed: {:?}", name, value, e);
            Err(e)
        }
    }
}

fn apply_defaults(
    handle: &Handle,
    display: DisplayBackend,
    client_side_decorations: bool,
) -> crate::error::Result<()> {
    let set = |name: &str, value: &str| set_option_or_skip(handle, name, value);

    set("osd-level", "0")?;
    set("osc", "no")?;
    set("display-tags", "")?;

    set("track-auto-selection", "no")?;

    set("input-default-bindings", "no")?;
    set("input-vo-keyboard", "no")?;
    set("input-cursor", "no")?;
    set("cursor-autohide", "no")?;

    if display == DisplayBackend::Other {
        set("input-vo-cursor", "no")?;
        set("input-keyboard", "no")?;
    }

    if display == DisplayBackend::Wayland {
        set("clipboard-backends", "")?;
    }

    set("stop-screensaver", "no")?;
    set("keepaspect-window", "no")?;
    set("auto-window-resize", "no")?;
    let suppress_ssd = display == DisplayBackend::Wayland && client_side_decorations;
    set("border", if suppress_ssd { "no" } else { "yes" })?;
    set("title", "Jellium Desktop")?;
    set("wayland-app-id", "net.nullsum.JelliumDesktop")?;

    set("force-window", "yes")?;
    set("idle", "yes")?;

    Ok(())
}

fn apply_boot_options(handle: &Handle, boot: &JfnMpvBoot) -> crate::error::Result<()> {
    let set = |name: &str, value: &str| set_option_or_skip(handle, name, value);
    let set_flag = |name: &str, value: bool| set_option_flag_or_skip(handle, name, value);

    set("config", "yes")?;
    set("ytdl", "no")?;

    if let Some(ua) = unsafe { cstr_opt(boot.user_agent) } {
        set("user-agent", &ua)?;
    }
    if let Some(hwdec) = unsafe { cstr_opt(boot.hwdec) } {
        set("hwdec", &hwdec)?;
    }
    if let Some(geom) = unsafe { cstr_opt(boot.geometry) } {
        set("geometry", &geom)?;
    }
    if boot.wid != 0 {
        set("wid", &boot.wid.to_string())?;
    }
    if boot.force_window_position {
        set("force-window-position", "yes")?;
    }
    if boot.window_maximized_at_boot {
        set("window-maximized", "yes")?;
    }
    if let Some(spdif) = unsafe { cstr_opt(boot.audio_passthrough) }
        && !spdif.is_empty()
    {
        set("audio-spdif", &spdif)?;
    }
    if boot.audio_exclusive {
        set_flag("audio-exclusive", true)?;
    }
    if let Some(ch) = unsafe { cstr_opt(boot.audio_channels) }
        && !ch.is_empty()
    {
        set("audio-channels", &ch)?;
    }
    Ok(())
}

pub unsafe fn jfn_mpv_handle_init(boot: *const JfnMpvBoot) -> *mut sys::mpv_handle {
    if boot.is_null() {
        return ptr::null_mut();
    }
    let boot = unsafe { &*boot };
    let display = DisplayBackend::from_raw(boot.display_backend);

    let handle = match Handle::create() {
        Ok(h) => h,
        Err(e) => {
            tracing::error!(target: "mpv", "mpv_create failed: {:?}", e);
            return ptr::null_mut();
        }
    };

    if let Err(e) = apply_defaults(&handle, display, boot.client_side_decorations) {
        tracing::error!(target: "mpv", "apply_defaults failed: {:?}", e);
        return ptr::null_mut();
    }
    if let Err(e) = apply_boot_options(&handle, boot) {
        tracing::error!(target: "mpv", "apply_boot_options failed: {:?}", e);
        return ptr::null_mut();
    }

    handle.set_wakeup_callback(|| {});

    if let Err(e) = handle.initialize() {
        tracing::error!(target: "mpv", "mpv_initialize failed: {:?}", e);
        return ptr::null_mut();
    }

    if let Some(level) = unsafe { cstr_opt(boot.mpv_log_level) }
        && !level.is_empty()
    {
        unsafe {
            use std::ffi::CString;
            if let Ok(c) = CString::new(level) {
                sys::mpv_request_log_messages(handle.raw(), c.as_ptr());
            }
        }
    }

    let raw = handle.raw();
    *handle_slot().lock() = Some(Arc::new(handle));
    raw
}

pub fn jfn_mpv_handle_terminate() {
    let taken = handle_slot().lock().take();
    let Some(handle) = taken else {
        return;
    };
    if Arc::into_inner(handle).is_none() {
        tracing::warn!(
            target: "mpv",
            "mpv handle still referenced at terminate; mpv_terminate_destroy deferred"
        );
    }
}

pub fn jfn_mpv_handle_get() -> *mut sys::mpv_handle {
    current_raw_handle().unwrap_or(ptr::null_mut())
}

pub fn current_raw_handle() -> Option<*mut sys::mpv_handle> {
    handle_slot().lock().as_ref().map(|h| h.raw())
}

pub fn current_handle() -> Option<Arc<Handle>> {
    handle_slot().lock().as_ref().map(Arc::clone)
}
