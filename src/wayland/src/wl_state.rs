//! Wayland surface / present / transition state, driven through the
//! [`crate::wl_ops`] entry points reached from the Platform adapter
//! ([`crate::make_platform`]).
//!
//! Owns:
//!   * A dedicated `EventQueue` over an mpv-owned `wl_display`
//!     (foreign-display backend, never closes the fd)
//!   * Bindings for `wl_compositor`, `wl_subcompositor`, `wl_shm`,
//!     `zwp_linux_dmabuf_v1`, `wp_viewporter`
//!   * The list of per-layer `PlatformSurface`s and their popup
//!     children
//!   * The fullscreen-transition state machine (begin/end + tolerance
//!     gate for the paint path)
//!
//! All mutable state lives behind a single `Mutex` — mirrors the C++
//! `surface_mtx` discipline. Coarse locking is intentional: the paint
//! path holds the lock during commit/flush, and finer-grained locking
//! would risk null-attach vs. commit ordering races.

use nix::sys::memfd::MFdFlags;
use parking_lot::Mutex;
use std::ffi::c_void;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use jfn_gpu_paint::Surfaces;

use crate::layer::SurfaceRef;
use crate::layer_actor::LayerActor;

use memmap2::MmapOptions;
use wayland_backend::client::Backend;
use wayland_client::backend::ObjectId;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{
    wl_buffer::WlBuffer,
    wl_compositor::WlCompositor,
    wl_region::WlRegion,
    wl_registry::WlRegistry,
    wl_shm::{Format, WlShm},
    wl_shm_pool::WlShmPool,
    wl_subcompositor::WlSubcompositor,
    wl_subsurface::WlSubsurface,
    wl_surface::WlSurface,
};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1::{Flags as DmabufFlags, ZwpLinuxBufferParamsV1},
    zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};

const fn fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
    (a as u32) | ((b as u32) << 8) | ((c as u32) << 16) | ((d as u32) << 24)
}

const DRM_FORMAT_ARGB8888: u32 = fourcc(b'A', b'R', b'2', b'4');

/// FS transition tolerance in texels — first paint within this of the
/// new mpv size ends the transition.
pub(crate) const TRANSITION_TOLERANCE_TEXELS: i32 = 32;

// =====================================================================
// Per-surface state
// =====================================================================

/// A layer's synchronized subsurface.
/// The app root `wl_surface`, exposed only as a subsurface parent.
#[derive(Clone)]
pub(crate) struct RootParent(WlSurface);

impl RootParent {
    pub(crate) fn attach_child(
        &self,
        subcompositor: &WlSubcompositor,
        surface: &WlSurface,
        qh: &QueueHandle<DispatchState>,
    ) -> SyncSubsurface {
        SyncSubsurface::create(subcompositor, surface, &self.0, qh)
    }
}

pub(crate) struct SyncSubsurface(WlSubsurface);

impl SyncSubsurface {
    pub(crate) fn create(
        subcompositor: &WlSubcompositor,
        surface: &WlSurface,
        parent: &WlSurface,
        qh: &QueueHandle<DispatchState>,
    ) -> Self {
        let sub = subcompositor.get_subsurface(surface, parent, qh, ());
        Self(sub)
    }

    pub(crate) fn set_position(&self, x: i32, y: i32) {
        self.0.set_position(x, y);
    }

    pub(crate) fn place_above(&self, sibling: &WlSurface) {
        self.0.place_above(sibling);
    }

    pub(crate) fn destroy(self) {
        self.0.destroy();
    }
}

pub(crate) struct OwnedBuffer {
    buf: WlBuffer,
    registry: &'static BufferRegistry,
}

impl OwnedBuffer {
    fn id(&self) -> ObjectId {
        self.buf.id()
    }

    /// Marks the buffer in-use until its next release.
    pub(crate) fn attach_to(&self, surface: &WlSurface, x: i32, y: i32) {
        self.registry.mark_attached(&self.id());
        surface.attach(Some(&self.buf), x, y);
    }
}

impl Drop for OwnedBuffer {
    fn drop(&mut self) {
        self.registry.retire(self.buf.clone());
    }
}

pub(crate) struct DmabufBuf {
    pub id: (u64, u64),
    pub w: i32,
    pub h: i32,
    pub stride: u32,
    pub modifier: u64,
    pub buf: OwnedBuffer,
}

pub(crate) struct PlatformSurface {
    pub surface: Option<SurfaceRef>,
    pub subsurface: Option<SyncSubsurface>,
    pub visible: bool,
    pub null_attached: bool,

    pub popup_surface: Option<WlSurface>,
    pub popup_subsurface: Option<SyncSubsurface>,
    pub popup_viewport: Option<WpViewport>,
    pub popup_buffer: Option<OwnedBuffer>,
    pub popup_visible: bool,

    pub layer_actor: Option<LayerActor>,
}

impl PlatformSurface {
    pub(crate) fn new() -> Self {
        Self {
            surface: None,
            subsurface: None,
            visible: true,
            null_attached: false,
            popup_surface: None,
            popup_subsurface: None,
            popup_viewport: None,
            popup_buffer: None,
            popup_visible: false,
            layer_actor: None,
        }
    }
}

// =====================================================================
// Wl-side state (one global, mutex-guarded — mirrors C++ surface_mtx)
// =====================================================================

pub(crate) struct WlState {
    pub conn: Connection,
    pub qh: QueueHandle<DispatchState>,
    /// Dedicated event queue — kept alive so all our proxies route here
    /// instead of mpv's default queue.
    #[allow(dead_code)]
    pub queue: EventQueue<DispatchState>,

    pub compositor: WlCompositor,
    pub subcompositor: WlSubcompositor,
    pub shm: WlShm,
    pub dmabuf: Option<ZwpLinuxDmabufV1>,
    pub viewporter: Option<WpViewporter>,

    pub root_surface: Option<RootParent>,

    /// Stack order, bottom-to-top. Raw pointers are valid for the
    /// lifetime of each `PlatformSurface` (heap-allocated via `Box`,
    /// removed before drop).
    pub stack: Vec<*mut PlatformSurface>,

    pub was_fullscreen: bool,

    pub gpu: Option<&'static Surfaces>,
    /// When true, `surface_present_software` routes through each
    /// surface's GPU paint worker (Vulkan WSI) instead of `wl_shm`.
    /// `set_visible` and `resize` also skip their
    /// `wl_surface.attach`/`viewport.set_destination` work for the
    /// gpu_paint surface.
    pub use_gpu_paint: bool,

    pub scene: crate::scene::Scene,
    pub menu_io: crate::popup::MenuIo,
}

// Raw pointers in `stack` are only ever dereferenced under the Mutex
// that wraps the WlState itself.
unsafe impl Send for WlState {}

pub(crate) struct DispatchState {
    buffers: &'static BufferRegistry,
}

impl DispatchState {
    pub(crate) fn new(buffers: &'static BufferRegistry) -> Self {
        Self { buffers }
    }
}

// =====================================================================
// Dispatch impls — all no-ops; events we'd care about (wl_buffer.release,
// dmabuf format/modifier) are intentionally ignored to match the C++
// implementation's behavior.
// =====================================================================

impl Dispatch<WlRegistry, GlobalListContents> for DispatchState {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

macro_rules! noop_dispatch {
    ($($ty:ty),+ $(,)?) => {
        $(
            impl Dispatch<$ty, ()> for DispatchState {
                fn event(
                    _: &mut Self,
                    _: &$ty,
                    _: <$ty as Proxy>::Event,
                    _: &(),
                    _: &Connection,
                    _: &QueueHandle<Self>,
                ) {}
            }
        )+
    };
}

noop_dispatch!(
    WlCompositor,
    WlSubcompositor,
    WlSurface,
    WlSubsurface,
    WlRegion,
    WlShm,
    WlShmPool,
    ZwpLinuxDmabufV1,
    ZwpLinuxBufferParamsV1,
    WpViewporter,
    WpViewport,
);

// Release-state metadata for a live buffer, keyed by protocol identity rather
// than an owning proxy clone (which would make this a second owner).
//
// Under a synchronized subsurface the compositor keeps reading a buffer for a
// frame after it is replaced. Destroying or re-attaching one while `released`
// is false shows a blank frame.
struct ManagedBuffer {
    id: ObjectId,
    released: bool,
    // Holds the owning proxy only after `retire_buffer` on a still-unreleased
    // buffer: ownership moves here so it can be destroyed once its release
    // arrives. `None` while the buffer is owned elsewhere (attached / pooled).
    doomed: Option<WlBuffer>,
}

pub(crate) struct BufferRegistry {
    managed: Mutex<Vec<ManagedBuffer>>,
}

impl BufferRegistry {
    pub(crate) fn new() -> Self {
        Self {
            managed: Mutex::new(Vec::new()),
        }
    }

    fn adopt(&'static self, buf: WlBuffer) -> OwnedBuffer {
        self.managed.lock().push(ManagedBuffer {
            id: buf.id(),
            released: true,
            doomed: None,
        });
        OwnedBuffer {
            buf,
            registry: self,
        }
    }

    fn mark_attached(&self, id: &ObjectId) {
        let mut mgd = self.managed.lock();
        if let Some(m) = mgd.iter_mut().find(|m| &m.id == id) {
            m.released = false;
        }
    }

    pub(crate) fn is_idle(&self, buf: &OwnedBuffer) -> bool {
        let id = buf.id();
        self.managed
            .lock()
            .iter()
            .find(|m| m.id == id)
            .is_some_and(|m| m.released && m.doomed.is_none())
    }

    fn retire(&self, buf: WlBuffer) {
        let id = buf.id();
        let mut mgd = self.managed.lock();
        match mgd.iter().position(|m| m.id == id) {
            Some(pos) if mgd[pos].released => {
                mgd.swap_remove(pos);
                buf.destroy();
            }
            Some(pos) => mgd[pos].doomed = Some(buf),
            None => {
                debug_assert!(
                    false,
                    "retire: untracked buffer — a release may have been missed"
                );
                tracing::error!("retire: untracked buffer — a release may have been missed");
                mgd.push(ManagedBuffer {
                    id,
                    released: false,
                    doomed: Some(buf),
                });
            }
        }
    }

    pub(crate) fn note_release(&self, buffer: &WlBuffer) {
        let id = buffer.id();
        let mut mgd = self.managed.lock();
        if let Some(pos) = mgd.iter().position(|m| m.id == id) {
            if mgd[pos].doomed.is_some() {
                if let Some(doomed) = mgd.swap_remove(pos).doomed {
                    doomed.destroy();
                }
            } else {
                mgd[pos].released = true;
            }
        }
    }
}

pub(crate) fn damage_all(surface: &WlSurface) {
    surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
}

impl Dispatch<WlBuffer, ()> for DispatchState {
    fn event(
        state: &mut Self,
        buffer: &WlBuffer,
        event: <WlBuffer as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wayland_client::protocol::wl_buffer::Event::Release = event {
            state.buffers.note_release(buffer);
        }
    }
}

/// Dispatch the CEF connection's pending events (notably `wl_buffer.release`).
/// Called from the root-window read loop, the only reader of the shared display.
pub(crate) fn pump_events(rt: &'static crate::runtime::WlRuntime) {
    if let Some(state) = rt.try_core() {
        let mut st = state.lock();
        let st = &mut *st;
        let _ = st
            .queue
            .dispatch_pending(&mut DispatchState::new(rt.buffers()));
    }
}

// =====================================================================
// Init — bind globals against a dedicated EventQueue over the foreign
// (mpv-owned) wl_display.
// =====================================================================

/// SAFETY: `display_ptr` must be a live `*mut wl_display` owned by mpv.
pub(crate) unsafe fn init(
    rt: &'static crate::runtime::WlRuntime,
    display_ptr: *mut c_void,
) -> Result<WlState, String> {
    if display_ptr.is_null() {
        return Err("null display".into());
    }

    let backend = unsafe { Backend::from_foreign_display(display_ptr.cast()) };
    let conn = Connection::from_backend(backend);
    let (globals, queue) =
        registry_queue_init::<DispatchState>(&conn).map_err(|e| format!("registry init: {e}"))?;
    let qh = queue.handle();

    let compositor: WlCompositor = globals
        .bind(&qh, 1..=4, ())
        .map_err(|e| format!("bind wl_compositor: {e}"))?;
    let subcompositor: WlSubcompositor = globals
        .bind(&qh, 1..=1, ())
        .map_err(|e| format!("bind wl_subcompositor: {e}"))?;
    let shm: WlShm = globals
        .bind(&qh, 1..=1, ())
        .map_err(|e| format!("bind wl_shm: {e}"))?;
    let dmabuf: Option<ZwpLinuxDmabufV1> = globals.bind(&qh, 1..=4, ()).ok();
    let viewporter: Option<WpViewporter> = globals.bind(&qh, 1..=1, ()).ok();

    let mut state = WlState {
        conn,
        qh,
        queue,
        compositor,
        subcompositor,
        shm,
        dmabuf,
        viewporter,
        root_surface: None,
        stack: Vec::new(),
        was_fullscreen: false,
        gpu: None,
        use_gpu_paint: false,
        scene: crate::scene::Scene::default(),
        menu_io: crate::popup::MenuIo::default(),
    };

    ensure_root_locked(rt, &mut state);

    Ok(state)
}

fn surface_from_handle(
    conn: &Connection,
    handle: crate::root_window::RootSurfaceHandle,
    what: &str,
) -> Option<RootParent> {
    let raw = handle.as_ptr();
    // SAFETY: the handle carries a live `wl_proxy*` for the root `wl_surface` on
    // the same `wl_display` backing `conn` (minted by root_window from the real
    // root surface, which outlives the process).
    let id = match unsafe {
        wayland_client::backend::ObjectId::from_ptr(WlSurface::interface(), raw.cast())
    } {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(target: "Main", "{what}: ObjectId::from_ptr: {e}");
            return None;
        }
    };
    match WlSurface::from_id(conn, id) {
        Ok(s) => Some(RootParent(s)),
        Err(e) => {
            tracing::error!(target: "Main", "{what}: WlSurface::from_id: {e}");
            None
        }
    }
}

fn parent_layer_locked(st: &mut WlState, ptr: *mut PlatformSurface) {
    if ptr.is_null() {
        return;
    }
    let Some(root) = st.root_surface.clone() else {
        return;
    };
    // SAFETY: live PlatformSurface address held in `stack`, accessed under the lock.
    let s = unsafe { &mut *ptr };
    if s.subsurface.is_some() {
        return;
    }
    let Some(surface) = s.surface.as_ref() else {
        return;
    };
    let sub = root.attach_child(&st.subcompositor, surface.as_arg(), &st.qh);
    sub.set_position(0, 0);
    s.subsurface = Some(sub);
}

pub(crate) fn ensure_root_locked(rt: &'static crate::runtime::WlRuntime, st: &mut WlState) {
    if st.root_surface.is_some() {
        return;
    }
    let Some(handle) = rt.root().root_surface_handle() else {
        return;
    };
    let Some(root) = surface_from_handle(&st.conn, handle, "overlay root") else {
        return;
    };
    st.root_surface = Some(root);

    let pending: Vec<*mut PlatformSurface> = st.stack.clone();
    for ptr in pending {
        parent_layer_locked(st, ptr);
    }
    tracing::info!(target: "Main", "CEF layers parented under app root");
}

pub(crate) fn parent_layer(st: &mut WlState, ptr: *mut PlatformSurface) {
    parent_layer_locked(st, ptr);
}

// =====================================================================
// Helpers
// =====================================================================

impl WlState {
    pub(crate) fn flush(&self) {
        let _ = self.conn.flush();
    }
}

impl WlState {
    pub(crate) fn install_gpu_paint(&mut self, gpu: &'static Surfaces) {
        self.gpu = Some(gpu);
        self.use_gpu_paint = true;
    }
}

// Does an incoming frame's visible size match the authoritative physical window
// size (within tolerance)? Reads the single source, not a per-layer copy.
pub(crate) fn size_in_tolerance(rt: &crate::runtime::WlRuntime, vw: i32, vh: i32) -> bool {
    let Some(ext) = rt.window().window_extent() else {
        return true;
    };
    let (pw, ph) = (ext.physical().w(), ext.physical().h());
    (vw - pw).abs() <= TRANSITION_TOLERANCE_TEXELS && (vh - ph).abs() <= TRANSITION_TOLERANCE_TEXELS
}

// =====================================================================
// Buffer creation
// =====================================================================

/// Build an ARGB8888 `wl_shm` buffer of `w`×`h`, handing `fill` a mapping of
/// exactly `stride*h` bytes to populate (`false` aborts).
pub(crate) fn build_argb8888_shm_buffer<D>(
    reg: &'static BufferRegistry,
    shm: &WlShm,
    qh: &QueueHandle<D>,
    label: &str,
    w: i32,
    h: i32,
    fill: impl FnOnce(&mut [u8]) -> bool,
) -> Option<OwnedBuffer>
where
    D: Dispatch<WlShmPool, ()> + Dispatch<WlBuffer, ()> + 'static,
{
    let stride = w.checked_mul(4)?;
    let size = stride.checked_mul(h)?;
    if size <= 0 {
        return None;
    }
    let fd = memfd_anon(label, size as usize)?;
    {
        let mut mmap = unsafe { MmapOptions::new().len(size as usize).map_mut(&fd) }.ok()?;
        if !fill(&mut mmap) {
            return None;
        }
    }
    let pool = shm.create_pool(fd.as_fd(), size, qh, ());
    let buf = pool.create_buffer(0, w, h, stride, Format::Argb8888, qh, ());
    pool.destroy();
    Some(reg.adopt(buf))
}

/// Copy `pixels` into a fresh ARGB8888 shm buffer, or `None` if it's too short.
pub(crate) fn build_shm_buffer_from_pixels<D>(
    reg: &'static BufferRegistry,
    shm: &WlShm,
    qh: &QueueHandle<D>,
    label: &str,
    pixels: &[u8],
    w: i32,
    h: i32,
) -> Option<OwnedBuffer>
where
    D: Dispatch<WlShmPool, ()> + Dispatch<WlBuffer, ()> + 'static,
{
    build_argb8888_shm_buffer(reg, shm, qh, label, w, h, |dst| {
        if pixels.len() < dst.len() {
            return false;
        }
        dst.copy_from_slice(&pixels[..dst.len()]);
        true
    })
}

/// Create a wl_shm ARGB8888 buffer from a CPU pixel array.
pub(crate) fn create_shm_buffer(
    reg: &'static BufferRegistry,
    state: &WlState,
    pixels: &[u8],
    w: i32,
    h: i32,
) -> Option<OwnedBuffer> {
    build_shm_buffer_from_pixels(reg, &state.shm, &state.qh, "cef-sw", pixels, w, h)
}

pub(crate) struct DmabufPlane<'a> {
    pub(crate) fd: BorrowedFd<'a>,
    pub(crate) stride: u32,
    pub(crate) modifier: u64,
    pub(crate) w: i32,
    pub(crate) h: i32,
}

/// Create a dmabuf-backed wl_buffer from a single-plane fd.
pub(crate) fn create_dmabuf_buffer(
    reg: &'static BufferRegistry,
    dmabuf: &ZwpLinuxDmabufV1,
    qh: &QueueHandle<DispatchState>,
    plane: DmabufPlane<'_>,
) -> Option<OwnedBuffer> {
    let params: ZwpLinuxBufferParamsV1 = dmabuf.create_params(qh, ());
    params.add(
        plane.fd,
        0,
        0,
        plane.stride,
        (plane.modifier >> 32) as u32,
        (plane.modifier & 0xffff_ffff) as u32,
    );
    let buf = params.create_immed(
        plane.w,
        plane.h,
        DRM_FORMAT_ARGB8888,
        DmabufFlags::empty(),
        qh,
        (),
    );
    params.destroy();
    Some(reg.adopt(buf))
}

/// Create a CLOEXEC anonymous memfd of the given size and truncate it.
pub(crate) fn memfd_anon(name: &str, size: usize) -> Option<OwnedFd> {
    let c = std::ffi::CString::new(name).ok()?;
    let owned = nix::sys::memfd::memfd_create(c.as_c_str(), MFdFlags::MFD_CLOEXEC).ok()?;
    nix::unistd::ftruncate(&owned, size as i64).ok()?;
    Some(owned)
}
