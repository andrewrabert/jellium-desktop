use std::sync::OnceLock;
use std::thread::{self, JoinHandle};

use crossbeam_channel::Receiver;
use jfn_mpv::{Event, PropertyValue};
use jfn_platform_abi::{LogicalSize, Scale};

use crate::ffi::post as post_input;
use crate::ingest::{
    IngestCtx, IngestOut, IngestState, extent_at, ingest_event_for_ffi, ingest_property_for_ffi,
};

fn state() -> &'static IngestState {
    static STATE: OnceLock<IngestState> = OnceLock::new();
    STATE.get_or_init(IngestState::new)
}

pub const INGEST_FLAG_SHUTDOWN: u8 = 1;

struct PlatformCtx<'a>(&'a dyn jfn_platform_abi::Platform);

impl IngestCtx for PlatformCtx<'_> {
    fn scale(&self) -> Scale {
        self.0.scale()
    }

    fn os_logical_size(&self) -> Option<LogicalSize> {
        self.0.mpv_host().logical_content_size()
    }
}

fn dispatch(outs: Vec<IngestOut>) -> u8 {
    let mut flags = 0u8;
    for o in outs {
        match o {
            IngestOut::Input(i) => post_input(i),
            IngestOut::WindowExtentChanged => jfn_platform_abi::notify_window_changed(),
            IngestOut::Shutdown => flags |= INGEST_FLAG_SHUTDOWN,
        }
    }
    flags
}

pub fn jfn_playback_window_extent() -> Option<jfn_platform_abi::WindowExtent> {
    state().window_extent()
}

pub fn jfn_playback_window_id() -> Option<i64> {
    state().window_id()
}

pub fn jfn_playback_ingest_mpv_event_owned(
    event: &Event,
    platform: &dyn jfn_platform_abi::Platform,
) -> u8 {
    let outs = ingest_event_for_ffi(event, state(), &PlatformCtx(platform));
    dispatch(outs)
}

pub fn jfn_playback_reconcile_window_mode() {
    let Some(lease) = jfn_platform_abi::try_lease() else {
        return;
    };
    let snap = lease.platform().window_owner().source().snapshot();
    post_window_state(snap.fullscreen, snap.maximized);
}

fn post_window_state(fullscreen: bool, maximized: bool) {
    use crate::ingest::observe_id::{FULLSCREEN, WINDOW_MAX};
    let Some(lease) = jfn_platform_abi::try_lease() else {
        return;
    };
    let ctx = PlatformCtx(lease.platform());
    let outs = ingest_property_for_ffi(FULLSCREEN, &PropertyValue::Flag(fullscreen), state(), &ctx);
    dispatch(outs);
    let outs = ingest_property_for_ffi(WINDOW_MAX, &PropertyValue::Flag(maximized), state(), &ctx);
    dispatch(outs);
}

pub fn jfn_playback_fullscreen() -> bool {
    state().fullscreen()
}

pub fn jfn_playback_window_maximized() -> bool {
    state().window_maximized()
}

pub fn jfn_playback_rescale_window_extent() -> bool {
    let Some(lease) = jfn_platform_abi::try_lease() else {
        return false;
    };
    let plat = lease.platform();
    let logical = match plat.mpv_host().logical_content_size() {
        Some(logical) => logical,
        None => match state().window_extent() {
            Some(extent) => extent.logical(),
            None => return false,
        },
    };
    let Some(extent) = extent_at(plat.scale(), logical) else {
        return false;
    };
    state().set_window_extent(extent);
    jfn_platform_abi::notify_window_changed();
    true
}

pub fn jfn_playback_display_hz() -> f64 {
    state().display_hz()
}

pub fn jfn_playback_set_display_hz(hz: f64) {
    state().set_display_hz(hz);
}

pub const BACKEND_WAYLAND: u8 = 0;
pub const BACKEND_X11: u8 = 1;

pub fn jfn_playback_observe_mpv_properties(backend: u8) -> bool {
    use crate::ingest::observe_id::*;
    use jfn_mpv::sys::mpv_format;

    let Some(raw) = jfn_mpv::boot::current_raw_handle() else {
        return false;
    };

    let pairs: &[(u64, &std::ffi::CStr, mpv_format)] = &[
        (WINDOW_ID, c"window-id", mpv_format::MPV_FORMAT_INT64),
        (OSD_DIMS, c"osd-dimensions", mpv_format::MPV_FORMAT_NODE),
        (FULLSCREEN, c"fullscreen", mpv_format::MPV_FORMAT_FLAG),
        (PAUSE, c"pause", mpv_format::MPV_FORMAT_FLAG),
        (TIME_POS, c"time-pos", mpv_format::MPV_FORMAT_DOUBLE),
        (DURATION, c"duration", mpv_format::MPV_FORMAT_DOUBLE),
        (SPEED, c"speed", mpv_format::MPV_FORMAT_DOUBLE),
        (SEEKING, c"seeking", mpv_format::MPV_FORMAT_FLAG),
        (DISPLAY_FPS, c"display-fps", mpv_format::MPV_FORMAT_DOUBLE),
        (
            CACHE_STATE,
            c"demuxer-cache-state",
            mpv_format::MPV_FORMAT_NODE,
        ),
        (WINDOW_MAX, c"window-maximized", mpv_format::MPV_FORMAT_FLAG),
        (
            PAUSED_FOR_CACHE,
            c"paused-for-cache",
            mpv_format::MPV_FORMAT_FLAG,
        ),
        (CORE_IDLE, c"core-idle", mpv_format::MPV_FORMAT_FLAG),
        (
            VIDEO_FRAME_INFO,
            c"video-frame-info",
            mpv_format::MPV_FORMAT_NODE,
        ),
    ];

    for &(id, name, fmt) in pairs {
        if matches!(backend, BACKEND_WAYLAND | BACKEND_X11)
            && matches!(id, OSD_DIMS | FULLSCREEN | WINDOW_MAX | WINDOW_ID)
        {
            continue;
        }
        unsafe { jfn_mpv::sys::mpv_observe_property(raw, id, name.as_ptr(), fmt) };
    }
    true
}

pub fn jfn_playback_seed_display_hz_sync() {
    let Some(raw) = jfn_mpv::boot::current_raw_handle() else {
        return;
    };
    let mut fps: f64 = 0.0;
    let rc = unsafe {
        jfn_mpv::sys::mpv_get_property(
            raw,
            c"display-fps".as_ptr(),
            jfn_mpv::sys::mpv_format::MPV_FORMAT_DOUBLE,
            &mut fps as *mut _ as *mut std::ffi::c_void,
        )
    };
    if rc >= 0 && fps > 0.0 {
        state().set_display_hz(fps);
    }
}

type FullscreenHandler = Box<dyn Fn(bool) + Send + Sync + 'static>;
type ShutdownHandler = Box<dyn Fn() + Send + Sync + 'static>;

fn fullscreen_handler_slot() -> &'static parking_lot::Mutex<Option<FullscreenHandler>> {
    static SLOT: OnceLock<parking_lot::Mutex<Option<FullscreenHandler>>> = OnceLock::new();
    SLOT.get_or_init(|| parking_lot::Mutex::new(None))
}

fn shutdown_handler_slot() -> &'static parking_lot::Mutex<Option<ShutdownHandler>> {
    static SLOT: OnceLock<parking_lot::Mutex<Option<ShutdownHandler>>> = OnceLock::new();
    SLOT.get_or_init(|| parking_lot::Mutex::new(None))
}

struct EventThread {
    events: jfn_mpv::EventLoop,
    join: Option<JoinHandle<()>>,
}

fn event_thread_slot() -> &'static parking_lot::Mutex<Option<EventThread>> {
    static SLOT: OnceLock<parking_lot::Mutex<Option<EventThread>>> = OnceLock::new();
    SLOT.get_or_init(|| parking_lot::Mutex::new(None))
}

pub fn jfn_playback_set_fullscreen_handler<F: Fn(bool) + Send + Sync + 'static>(cb: F) {
    *fullscreen_handler_slot().lock() = Some(Box::new(cb));
}

pub fn jfn_playback_set_shutdown_handler<F: Fn() + Send + Sync + 'static>(cb: F) {
    *shutdown_handler_slot().lock() = Some(Box::new(cb));
}

fn invoke_fullscreen_handler(f: bool) {
    if let Some(cb) = fullscreen_handler_slot().lock().as_ref() {
        cb(f);
    }
}

fn invoke_shutdown_handler() {
    if let Some(cb) = shutdown_handler_slot().lock().as_ref() {
        cb();
    }
}

pub fn jfn_playback_start_mpv_event_thread() -> bool {
    let mut guard = event_thread_slot().lock();
    if guard.is_some() {
        return false;
    }
    let Some(handle) = jfn_mpv::boot::current_handle() else {
        return false;
    };
    let (events, rx) = match jfn_mpv::EventLoop::spawn(handle) {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("[playback] failed to spawn mpv event loop: {e}");
            return false;
        }
    };
    let join = match thread::Builder::new()
        .name("jfn-mpv-ingest".into())
        .spawn(move || ingest_events(rx))
    {
        Ok(join) => join,
        Err(e) => {
            eprintln!("[playback] failed to spawn jfn-mpv-ingest thread: {e}");
            return false;
        }
    };
    *guard = Some(EventThread {
        events,
        join: Some(join),
    });
    true
}

pub fn jfn_playback_stop_mpv_event_thread() {
    let entry = event_thread_slot().lock().take();
    let Some(mut t) = entry else { return };
    t.events.stop();
    if let Some(join) = t.join.take() {
        let _ = join.join();
    }
}

fn ingest_events(rx: Receiver<Event>) {
    for event in rx {
        if let Event::PropertyChange { id, ref value, .. } = event
            && id == crate::ingest::observe_id::FULLSCREEN
            && let PropertyValue::Flag(f) = value
        {
            invoke_fullscreen_handler(*f);
        }
        let Some(lease) = jfn_platform_abi::try_lease() else {
            return;
        };
        let outs = ingest_event_for_ffi(&event, state(), &PlatformCtx(lease.platform()));
        if dispatch(outs) & INGEST_FLAG_SHUTDOWN != 0 {
            invoke_shutdown_handler();
            return;
        }
    }
}
