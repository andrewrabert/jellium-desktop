//! Adapters wiring [`crate::ingest`] to the rest of the world:
//! the global [`IngestState`], entry points for the mpv event thread,
//! and the side-channel callbacks (display scale, window pixels,
//! shutdown) that don't flow through the coordinator queue.

use std::sync::OnceLock;
use std::thread::{self, JoinHandle};

use crossbeam_channel::Receiver;
use jfn_mpv::{Event, PropertyValue};

use crate::ffi::post as post_input;
use crate::ingest::{
    IngestCtx, IngestOut, IngestState, ingest_event_for_ffi, ingest_property_for_ffi,
};

// ---------------------------------------------------------------------
// Globals
// ---------------------------------------------------------------------

fn state() -> &'static IngestState {
    static STATE: OnceLock<IngestState> = OnceLock::new();
    STATE.get_or_init(IngestState::new)
}

/// Returned by [`jfn_playback_ingest_mpv_event_owned`] as a bitfield:
///   bit 0 — `MPV_EVENT_SHUTDOWN` reached; caller should break its loop.
pub const INGEST_FLAG_SHUTDOWN: u8 = 1;

struct CallerCtx {
    scale: f32,
    mac: Option<(i32, i32)>,
}

impl IngestCtx for CallerCtx {
    fn scale(&self) -> f32 {
        self.scale
    }
    fn macos_logical_size(&self) -> Option<(i32, i32)> {
        self.mac
    }
}

// ---------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------

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

// ---------------------------------------------------------------------
// FFI
// ---------------------------------------------------------------------

/// Last known window extent — coherent (logical, physical, scale) from
/// the most recent osd-dimensions digest.
pub fn jfn_playback_window_extent() -> Option<jfn_platform_abi::WindowExtent> {
    state().window_extent()
}

/// mpv's native window handle (`window-id`) as last observed. `None` before
/// mpv's VO has created its window, and on backends where mpv embeds into a
/// host window.
pub fn jfn_playback_window_id() -> Option<i64> {
    state().window_id()
}

/// Returns flag bits — see [`INGEST_FLAG_SHUTDOWN`].
pub fn jfn_playback_ingest_mpv_event_owned(
    event: &Event,
    scale: f32,
    macos_logical: Option<(i32, i32)>,
) -> u8 {
    let ctx = CallerCtx {
        scale,
        mac: macos_logical,
    };
    let outs = ingest_event_for_ffi(event, state(), &ctx);
    dispatch(outs)
}

/// Reconcile the playback window mode from the current window snapshot.
/// Idempotent — the state machine dedupes, so an unchanged mode emits
/// nothing.
pub fn jfn_playback_reconcile_window_mode() {
    let snap = jfn_platform_abi::get().window_source().snapshot();
    post_window_state(snap.fullscreen, snap.maximized);
}

/// Push the window mode through the same digest path the `fullscreen` /
/// `window-maximized` property observations drive.
///
/// `FULLSCREEN` must digest first: entering fullscreen reads the *stored*
/// maximized flag for `was_maximized`, and a fullscreen snapshot carries
/// `maximized == false` (the modes are mutually exclusive), which must not
/// clobber that flag before it is read.
fn post_window_state(fullscreen: bool, maximized: bool) {
    use crate::ingest::observe_id::{FULLSCREEN, WINDOW_MAX};
    let ctx = CallerCtx {
        scale: 1.0,
        mac: None,
    };
    let outs = ingest_property_for_ffi(FULLSCREEN, &PropertyValue::Flag(fullscreen), state(), &ctx);
    dispatch(outs);
    let outs = ingest_property_for_ffi(WINDOW_MAX, &PropertyValue::Flag(maximized), state(), &ctx);
    dispatch(outs);
}

// ---------------------------------------------------------------------
// State accessors mirroring the legacy `mpv::*` getters
// ---------------------------------------------------------------------

pub fn jfn_playback_fullscreen() -> bool {
    state().fullscreen()
}

pub fn jfn_playback_window_maximized() -> bool {
    state().window_maximized()
}

pub fn jfn_playback_display_scale() -> f64 {
    state().display_scale()
}

pub fn jfn_playback_display_hz() -> f64 {
    state().display_hz()
}

/// Seed the display-hz cache from a synchronous probe (call only from a
/// non-event context — sync mpv property reads from inside the event
/// thread deadlock).
pub fn jfn_playback_set_display_hz(hz: f64) {
    state().set_display_hz(hz);
}

// ---------------------------------------------------------------------
// Property observation + sync seed
// ---------------------------------------------------------------------

/// Display-backend discriminant.
///   0 = Wayland, 1 = X11, 2 = Other (macOS/Windows)
pub const BACKEND_WAYLAND: u8 = 0;
pub const BACKEND_X11: u8 = 1;

/// Register the property observations whose IDs are dispatched by the
/// ingest layer. Backend selection skips `osd-dimensions`, `fullscreen`,
/// and `window-maximized` on Wayland and X11 — the app owns the toplevel
/// there, so the host window feeds dims and mode through the native
/// [`jfn_platform_abi::WindowSource`] (via `notify_window_changed` →
/// `jfn_playback_reconcile_window_mode`) instead, and mpv's own properties
/// either never change (mode) or describe an embedded child, not the
/// window.
///
/// Requires `jfn_mpv_handle_init` to have succeeded; returns false if
/// the handle is missing.
pub fn jfn_playback_observe_mpv_properties(backend: u8) -> bool {
    use crate::ingest::observe_id::*;
    use jfn_mpv::sys::mpv_format;

    let Some(raw) = jfn_mpv::boot::current_raw_handle() else {
        return false;
    };

    // Order matches the legacy C++ observe_properties(): display-hidpi-scale
    // is registered before osd-dimensions so mpv's FIFO initial-value
    // delivery seeds the scale before osd-dimensions consumes it. window-id
    // precedes both so the platform's window handle resolves before the first
    // digest asks the platform for scale.
    let pairs: &[(u64, &std::ffi::CStr, mpv_format)] = &[
        (WINDOW_ID, c"window-id", mpv_format::MPV_FORMAT_INT64),
        (
            DISPLAY_SCALE,
            c"display-hidpi-scale",
            mpv_format::MPV_FORMAT_DOUBLE,
        ),
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

/// Sync mpv read for `display-fps`; seeds the `display_hz` cache from a
/// non-event context. Must not be called from inside an mpv event
/// callback — sync property reads from the event thread deadlock.
///
/// No-op if the handle isn't initialized or the property is unavailable.
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

// ---------------------------------------------------------------------
// Rust-owned mpv event thread
// ---------------------------------------------------------------------

type ScaleProvider = Box<dyn Fn() -> f32 + Send + Sync + 'static>;
type MacosLogicalProvider = Box<dyn Fn() -> Option<(i32, i32)> + Send + Sync + 'static>;
type FullscreenHandler = Box<dyn Fn(bool) + Send + Sync + 'static>;
type ShutdownHandler = Box<dyn Fn() + Send + Sync + 'static>;

fn scale_slot() -> &'static parking_lot::Mutex<Option<ScaleProvider>> {
    static SLOT: OnceLock<parking_lot::Mutex<Option<ScaleProvider>>> = OnceLock::new();
    SLOT.get_or_init(|| parking_lot::Mutex::new(None))
}

fn macos_logical_slot() -> &'static parking_lot::Mutex<Option<MacosLogicalProvider>> {
    static SLOT: OnceLock<parking_lot::Mutex<Option<MacosLogicalProvider>>> = OnceLock::new();
    SLOT.get_or_init(|| parking_lot::Mutex::new(None))
}

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

/// Install the platform fullscreen-state thunk. Invoked from the Rust
/// event thread when the `fullscreen` property changes.
pub fn jfn_playback_set_fullscreen_handler<F: Fn(bool) + Send + Sync + 'static>(cb: F) {
    *fullscreen_handler_slot().lock() = Some(Box::new(cb));
}

/// Install the per-event scale provider used when normalizing OSD
/// dimensions. Must return the device pixel scale (> 0); zero or
/// negative is substituted with 1.0.
pub fn jfn_playback_set_scale_provider<F: Fn() -> f32 + Send + Sync + 'static>(cb: F) {
    *scale_slot().lock() = Some(Box::new(cb));
}

/// Install the macOS logical-content-size override provider. Returns
/// `Some((lw, lh))` when an override applies. Non-macOS callers should
/// leave this unset.
pub fn jfn_playback_set_macos_logical_provider<
    F: Fn() -> Option<(i32, i32)> + Send + Sync + 'static,
>(
    cb: F,
) {
    *macos_logical_slot().lock() = Some(Box::new(cb));
}

/// Install the `MPV_EVENT_SHUTDOWN` handler.
pub fn jfn_playback_set_shutdown_handler<F: Fn() + Send + Sync + 'static>(cb: F) {
    *shutdown_handler_slot().lock() = Some(Box::new(cb));
}

fn snapshot_scale() -> f32 {
    let guard = scale_slot().lock();
    let s = guard.as_ref().map(|f| f()).unwrap_or(1.0);
    if s > 0.0 { s } else { 1.0 }
}

fn snapshot_macos_logical() -> Option<(i32, i32)> {
    let guard = macos_logical_slot().lock();
    guard.as_ref().and_then(|cb| cb())
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

/// Spawn the [`jfn_mpv::EventLoop`] drain thread plus the ingest
/// consumer thread that reads its receiver and routes each event through
/// the same path [`jfn_playback_ingest_mpv_event_owned`] uses. Returns
/// `false` if the handle is not yet initialized or the threads are
/// already running.
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

/// Stop the drain loop, then join the ingest thread. Idempotent.
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
        let ctx = CallerCtx {
            scale: snapshot_scale(),
            mac: snapshot_macos_logical(),
        };
        let outs = ingest_event_for_ffi(&event, state(), &ctx);
        if dispatch(outs) & INGEST_FLAG_SHUTDOWN != 0 {
            invoke_shutdown_handler();
            return;
        }
    }
}
