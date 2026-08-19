//! Shared X11 state, split by ownership so every mutable resource has exactly
//! one writer:
//!
//! - [`HostServices`] / [`PaintServices`] — immutable, process-lifetime facts
//!   set once (host-window creation, then platform init) and read lock-free
//!   thereafter.
//! - [`ParentSnapshot`] — the app top-level's live geometry, published by the
//!   geometry thread through an [`ArcSwap`] so all other readers are lock-free.
//! - [`crate::registry`] — the owned surface arena and the geometry-command
//!   queue; the geometry thread is the sole structure writer.
//! - [`GATE`] — the resize transition gate (its own small lock).
//!
//! The xcb/x11rb connections still live behind `Arc`s so the input and content
//! threads can hold references independent of any lock.

use parking_lot::Mutex;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::{Arc, OnceLock};

use arc_swap::{ArcSwap, ArcSwapOption};
use jfn_compositor_core::transition::TransitionGate;
use memmap2::MmapMut;
use x11rb::protocol::shm;
use x11rb::rust_connection::RustConnection;

/// Owns one MIT-SHM segment plus its mapping. Two per surface so the renderer
/// can double-buffer.
pub struct ShmBuffer {
    seg: shm::Seg,
    map: Option<MmapMut>,
    w: i32,
    h: i32,
}

impl ShmBuffer {
    pub fn empty() -> Self {
        Self {
            seg: 0,
            map: None,
            w: 0,
            h: 0,
        }
    }

    /// The segment registered with the server, or 0 while unmapped.
    pub fn seg(&self) -> shm::Seg {
        self.seg
    }

    pub fn is_mapped(&self) -> bool {
        self.map.is_some()
    }

    pub fn dims(&self) -> (i32, i32) {
        (self.w, self.h)
    }

    /// The live mapping, or an empty slice while unmapped.
    pub fn pixels_mut(&mut self) -> &mut [u8] {
        self.map.as_mut().map_or(&mut [], |m| &mut m[..])
    }

    /// Replaces the mapping; the caller detaches the previous segment first.
    pub fn set(&mut self, seg: shm::Seg, map: MmapMut, w: i32, h: i32) {
        self.seg = seg;
        self.map = Some(map);
        self.w = w;
        self.h = h;
    }

    /// Unmaps and returns the buffer to its empty state.
    pub fn clear(&mut self) {
        *self = Self::empty();
    }
}

impl Default for ShmBuffer {
    fn default() -> Self {
        Self::empty()
    }
}

#[derive(Copy, Clone)]
pub struct Atoms {
    pub net_wm_window_type: u32,
    pub net_wm_window_type_normal: u32,
    pub net_wm_state: u32,
    pub net_wm_state_skip_taskbar: u32,
    pub net_wm_state_skip_pager: u32,
    pub net_wm_state_fullscreen: u32,
    pub net_wm_state_maximized_vert: u32,
    pub net_wm_state_maximized_horz: u32,
    pub wm_protocols: u32,
    pub wm_delete_window: u32,
    // Consumed by the Phase 3 `_NET_WM_SYNC_REQUEST` handshake.
    pub net_wm_sync_request: u32,
    pub net_wm_sync_request_counter: u32,
    pub cardinal: u32,
    pub motif_wm_hints: u32,
    pub net_active_window: u32,
    pub clipboard: u32,
    pub primary: u32,
    pub targets: u32,
    pub timestamp: u32,
    pub utf8_string: u32,
    pub text: u32,
    pub text_plain_utf8: u32,
    pub incr: u32,
    /// The property a conversion is delivered into.
    pub jfn_selection: u32,
}

/// Immutable host-window facts, set once by [`crate::lifecycle::ensure_host_window`].
pub struct HostServices {
    pub screen_num: i32,
    pub root: u32,
    /// App-owned WM-managed top-level; carries the identity/title and owns
    /// fullscreen.
    pub toplevel: u32,
    /// App-owned child of [`Self::toplevel`] filling its client area at the
    /// bottom of the stack; mpv embeds into it via `--wid`.
    pub video_host: u32,
    pub atoms: Atoms,
    /// XSync counter advertised via `_NET_WM_SYNC_REQUEST_COUNTER`, or 0 when the
    /// full handshake could not be established (then the protocol is NOT
    /// advertised and resizes degrade to same-pass chase).
    pub sync_counter: u32,
}

/// Immutable visual facts, set once by [`crate::lifecycle::init`] once the ARGB
/// visual is found. The composite tier is not here — [`crate::paint`] owns it.
pub struct PaintServices {
    pub argb_visual: u32,
    pub argb_depth: u8,
    pub colormap: u32,
}

/// The app top-level's live geometry, published by the geometry thread. An
/// immutable snapshot swapped wholesale so every other reader is lock-free and
/// never tears placement mid-update.
#[derive(Copy, Clone, Debug)]
pub struct ParentSnapshot {
    pub origin_x: i32,
    pub origin_y: i32,
    pub width: i32,
    pub height: i32,
    pub fullscreen: bool,
    pub maximized: bool,
    pub scale: jfn_platform_abi::Scale,
}

static HOST: OnceLock<HostServices> = OnceLock::new();
static PAINT: OnceLock<PaintServices> = OnceLock::new();
static PARENT: OnceLock<ArcSwapOption<ParentSnapshot>> = OnceLock::new();
/// Bottom-to-top live overlay window ids, republished by the geometry thread on
/// every create/destroy so the cursor thread can target them lock-free.
static OVERLAY_WINDOWS: OnceLock<ArcSwap<Vec<u32>>> = OnceLock::new();

/// Resize transition gate. Drops stale-size frames during a resize so the last
/// good frame holds. Small dedicated lock (the [`TransitionGate`] is a pure
/// value type).
pub static GATE: Mutex<TransitionGate> = Mutex::new(TransitionGate::new());

/// X11's resize-transition gate: it captures the published parent size when a
/// transition begins, so a stale-size frame is dropped until the window
/// settles.
pub(crate) struct X11ResizeGate;

pub(crate) static X11_RESIZE_GATE: X11ResizeGate = X11ResizeGate;

impl jfn_platform_abi::ResizeGate for X11ResizeGate {
    fn begin(&self) {
        let Some(snap) = parent_snapshot() else {
            tracing::warn!(target: "Platform", "no published geometry; nothing to gate");
            return;
        };
        GATE.lock().begin_capturing((snap.width, snap.height));
    }

    /// Ends the gate only; the geometry thread is the sole owner of overlay
    /// structure, so nothing is re-applied here.
    fn end(&self) {
        GATE.lock().end();
    }

    fn in_transition(&self) -> bool {
        GATE.lock().in_transition()
    }

    fn set_expected(&self, size: jfn_platform_abi::PhysicalSize) {
        GATE.lock().set_expected((size.w, size.h));
    }
}

static CONN: OnceLock<Arc<xcb::Connection>> = OnceLock::new();
pub static X11RB_CONN: OnceLock<Arc<RustConnection>> = OnceLock::new();

pub(crate) fn set_host_services(h: HostServices) -> bool {
    HOST.set(h).is_ok()
}

pub(crate) fn host() -> Option<&'static HostServices> {
    HOST.get()
}

pub(crate) fn set_paint_services(p: PaintServices) -> bool {
    PAINT.set(p).is_ok()
}

pub(crate) fn paint() -> Option<&'static PaintServices> {
    PAINT.get()
}

fn parent_cell() -> &'static ArcSwapOption<ParentSnapshot> {
    PARENT.get_or_init(ArcSwapOption::empty)
}

/// Publish a fresh parent snapshot. Called only by the geometry thread.
pub(crate) fn publish_parent(snap: ParentSnapshot) {
    parent_cell().store(Some(Arc::new(snap)));
}

/// Lock-free read of the latest published parent geometry. `None` until the
/// geometry thread has published one.
pub fn parent_snapshot() -> Option<Arc<ParentSnapshot>> {
    parent_cell().load_full()
}

fn overlay_windows_cell() -> &'static ArcSwap<Vec<u32>> {
    OVERLAY_WINDOWS.get_or_init(|| ArcSwap::from_pointee(Vec::new()))
}

/// Publish the live overlay window ids. Called only by the geometry thread.
pub(crate) fn publish_overlay_windows(windows: Vec<u32>) {
    overlay_windows_cell().store(Arc::new(windows));
}

/// Lock-free read of the live overlay window ids.
pub fn overlay_windows() -> Arc<Vec<u32>> {
    overlay_windows_cell().load_full()
}

pub(crate) fn open_xcb_connection() -> Result<Arc<xcb::Connection>, String> {
    let conn = xcb::Connection::connect(None)
        .map(|(conn, _)| Arc::new(conn))
        .map_err(|e| format!("{e:?}"))?;
    CONN.set(conn.clone())
        .map_err(|_| "xcb connection already initialized".to_string())?;
    Ok(conn)
}

pub(crate) fn xcb_conn() -> Option<Arc<xcb::Connection>> {
    CONN.get().cloned()
}

pub fn x11rb_conn() -> Option<Arc<RustConnection>> {
    X11RB_CONN.get().cloned()
}

pub(crate) fn raw_xcb_connection() -> Option<NonNull<c_void>> {
    let conn = CONN.get()?;
    NonNull::new(conn.get_raw_conn() as *mut c_void)
}
