//! Wayland surface / present / transition state, driven through the
//! [`crate::wl_ops`] entry points reached from the Platform adapter
//! ([`crate::make_platform`]).
//!
//! Owns:
//!   * A dedicated `EventQueue` over an mpv-owned `wl_display`
//!     (foreign-display backend, never closes the fd)
//!   * Bindings for `wl_compositor`, `wl_subcompositor`, `wl_shm`,
//!     `zwp_linux_dmabuf_v1`, `wp_viewporter`
//!   * The list of per-layer `PlatformSurface`s
//!   * The fullscreen-transition state machine (begin/end + tolerance
//!     gate for the paint path)
//!
//! All mutable state lives behind a single `Mutex` — mirrors the C++
//! `surface_mtx` discipline. Coarse locking is intentional: the paint
//! path holds the lock during commit/flush, and finer-grained locking
//! would risk null-attach vs. commit ordering races.

use parking_lot::{Condvar, Mutex};
use std::ffi::c_void;
use std::os::fd::BorrowedFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use jfn_gpu_paint::Surfaces;
use jfn_platform_abi::Visibility;

use crate::layer::SurfaceRef;
use crate::layer_actor::LayerActor;

use smithay_client_toolkit::compositor::Region;
use smithay_client_toolkit::error::GlobalError;
use wayland_client::globals::BindError;

use smithay_client_toolkit::globals::ProvidesBoundGlobal;
use smithay_client_toolkit::shm::slot::{Buffer as SlotBuffer, SlotPool};
use wayland_backend::client::Backend;
use wayland_client::backend::ObjectId;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{
    wl_buffer::WlBuffer,
    wl_callback::WlCallback,
    wl_compositor::WlCompositor,
    wl_registry::WlRegistry,
    wl_shm::{Format, WlShm},
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
    ) -> Subsurface {
        Subsurface::create(subcompositor, surface, &self.0, qh)
    }
}

pub(crate) struct Subsurface(WlSubsurface);

impl Subsurface {
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

    /// Commits apply on this surface's own commit rather than the parent's.
    ///
    /// A synchronized subsurface only reaches the compositor when its parent
    /// commits, so the frame callbacks a FIFO swapchain throttles on never
    /// arrive and the second present blocks forever.
    pub(crate) fn set_desync(&self) {
        self.0.set_desync();
    }

    pub(crate) fn destroy(self) {
        self.0.destroy();
    }
}

pub(crate) struct DmabufBuffer {
    buf: WlBuffer,
    registry: &'static DmabufRegistry,
}

impl DmabufBuffer {
    fn id(&self) -> ObjectId {
        self.buf.id()
    }

    /// Marks the buffer in-use until its next release.
    pub(crate) fn attach_to(&self, surface: &WlSurface) {
        self.registry.mark_attached(&self.id());
        surface.attach(Some(&self.buf), 0, 0);
    }
}

impl Drop for DmabufBuffer {
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
    pub buf: DmabufBuffer,
}

#[derive(Clone, Copy)]
pub(crate) enum FrameBuffer<'a> {
    Shm(&'a SlotBuffer),
    Dmabuf(&'a DmabufBuffer),
}

impl FrameBuffer<'_> {
    pub(crate) fn attach_to(&self, surface: &WlSurface) {
        match self {
            Self::Shm(buf) => {
                if let Err(e) = buf.attach_to(surface) {
                    tracing::error!(target: "Main", "attach shm buffer: {e}");
                }
            }
            Self::Dmabuf(buf) => buf.attach_to(surface),
        }
    }
}

pub(crate) enum AttachedBuffer {
    Shm(SlotBuffer),
    Dmabuf(DmabufBuffer),
}

impl AttachedBuffer {
    pub(crate) fn borrow(&self) -> FrameBuffer<'_> {
        match self {
            Self::Shm(buf) => FrameBuffer::Shm(buf),
            Self::Dmabuf(buf) => FrameBuffer::Dmabuf(buf),
        }
    }

    pub(crate) fn attach_to(&self, surface: &WlSurface) {
        self.borrow().attach_to(surface);
    }
}

#[derive(Clone)]
pub(crate) struct CompositorGlobal(WlCompositor);

impl ProvidesBoundGlobal<WlCompositor, 6> for CompositorGlobal {
    fn bound_global(&self) -> Result<WlCompositor, GlobalError> {
        Ok(self.0.clone())
    }
}

#[derive(Clone)]
pub(crate) struct ShmGlobal(WlShm);

impl ShmGlobal {
    pub(crate) fn new(shm: WlShm) -> Self {
        Self(shm)
    }
}

impl ProvidesBoundGlobal<WlShm, 1> for ShmGlobal {
    fn bound_global(&self) -> Result<WlShm, GlobalError> {
        Ok(self.0.clone())
    }
}

const POOL_INITIAL_BYTES: usize = 4 * 1024 * 1024;

pub(crate) fn new_slot_pool(shm: &ShmGlobal, what: &str) -> Option<SlotPool> {
    match SlotPool::new(POOL_INITIAL_BYTES, shm) {
        Ok(pool) => Some(pool),
        Err(e) => {
            tracing::error!(target: "Main", "{what}: shm pool: {e}");
            None
        }
    }
}

pub(crate) fn draw_argb8888(
    pool: &mut SlotPool,
    w: i32,
    h: i32,
    fill: impl FnOnce(&mut [u8]) -> bool,
) -> Option<SlotBuffer> {
    let stride = w.checked_mul(4)?;
    if stride <= 0 || h <= 0 {
        return None;
    }
    // The pool rounds slots up to a 64-byte boundary, so the canvas it hands
    // back can be longer than the buffer's pixel data. Trim it to what the
    // compositor will actually read.
    let len = (h as usize).checked_mul(stride as usize)?;
    let (buffer, canvas) = pool.create_buffer(w, h, stride, Format::Argb8888).ok()?;
    if !fill(canvas.get_mut(..len)?) {
        return None;
    }
    Some(buffer)
}

pub(crate) fn draw_from_pixels(
    pool: &mut SlotPool,
    pixels: &[u8],
    w: i32,
    h: i32,
) -> Option<SlotBuffer> {
    draw_argb8888(pool, w, h, |dst| {
        if pixels.len() < dst.len() {
            return false;
        }
        dst.copy_from_slice(&pixels[..dst.len()]);
        true
    })
}

pub(crate) struct PlatformSurface {
    pub surface: Option<SurfaceRef>,
    pub subsurface: Option<Subsurface>,
    /// The process's single statement of whether this surface is on screen; the
    /// actor mirrors it only as the request it has yet to commit.
    pub visibility: Visibility,
    pub layer_actor: Option<LayerActor>,
    /// Set by `Platform::surface_window_target`. The actor attaches no buffer
    /// and drops every present for a surface with this set.
    pub external: bool,
    pub top_logical: i32,
    pub top_physical: i32,
}

impl PlatformSurface {
    pub(crate) fn new(visibility: Visibility) -> PlatformSurface {
        PlatformSurface {
            surface: None,
            subsurface: None,
            visibility,
            layer_actor: None,
            external: false,
            top_logical: 0,
            top_physical: 0,
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
    pub shm: ShmGlobal,
    pub dmabuf: Option<ZwpLinuxDmabufV1>,
    pub viewporter: WpViewporter,

    pub root_surface: Option<RootParent>,

    /// Stack order, bottom-to-top. Raw pointers are valid for the
    /// lifetime of each `PlatformSurface` (heap-allocated via `Box`,
    /// removed before drop).
    pub stack: Vec<*mut PlatformSurface>,

    pub was_fullscreen: bool,

    pub gpu: Option<&'static Surfaces>,
    /// When true, a software present routes through each surface's GPU paint
    /// worker (Vulkan WSI) instead of `wl_shm`.
    pub use_gpu_paint: bool,

    pub scene: crate::scene::Scene,
}

// Raw pointers in `stack` are only ever dereferenced under the Mutex
// that wraps the WlState itself.
unsafe impl Send for WlState {}

pub(crate) struct DispatchState {
    buffers: &'static DmabufRegistry,
    callbacks: &'static Callbacks,
}

impl DispatchState {
    pub(crate) fn new(buffers: &'static DmabufRegistry, callbacks: &'static Callbacks) -> Self {
        Self { buffers, callbacks }
    }
}

/// The `wl_callback`s the app waits on: the `wl_surface.frame` of a commit
/// that carries a buffer, the `wl_display.sync` of one that empties the
/// surface.
pub(crate) struct Callbacks {
    armed: Mutex<Vec<(ObjectId, Arc<Signal>)>>,
    closed: AtomicBool,
}

/// One acknowledgement's resolved flag and the parking spot for its waiter.
struct Signal {
    done: Mutex<bool>,
    woken: Condvar,
}

impl Signal {
    fn resolve(&self) {
        *self.done.lock() = true;
        self.woken.notify_all();
    }
}

impl Callbacks {
    pub(crate) fn new() -> Callbacks {
        Callbacks {
            armed: Mutex::new(Vec::new()),
            closed: AtomicBool::new(false),
        }
    }

    /// Arms `callback` and hands back the token its `done` event resolves.
    pub(crate) fn arm(&'static self, callback: &WlCallback) -> Acked {
        let signal = Arc::new(Signal {
            done: Mutex::new(false),
            woken: Condvar::new(),
        });
        if self.closed.load(Ordering::Acquire) {
            signal.resolve();
            return Acked { signal };
        }
        self.armed.lock().push((callback.id(), Arc::clone(&signal)));
        Acked { signal }
    }

    pub(crate) fn note_done(&self, callback: &WlCallback) {
        let id = callback.id();
        let mut armed = self.armed.lock();
        let Some(pos) = armed.iter().position(|(armed_id, _)| *armed_id == id) else {
            return;
        };
        let (_, signal) = armed.swap_remove(pos);
        drop(armed);
        signal.resolve();
    }

    /// Releases every waiter, so no acknowledgement outlives the connection.
    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::Release);
        let armed = std::mem::take(&mut *self.armed.lock());
        for (_, signal) in armed {
            signal.resolve();
        }
    }
}

/// One compositor acknowledgement in flight.
pub(crate) struct Acked {
    signal: Arc<Signal>,
}

impl Acked {
    /// Blocks until the compositor fired the callback, or the connection closed.
    pub(crate) fn wait(self) {
        let mut done = self.signal.done.lock();
        while !*done {
            self.signal.woken.wait(&mut done);
        }
    }
}

impl Dispatch<WlCallback, ()> for DispatchState {
    fn event(
        state: &mut Self,
        callback: &WlCallback,
        event: <WlCallback as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wayland_client::protocol::wl_callback::Event::Done { .. } = event {
            state.callbacks.note_done(callback);
        }
    }
}

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

// =====================================================================
// Dispatch impls — all no-ops; events we'd care about (wl_buffer.release,
// dmabuf format/modifier) are intentionally ignored to match the C++
// implementation's behavior.
// =====================================================================

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
    WlShm,
    ZwpLinuxDmabufV1,
    ZwpLinuxBufferParamsV1,
    WpViewporter,
    WpViewport,
);

// Release-state metadata for a live dmabuf buffer, keyed by protocol identity
// rather than an owning proxy clone (which would make this a second owner).
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

pub(crate) struct DmabufRegistry {
    managed: Mutex<Vec<ManagedBuffer>>,
}

impl DmabufRegistry {
    pub(crate) fn new() -> Self {
        Self {
            managed: Mutex::new(Vec::new()),
        }
    }

    fn adopt(&'static self, buf: WlBuffer) -> DmabufBuffer {
        self.managed.lock().push(ManagedBuffer {
            id: buf.id(),
            released: true,
            doomed: None,
        });
        DmabufBuffer {
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

    pub(crate) fn is_idle(&self, buf: &DmabufBuffer) -> bool {
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
            .dispatch_pending(&mut DispatchState::new(rt.buffers(), rt.callbacks()));
    }
}

// =====================================================================
// Init — bind globals against a dedicated EventQueue over the foreign
// (mpv-owned) wl_display.
// =====================================================================

#[derive(Debug, thiserror::Error)]
pub(crate) enum InitError {
    #[error("null display")]
    NullDisplay,
    #[error("registry init: {0}")]
    Registry(#[from] wayland_client::globals::GlobalError),
    #[error("bind {interface}: {source}")]
    Bind {
        interface: &'static str,
        #[source]
        source: BindError,
    },
}

pub(crate) fn bind_error(interface: &'static str) -> impl FnOnce(BindError) -> InitError {
    move |source| InitError::Bind { interface, source }
}

/// SAFETY: `display_ptr` must be a live `*mut wl_display` owned by mpv.
pub(crate) unsafe fn init(
    rt: &'static crate::runtime::WlRuntime,
    display_ptr: *mut c_void,
) -> Result<WlState, InitError> {
    if display_ptr.is_null() {
        return Err(InitError::NullDisplay);
    }

    let backend = unsafe { Backend::from_foreign_display(display_ptr.cast()) };
    let conn = Connection::from_backend(backend);
    let (globals, queue) = registry_queue_init::<DispatchState>(&conn)?;
    let qh = queue.handle();

    let compositor: WlCompositor = globals
        .bind(&qh, 1..=4, ())
        .map_err(bind_error("wl_compositor"))?;
    let subcompositor: WlSubcompositor = globals
        .bind(&qh, 1..=1, ())
        .map_err(bind_error("wl_subcompositor"))?;
    let shm = ShmGlobal::new(globals.bind(&qh, 1..=1, ()).map_err(bind_error("wl_shm"))?);
    let dmabuf: Option<ZwpLinuxDmabufV1> = globals.bind(&qh, 1..=4, ()).ok();
    let viewporter: WpViewporter = globals
        .bind(&qh, 1..=1, ())
        .map_err(bind_error("wp_viewporter"))?;

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
    // A surface already claimed for external presentation must not wait on a
    // parent commit to reach the compositor.
    if s.external {
        sub.set_desync();
    }
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

    pub(crate) fn empty_region(&self) -> Option<Region> {
        Region::new(&CompositorGlobal(self.compositor.clone()))
            .inspect_err(|e| tracing::error!(target: "Main", "empty input region: {e}"))
            .ok()
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
// Dmabuf buffer creation
// =====================================================================

pub(crate) struct DmabufPlane<'a> {
    pub(crate) fd: BorrowedFd<'a>,
    pub(crate) stride: u32,
    pub(crate) modifier: u64,
    pub(crate) w: i32,
    pub(crate) h: i32,
}

/// Create a dmabuf-backed wl_buffer from a single-plane fd.
pub(crate) fn create_dmabuf_buffer(
    reg: &'static DmabufRegistry,
    dmabuf: &ZwpLinuxDmabufV1,
    qh: &QueueHandle<DispatchState>,
    plane: DmabufPlane<'_>,
) -> Option<DmabufBuffer> {
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
