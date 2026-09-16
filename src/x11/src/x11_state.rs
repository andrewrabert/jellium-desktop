use parking_lot::Mutex;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::{Arc, OnceLock};

use arc_swap::{ArcSwap, ArcSwapOption};
use jfn_compositor_core::transition::TransitionGate;
use memmap2::MmapMut;
use x11rb::protocol::shm;
use x11rb::rust_connection::RustConnection;

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

    pub fn seg(&self) -> shm::Seg {
        self.seg
    }

    pub fn is_mapped(&self) -> bool {
        self.map.is_some()
    }

    pub fn dims(&self) -> (i32, i32) {
        (self.w, self.h)
    }

    pub fn pixels_mut(&mut self) -> &mut [u8] {
        self.map.as_mut().map_or(&mut [], |m| &mut m[..])
    }

    pub fn set(&mut self, seg: shm::Seg, map: MmapMut, w: i32, h: i32) {
        self.seg = seg;
        self.map = Some(map);
        self.w = w;
        self.h = h;
    }

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
    pub jfn_selection: u32,
}

pub struct HostServices {
    pub screen_num: i32,
    pub root: u32,
    pub toplevel: u32,
    pub video_host: u32,
    pub atoms: Atoms,
    pub sync_counter: u32,
}

pub struct PaintServices {
    pub argb_visual: u32,
    pub argb_depth: u8,
    pub colormap: u32,
}

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
static OVERLAY_WINDOWS: OnceLock<ArcSwap<Vec<u32>>> = OnceLock::new();

pub static GATE: Mutex<TransitionGate> = Mutex::new(TransitionGate::new());

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

pub(crate) fn publish_parent(snap: ParentSnapshot) {
    parent_cell().store(Some(Arc::new(snap)));
}

pub fn parent_snapshot() -> Option<Arc<ParentSnapshot>> {
    parent_cell().load_full()
}

fn overlay_windows_cell() -> &'static ArcSwap<Vec<u32>> {
    OVERLAY_WINDOWS.get_or_init(|| ArcSwap::from_pointee(Vec::new()))
}

pub(crate) fn publish_overlay_windows(windows: Vec<u32>) {
    overlay_windows_cell().store(Arc::new(windows));
}

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
