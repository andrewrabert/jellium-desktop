use std::os::fd::AsFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};

use smithay_client_toolkit::shm::slot::SlotPool;
use wayland_client::QueueHandle;
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1;

use jfn_gpu_paint::{FrameSize as PhysicalSize, Pixels, PresentFailed, SharedTexture, Surfaces};
use jfn_mailbox::Mailbox;
use jfn_platform_abi::{Ack, FrameRetry, FrameSource, JfnRect, Visibility, VisibilityCommit};

use crate::layer::{Committed, FrameCommit, LayerSurface, PresentError, ViewportState};
use crate::runtime::WlRuntime;
use crate::wl_ops::dmabuf_pool_key;
use crate::wl_state::{
    Acked, AttachedBuffer, DispatchState, DmabufBuf, DmabufBuffer, DmabufPlane, FrameBuffer,
    ShmGlobal, create_dmabuf_buffer, draw_from_pixels, new_slot_pool,
};

const DMABUF_POOL_CAP: usize = 16;

pub(crate) struct LayerDeps {
    pub(crate) rt: &'static WlRuntime,
    pub(crate) qh: QueueHandle<DispatchState>,
    pub(crate) shm: ShmGlobal,
    pub(crate) dmabuf: Option<ZwpLinuxDmabufV1>,
}

pub(crate) enum LayerBackend {
    Gpu(&'static Surfaces),
    Shm,
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum Kind {
    Gpu,
    Shm,
}

struct ShmRect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    pixels: Vec<u8>,
}

struct GpuPayload {
    pixels: Vec<u8>,
    dirty: Vec<JfnRect>,
    width: u32,
    height: u32,
    stride: u32,
}

struct ShmPayload {
    rects: Vec<ShmRect>,
    full_pixels: Option<Vec<u8>>,
    width: i32,
    height: i32,
}

/// One software frame as its producer handed it over: the rows it covers, the
/// regions that changed, and the geometry both are read against.
struct SoftwareFrame<'a> {
    pixels: &'a [u8],
    dirty: &'a [JfnRect],
    width: i32,
    height: i32,
    stride: usize,
}

enum PendingFrame {
    Gpu(GpuPayload),
    Shm(ShmPayload),
    Dmabuf(SharedTexture),
}

/// Every event that invalidates the shadow (dmabuf, hide, resize) must reset it
/// to `Stale`, or a later dirty-only frame patches stale pixels.
enum ShadowState {
    Stale,
    Valid { size: (i32, i32) },
}

struct LayerState {
    pending: Option<PendingFrame>,
    /// The producer that owes the pending frame's successor, held so a frame
    /// the swapchain could not take can ask for the one that replaces it.
    source: Option<Arc<dyn FrameSource>>,
    shadow: ShadowState,
    viewport: ViewportState,
    /// The visibility the worker has yet to carry into a commit. Not a second
    /// statement of visibility: the surface's own field is the only one.
    requested: Option<Visibility>,
    /// Numbers the visibility requests; the requester waits for [`Self::acked`]
    /// to carry its own number or a later one.
    request_seq: u64,
    /// The acknowledgement of the commit that carried request `n`, taken by the
    /// requester waiting on it.
    acked: Option<(u64, Acked)>,
    viewport_dirty: bool,
    shutdown: bool,
}

impl LayerState {
    fn new(viewport: ViewportState) -> LayerState {
        LayerState {
            pending: None,
            source: None,
            shadow: ShadowState::Stale,
            viewport,
            requested: None,
            request_seq: 0,
            acked: None,
            viewport_dirty: false,
            shutdown: false,
        }
    }

    /// Returns this request's number, the one its acknowledgement carries.
    fn request_visibility(&mut self, visibility: Visibility) -> u64 {
        self.requested = Some(visibility);
        self.request_seq = self.request_seq.wrapping_add(1);
        if !visibility.is_shown() {
            self.pending = None;
            self.source = None;
            self.shadow = ShadowState::Stale;
        }
        self.request_seq
    }

    /// A newer frame supersedes the pending one it replaces.
    fn supersede(&mut self, successor: PendingFrame) {
        self.pending = Some(successor);
    }

    fn resize(&mut self, viewport: ViewportState) {
        // Callers invoke this per frame; without the guard an unchanged extent
        // would stale the shadow every frame and defeat dirty-only coalescing.
        if self.viewport == viewport {
            return;
        }
        self.viewport = viewport;
        self.viewport_dirty = true;
        self.shadow = ShadowState::Stale;
    }

    fn present_dmabuf(&mut self, frame: SharedTexture) {
        self.supersede(PendingFrame::Dmabuf(frame));
        self.shadow = ShadowState::Stale;
    }

    fn enqueue_gpu(&mut self, payload: GpuPayload) {
        self.supersede(PendingFrame::Gpu(payload));
    }

    fn needs_full_copy(&self, width: i32, height: i32) -> bool {
        !matches!(self.shadow, ShadowState::Valid { size } if size == (width, height))
            || matches!(
                &self.pending,
                Some(PendingFrame::Shm(ShmPayload {
                    full_pixels: Some(_),
                    ..
                }))
            )
    }

    /// Merges dirty rects into a co-pending dirty-only frame of the same size
    /// instead of replacing it and dropping the earlier rects. An empty-damage
    /// frame still stores: nothing the stream accepted is dropped for carrying
    /// no rects.
    fn store_shm(
        &mut self,
        rects: Vec<ShmRect>,
        full_pixels: Option<Vec<u8>>,
        width: i32,
        height: i32,
    ) {
        if full_pixels.is_some() {
            self.shadow = ShadowState::Valid {
                size: (width, height),
            };
        }
        if full_pixels.is_none()
            && let Some(PendingFrame::Shm(existing)) = self.pending.as_mut()
            && existing.full_pixels.is_none()
            && existing.width == width
            && existing.height == height
        {
            existing.rects.extend(rects);
            return;
        }
        self.supersede(PendingFrame::Shm(ShmPayload {
            rects,
            full_pixels,
            width,
            height,
        }));
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Route {
    Gpu,
    Shm,
}

fn route_software(kind: Kind, gpu_failed: bool) -> Route {
    match kind {
        Kind::Gpu if !gpu_failed => Route::Gpu,
        _ => Route::Shm,
    }
}

pub(crate) struct LayerActor {
    kind: Kind,
    mailbox: Mailbox<LayerState>,
    gpu_failed: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl LayerActor {
    pub(crate) fn new(
        backend: LayerBackend,
        deps: LayerDeps,
        layer: LayerSurface,
        viewport_state: ViewportState,
        visibility: Visibility,
    ) -> Self {
        let kind = match backend {
            LayerBackend::Gpu(_) => Kind::Gpu,
            LayerBackend::Shm => Kind::Shm,
        };
        let mailbox = Mailbox::new(LayerState::new(viewport_state));
        let gpu_failed = Arc::new(AtomicBool::new(false));
        let worker_mailbox = mailbox.clone();
        let worker_failed = Arc::clone(&gpu_failed);
        let thread = thread::spawn(move || {
            run(
                backend,
                deps,
                layer,
                worker_mailbox,
                worker_failed,
                visibility,
            );
        });
        Self {
            kind,
            mailbox,
            gpu_failed,
            thread: Some(thread),
        }
    }

    pub(crate) fn resize(&self, lw: i32, lh: i32, pw: i32, ph: i32) {
        if pw <= 0 || ph <= 0 {
            return;
        }
        self.mailbox
            .update(|s| s.resize(ViewportState { lw, lh, pw, ph }));
    }

    /// Requests `visibility`; the returned commit is acknowledged when the
    /// compositor fires the callback armed with the commit that carried it.
    pub(crate) fn apply_visibility(&self, visibility: Visibility) -> VisibilityCommit {
        let seq = self.mailbox.update(|s| s.request_visibility(visibility));
        let mailbox = self.mailbox.clone();
        VisibilityCommit::issued(
            visibility,
            Ack::deferred(Box::new(move || {
                let acked = mailbox.wait(
                    |s| s.acked.as_ref().is_some_and(|(n, _)| *n >= seq) || s.shutdown,
                    |s| s.acked.take().map(|(_, acked)| acked),
                );
                if let Some(acked) = acked {
                    acked.wait();
                }
            })),
        )
    }

    /// Enqueues `frame`; the slot's occupant is discharged by a commit or by
    /// the frame that supersedes it, never by being dropped.
    pub(crate) fn present_dmabuf(&self, frame: SharedTexture, source: Arc<dyn FrameSource>) {
        self.mailbox.update(|s| {
            s.present_dmabuf(frame);
            s.source = Some(source);
        });
    }

    pub(crate) fn present_software(
        &self,
        pixels: &[u8],
        width: i32,
        height: i32,
        dirty: &[JfnRect],
        source: Arc<dyn FrameSource>,
    ) {
        let stride = (width as usize).saturating_mul(4);
        let len = (height as usize).saturating_mul(stride).min(pixels.len());
        let frame = SoftwareFrame {
            pixels: &pixels[..len],
            dirty,
            width,
            height,
            stride,
        };
        match route_software(self.kind, self.gpu_failed.load(Ordering::Acquire)) {
            Route::Gpu => self.enqueue_gpu(&frame, source),
            Route::Shm => self.enqueue_shm(&frame, source),
        }
    }

    fn enqueue_gpu(&self, frame: &SoftwareFrame<'_>, source: Arc<dyn FrameSource>) {
        let dirty = frame.dirty.to_vec();
        self.mailbox.update(|s| {
            s.enqueue_gpu(GpuPayload {
                pixels: frame.pixels.to_vec(),
                dirty,
                width: frame.width as u32,
                height: frame.height as u32,
                stride: frame.stride as u32,
            });
            s.source = Some(source);
        });
    }

    fn enqueue_shm(&self, frame: &SoftwareFrame<'_>, source: Arc<dyn FrameSource>) {
        let rects: Vec<ShmRect> = frame
            .dirty
            .iter()
            .filter_map(|rect| {
                copy_dirty_rect(frame.pixels, frame.stride, frame.width, frame.height, rect)
            })
            .collect();

        self.mailbox.update(|s| {
            let full_pixels = s
                .needs_full_copy(frame.width, frame.height)
                .then(|| frame.pixels.to_vec());
            s.store_shm(rects, full_pixels, frame.width, frame.height);
            s.source = Some(source);
        });
    }

    pub(crate) fn shutdown(mut self) {
        self.mailbox.update(|s| {
            s.shutdown = true;
            s.pending = None;
            s.source = None;
        });
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

// ===================================================================
// Worker loop decision (pure)
// ===================================================================

/// The primary frame op for one worker iteration.
#[derive(Debug, PartialEq)]
pub(crate) enum Action<F> {
    Hide,
    Present(F),
    ReapplyViewport,
    Nop,
}

/// The frame op for one worker iteration, from one mailbox snapshot. Driven by
/// the final desired `visibility`: a frame arriving in the same wake as a
/// coalesced hide+show is presented, not dropped.
///
/// # Examples
/// ```ignore
/// let action = decide(Some(7u32), Visibility::Shown, false);
/// assert_eq!(action, Action::Present(7));
/// ```
pub(crate) fn decide<F>(
    pending: Option<F>,
    visibility: Visibility,
    viewport_dirty: bool,
) -> Action<F> {
    if !visibility.is_shown() {
        Action::Hide
    } else if let Some(frame) = pending {
        Action::Present(frame)
    } else if viewport_dirty {
        Action::ReapplyViewport
    } else {
        Action::Nop
    }
}

// ===================================================================
// Actor thread
// ===================================================================

#[derive(Default)]
struct ShmShadow {
    pixels: Vec<u8>,
    size: (i32, i32),
}

enum Backend {
    Gpu {
        painter: Option<Box<jfn_gpu_paint::Surface<'static>>>,
    },
    Shm {
        shadow: ShmShadow,
    },
}

fn degrade(
    backend: &mut Backend,
    gpu_failed: &AtomicBool,
) -> Option<Box<jfn_gpu_paint::Surface<'static>>> {
    let old = match backend {
        Backend::Gpu { painter } => painter.take(),
        Backend::Shm { .. } => None,
    };
    *backend = Backend::Shm {
        shadow: ShmShadow::default(),
    };
    gpu_failed.store(true, Ordering::Release);
    old
}

struct Runner {
    rt: &'static WlRuntime,
    qh: QueueHandle<DispatchState>,
    shm_pool: Option<SlotPool>,
    dmabuf: Option<ZwpLinuxDmabufV1>,
    backend: Backend,
    gpu: Option<&'static Surfaces>,
    gpu_failed: Arc<AtomicBool>,
    /// Gates present-failure logging to the first failure of a failing streak.
    present_failing: bool,
    dmabuf_pool: Vec<DmabufBuf>,
    /// Held until the compositor releases it: an attached buffer must outlive
    /// its use by the compositor.
    current: Option<AttachedBuffer>,
}

fn run(
    backend: LayerBackend,
    deps: LayerDeps,
    layer: LayerSurface,
    mailbox: Mailbox<LayerState>,
    gpu_failed: Arc<AtomicBool>,
    initial: Visibility,
) {
    let LayerDeps {
        rt,
        qh,
        shm,
        dmabuf,
    } = deps;
    let (backend, gpu) = match backend {
        LayerBackend::Gpu(gpu) => (Backend::Gpu { painter: None }, Some(gpu)),
        LayerBackend::Shm => (
            Backend::Shm {
                shadow: ShmShadow::default(),
            },
            None,
        ),
    };
    let mut runner = Runner {
        rt,
        qh,
        shm_pool: new_slot_pool(&shm, "cef layer"),
        dmabuf,
        backend,
        gpu,
        gpu_failed,
        present_failing: false,
        dmabuf_pool: Vec::new(),
        current: None,
    };
    let mut visibility = initial;
    let mut retry = FrameRetry::default();

    loop {
        let ready = |s: &LayerState| {
            s.pending.is_some() || s.shutdown || s.requested.is_some() || s.viewport_dirty
        };
        let take = |s: &mut LayerState| {
            let viewport_dirty = std::mem::take(&mut s.viewport_dirty);
            (
                s.pending.take().zip(s.source.take()),
                s.viewport,
                s.requested.take().map(|v| (v, s.request_seq)),
                viewport_dirty,
                s.shutdown,
            )
        };
        let taken = match retry.due() {
            Some(at) => mailbox.wait_until(at, ready, take),
            None => Some(mailbox.wait(ready, take)),
        };
        // Nothing arrived before the held frame came due: present it against
        // the viewport it would have used.
        let (pending, viewport, requested, viewport_dirty, shutdown) =
            taken.unwrap_or_else(|| (None, mailbox.peek(|s| s.viewport), None, false, false));

        if shutdown {
            break;
        }
        if let Some((requested, _)) = requested {
            visibility = requested;
        }

        let (pending, source) = match retry.take(pending) {
            Some((frame, source)) => (Some(frame), Some(source)),
            None => (None, None),
        };
        let wake = Wake {
            action: decide(pending, visibility, viewport_dirty),
            source,
            visibility,
            viewport,
            viewport_dirty,
            request_seq: requested.map(|(_, seq)| seq),
        };
        if let Some(acked) = runner.service(&layer, wake, &mut retry) {
            mailbox.update(|s| s.acked = Some(acked));
        }
    }

    runner.shutdown();
}

/// One wake of the actor: the frame op it decided on and the state that op
/// commits against.
struct Wake {
    action: Action<PendingFrame>,
    /// The producer named by the pending frame, asked for its successor when
    /// the frame is not presented.
    source: Option<Arc<dyn FrameSource>>,
    visibility: Visibility,
    viewport: ViewportState,
    viewport_dirty: bool,
    /// The visibility request this wake carries, if any.
    request_seq: Option<u64>,
}

impl Runner {
    /// Issues this wake's commit stream and returns the acknowledgement of a
    /// visibility request it carried.
    fn service(
        &mut self,
        layer: &LayerSurface,
        wake: Wake,
        retry: &mut FrameRetry<PendingFrame>,
    ) -> Option<(u64, Acked)> {
        let Wake {
            action,
            source,
            visibility,
            viewport,
            viewport_dirty,
            request_seq,
        } = wake;
        // Whether this wake's commit stream attached a buffer, which decides
        // which callback a visibility request's commit can be acknowledged by.
        let mut carries_buffer = false;
        let mut layer_committed = match action {
            Action::Hide => self.hide(layer),
            Action::Present(frame) => match self.present(&frame, layer, viewport) {
                Ok(_committed) => {
                    self.present_failing = false;
                    carries_buffer = true;
                    true
                }
                Err(PresentError::Gpu(PresentFailed::Deferred(deferred))) => {
                    // The producer is named by the frame it made; a frame with
                    // no named producer is not held, because nothing could be
                    // asked for its successor.
                    if let Some(source) = source {
                        retry.defer(frame, source, deferred.retry_at());
                    }
                    false
                }
                Err(err) => {
                    self.on_present_error(err);
                    false
                }
            },
            Action::ReapplyViewport => {
                // Zero source args leave the latched source untouched; only the
                // destination is rescaled to the new logical size.
                layer.set_viewport(0, 0, viewport.lw, viewport.lh);
                layer.commit();
                true
            }
            Action::Nop => false,
        };

        // The visibility gate keeps this fallback commit off a hidden GPU/WSI
        // surface, whose buffers the compositor's swapchain owns.
        if visibility.is_shown() && !layer_committed && viewport_dirty {
            // Zero source args leave the latched source untouched; only the
            // destination is rescaled to the new logical size.
            layer.set_viewport(0, 0, viewport.lw, viewport.lh);
            layer.commit();
            layer_committed = true;
        }

        // A request completes only on a commit carrying it, and that commit is
        // the last one this wake issues, so its acknowledgement stands for
        // everything before it.
        let acked = request_seq.map(|seq| {
            (
                seq,
                layer.commit_acked(self.rt.callbacks(), &self.qh, carries_buffer),
            )
        });
        if acked.is_some() {
            layer_committed = true;
        }

        if layer_committed {
            layer.flush();
            self.rt.root().request_present();
        }
        acked
    }
}

impl Runner {
    fn set_current(&mut self, buf: Option<AttachedBuffer>) {
        self.current = buf;
    }

    /// Hiding is a commit that empties the surface, never a restack: nothing
    /// about a surface's position changes with its visibility. Always commits,
    /// so it always reports `true`.
    fn hide(&mut self, layer: &LayerSurface) -> bool {
        layer.attach_none();
        layer.commit();
        self.set_current(None);
        true
    }

    fn on_present_error(&mut self, err: PresentError) {
        let degraded = err.is_degrading();
        if degraded {
            let old = degrade(&mut self.backend, &self.gpu_failed);
            if let Some(painter) = old {
                drop(painter);
            }
        }
        if !self.present_failing {
            self.present_failing = true;
            tracing::warn!(error = %err, degraded, "wayland layer actor: present failed");
        }
    }

    /// Presents through the path this runner's backend has: a gpu payload
    /// reaching a degraded backend goes out as a full shm frame, and an shm
    /// payload reaching a gpu backend degrades it first.
    fn present(
        &mut self,
        frame: &PendingFrame,
        layer: &LayerSurface,
        vps: ViewportState,
    ) -> Result<Committed, PresentError> {
        match frame {
            PendingFrame::Gpu(p) => match (&self.backend, self.gpu) {
                (Backend::Gpu { .. }, Some(_)) => self.present_gpu(layer, vps, p),
                _ => {
                    self.present_pixels_shm(layer, vps, &p.pixels, p.width as i32, p.height as i32)
                }
            },
            PendingFrame::Shm(p) => {
                if matches!(self.backend, Backend::Gpu { .. }) {
                    drop(degrade(&mut self.backend, &self.gpu_failed));
                }
                self.present_shm(layer, vps, p)
            }
            PendingFrame::Dmabuf(frame) => self.present_dmabuf(layer, vps, frame),
        }
    }

    /// The shm path for a payload the gpu path could not take: a full frame,
    /// because the backend's shadow never saw the pixels this one patches.
    fn present_pixels_shm(
        &mut self,
        layer: &LayerSurface,
        vps: ViewportState,
        pixels: &[u8],
        width: i32,
        height: i32,
    ) -> Result<Committed, PresentError> {
        if matches!(self.backend, Backend::Gpu { .. }) {
            drop(degrade(&mut self.backend, &self.gpu_failed));
        }
        self.present_shm(
            layer,
            vps,
            &ShmPayload {
                rects: Vec::new(),
                full_pixels: Some(pixels.to_vec()),
                width,
                height,
            },
        )
    }

    fn present_gpu(
        &mut self,
        layer: &LayerSurface,
        vps: ViewportState,
        p: &GpuPayload,
    ) -> Result<Committed, PresentError> {
        let (Backend::Gpu { painter }, Some(gpu)) = (&mut self.backend, self.gpu) else {
            return self.present_pixels_shm(layer, vps, &p.pixels, p.width as i32, p.height as i32);
        };
        if painter.is_none() {
            let Some(target) = layer.window_target() else {
                return self.present_pixels_shm(
                    layer,
                    vps,
                    &p.pixels,
                    p.width as i32,
                    p.height as i32,
                );
            };
            // This path only ever carries CPU pixels; Wayland's shared frames
            // are a `wl_buffer` dmabuf attach and never reach wgpu.
            let new = gpu
                .new_surface(
                    target,
                    PhysicalSize {
                        w: p.width as i32,
                        h: p.height as i32,
                    },
                )
                .map_err(|e| PresentError::Gpu(PresentFailed::from(e)))?;
            *painter = Some(Box::new(new));
        }
        let Some(painter) = painter.as_mut() else {
            return self.present_pixels_shm(layer, vps, &p.pixels, p.width as i32, p.height as i32);
        };
        painter.resize(PhysicalSize {
            w: vps.pw.max(1),
            h: vps.ph.max(1),
        });
        let pixel_frame = Pixels {
            size: PhysicalSize {
                w: p.width as i32,
                h: p.height as i32,
            },
            stride: p.stride,
            bgra: &p.pixels,
            dirty: &p.dirty,
        };
        // Set the viewport source inside the present closure, not here: a
        // dropped frame must not leave a source pending ahead of the next
        // buffer. Clamped to min(buffer, physical) to stay within bounds.
        let src_w = (p.width as i32).min(vps.pw);
        let src_h = (p.height as i32).min(vps.ph);
        painter.present_pixels(pixel_frame, || {
            layer.set_viewport(src_w, src_h, vps.lw, vps.lh);
        })?;
        Ok(Committed::issued())
    }

    fn present_shm(
        &mut self,
        layer: &LayerSurface,
        vps: ViewportState,
        p: &ShmPayload,
    ) -> Result<Committed, PresentError> {
        let (width, height) = (p.width, p.height);
        let Backend::Shm { shadow } = &mut self.backend else {
            return Err(PresentError::ShmAlloc);
        };
        compose_shm_shadow(shadow, p);
        let Some(pool) = self.shm_pool.as_mut() else {
            return Err(PresentError::ShmAlloc);
        };
        let Some(buf) = draw_from_pixels(pool, &shadow.pixels, width, height) else {
            return Err(PresentError::ShmAlloc);
        };
        layer.present(FrameCommit::new(
            FrameBuffer::Shm(&buf),
            width,
            height,
            width.min(vps.pw),
            height.min(vps.ph),
            vps.lw,
            vps.lh,
        ));
        self.set_current(Some(AttachedBuffer::Shm(buf)));
        Ok(Committed::issued())
    }

    fn present_dmabuf(
        &mut self,
        layer: &LayerSurface,
        vps: ViewportState,
        frame: &SharedTexture,
    ) -> Result<Committed, PresentError> {
        let (vw, vh) = (frame.visible().w, frame.visible().h);
        let Some(pos) = self.lease_dmabuf(frame) else {
            return Err(PresentError::DmabufCreate);
        };
        let (cw, ch) = (frame.coded().w, frame.coded().h);
        match pos {
            DmabufLease::Pooled => {
                layer.present(FrameCommit::new(
                    FrameBuffer::Dmabuf(&self.dmabuf_pool[0].buf),
                    cw,
                    ch,
                    vw,
                    vh,
                    vps.lw,
                    vps.lh,
                ));
                self.set_current(None);
            }
            DmabufLease::OneShot(buf) => {
                layer.present(FrameCommit::new(
                    FrameBuffer::Dmabuf(&buf),
                    cw,
                    ch,
                    vw,
                    vh,
                    vps.lw,
                    vps.lh,
                ));
                self.set_current(Some(AttachedBuffer::Dmabuf(buf)));
            }
        }
        Ok(Committed::issued())
    }

    fn lease_dmabuf(&mut self, frame: &SharedTexture) -> Option<DmabufLease> {
        let dmabuf = self.dmabuf.as_ref()?;
        let plane = frame.planes().first()?;
        let Some(id) = dmabuf_pool_key(frame) else {
            let buf = create_dmabuf_buffer(
                self.rt.buffers(),
                dmabuf,
                &self.qh,
                DmabufPlane {
                    fd: plane.fd.as_fd(),
                    stride: plane.stride,
                    modifier: frame.modifier(),
                    w: frame.coded().w,
                    h: frame.coded().h,
                },
            )?;
            return Some(DmabufLease::OneShot(buf));
        };

        let hit = self.dmabuf_pool.iter().position(|e| {
            e.id == id
                && e.w == frame.coded().w
                && e.h == frame.coded().h
                && e.stride == plane.stride
                && e.modifier == frame.modifier()
        });
        if let Some(pos) = hit {
            if self.rt.buffers().is_idle(&self.dmabuf_pool[pos].buf) {
                let entry = self.dmabuf_pool.remove(pos);
                self.dmabuf_pool.insert(0, entry);
                return Some(DmabufLease::Pooled);
            }
            self.dmabuf_pool.remove(pos);
        }
        if let Some(stale) = self.dmabuf_pool.iter().position(|e| e.id == id) {
            self.dmabuf_pool.remove(stale);
        }

        let buf = create_dmabuf_buffer(
            self.rt.buffers(),
            dmabuf,
            &self.qh,
            DmabufPlane {
                fd: plane.fd.as_fd(),
                stride: plane.stride,
                modifier: frame.modifier(),
                w: frame.coded().w,
                h: frame.coded().h,
            },
        )?;
        self.dmabuf_pool.insert(
            0,
            DmabufBuf {
                id,
                w: frame.coded().w,
                h: frame.coded().h,
                stride: plane.stride,
                modifier: frame.modifier(),
                buf,
            },
        );
        self.dmabuf_pool.truncate(DMABUF_POOL_CAP);
        Some(DmabufLease::Pooled)
    }

    fn shutdown(mut self) {
        self.set_current(None);
        self.dmabuf_pool.clear();
        if let Backend::Gpu {
            painter: Some(painter),
        } = self.backend
        {
            drop(painter);
        }
    }
}

enum DmabufLease {
    Pooled,
    OneShot(DmabufBuffer),
}

fn compose_shm_shadow(shadow: &mut ShmShadow, payload: &ShmPayload) {
    let (width, height) = (payload.width, payload.height);
    if shadow.size != (width, height) {
        let stride = (width.max(0) as usize).saturating_mul(4);
        let size = (height.max(0) as usize).saturating_mul(stride);
        shadow.pixels.clear();
        shadow.pixels.resize(size, 0);
        shadow.size = (width, height);
    }
    if let Some(full_pixels) = payload.full_pixels.as_deref()
        && let Some(dst) = shadow.pixels.get_mut(..full_pixels.len())
    {
        dst.copy_from_slice(full_pixels);
    }
    apply_dirty_to_shadow(&mut shadow.pixels, width, &payload.rects);
}

fn copy_dirty_rect(
    pixels: &[u8],
    src_stride: usize,
    width: i32,
    height: i32,
    rect: &JfnRect,
) -> Option<ShmRect> {
    let JfnRect {
        x: rx,
        y: ry,
        w: rw,
        h: rh,
    } = rect.clamped(width, height)?;

    let (Ok(rw_us), Ok(rx_us)) = (usize::try_from(rw), usize::try_from(rx)) else {
        return None;
    };
    let (Some(row_bytes), Some(rx_bytes)) = (rw_us.checked_mul(4), rx_us.checked_mul(4)) else {
        return None;
    };
    let mut out = Vec::with_capacity(row_bytes.saturating_mul(usize::try_from(rh).unwrap_or(0)));
    for row in ry..(ry + rh) {
        let Ok(row_us) = usize::try_from(row) else {
            continue;
        };
        let Some(off) = row_us
            .checked_mul(src_stride)
            .and_then(|v| v.checked_add(rx_bytes))
        else {
            continue;
        };
        let Some(end) = off.checked_add(row_bytes) else {
            continue;
        };
        let Some(slice) = pixels.get(off..end) else {
            continue;
        };
        out.extend_from_slice(slice);
    }

    Some(ShmRect {
        x: rx,
        y: ry,
        w: rw,
        h: rh,
        pixels: out,
    })
}

fn apply_dirty_to_shadow(shadow: &mut [u8], width: i32, rects: &[ShmRect]) {
    let Ok(width_us) = usize::try_from(width) else {
        return;
    };
    let Some(dst_stride) = width_us.checked_mul(4) else {
        return;
    };
    for rect in rects {
        let (Ok(rw_us), Ok(rx_us)) = (usize::try_from(rect.w), usize::try_from(rect.x)) else {
            continue;
        };
        let (Some(row_bytes), Some(rx_bytes)) = (rw_us.checked_mul(4), rx_us.checked_mul(4)) else {
            continue;
        };
        for row in 0..rect.h {
            let Ok(row_us) = usize::try_from(row) else {
                continue;
            };
            let Some(dst_row) = rect
                .y
                .checked_add(row)
                .and_then(|y| usize::try_from(y).ok())
            else {
                continue;
            };
            let Some(src_off) = row_us.checked_mul(row_bytes) else {
                continue;
            };
            let Some(dst_off) = dst_row
                .checked_mul(dst_stride)
                .and_then(|v| v.checked_add(rx_bytes))
            else {
                continue;
            };
            let (Some(src_end), Some(dst_end)) = (
                src_off.checked_add(row_bytes),
                dst_off.checked_add(row_bytes),
            ) else {
                continue;
            };
            let (Some(src), Some(dst)) = (
                rect.pixels.get(src_off..src_end),
                shadow.get_mut(dst_off..dst_end),
            ) else {
                continue;
            };
            dst.copy_from_slice(src);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vp() -> ViewportState {
        ViewportState {
            lw: 100,
            lh: 100,
            pw: 100,
            ph: 100,
        }
    }

    fn rect(x: i32, y: i32, w: i32, h: i32) -> JfnRect {
        JfnRect { x, y, w, h }
    }

    #[test]
    fn coalesced_hide_then_show_frame_presents() {
        assert_eq!(
            decide(Some(7u32), Visibility::Shown, false),
            Action::Present(7)
        );
    }

    #[test]
    fn hide_alone_hides() {
        assert_eq!(decide(None::<u32>, Visibility::Hidden, false), Action::Hide);
    }

    #[test]
    fn hide_outranks_a_pending_frame() {
        assert_eq!(decide(Some(7u32), Visibility::Hidden, true), Action::Hide);
    }

    #[test]
    fn viewport_dirty_snapshot_reapplies_viewport() {
        assert_eq!(
            decide(None::<u32>, Visibility::Shown, true),
            Action::ReapplyViewport
        );
    }

    #[test]
    fn hide_request_drops_the_pending_frame_and_stales_the_shadow() {
        let mut mb = LayerState::new(vp());
        mb.shadow = valid_shadow();
        mb.store_shm(vec![test_rect(1)], None, 100, 100);
        assert_eq!(mb.request_visibility(Visibility::Hidden), 1);
        assert!(mb.pending.is_none());
        assert!(matches!(mb.shadow, ShadowState::Stale));
        assert_eq!(mb.requested, Some(Visibility::Hidden));
    }

    #[test]
    fn each_visibility_request_gets_its_own_number() {
        let mut mb = LayerState::new(vp());
        assert_eq!(mb.request_visibility(Visibility::Shown), 1);
        assert_eq!(mb.request_visibility(Visibility::Hidden), 2);
    }

    #[test]
    fn store_shm_merges_dirty_only_same_dims() {
        let mut mb = LayerState::new(vp());
        mb.store_shm(vec![test_rect(1)], None, 100, 100);
        mb.store_shm(vec![test_rect(2)], None, 100, 100);
        let Some(PendingFrame::Shm(p)) = &mb.pending else {
            panic!("expected shm payload");
        };
        assert_eq!(p.rects.len(), 2);
    }

    #[test]
    fn store_shm_replaces_on_dim_mismatch() {
        let mut mb = LayerState::new(vp());
        mb.store_shm(vec![test_rect(1)], None, 100, 100);
        mb.store_shm(vec![test_rect(2)], None, 200, 200);
        let Some(PendingFrame::Shm(p)) = &mb.pending else {
            panic!("expected shm payload");
        };
        assert_eq!(p.rects.len(), 1);
        assert_eq!((p.width, p.height), (200, 200));
    }

    #[test]
    fn store_shm_replaces_when_pending_has_full() {
        let mut mb = LayerState::new(vp());
        mb.store_shm(vec![], Some(vec![0u8; 4]), 1, 1);
        mb.store_shm(vec![test_rect(2)], None, 1, 1);
        let Some(PendingFrame::Shm(p)) = &mb.pending else {
            panic!("expected shm payload");
        };
        assert!(p.full_pixels.is_none());
        assert_eq!(p.rects.len(), 1);
    }

    fn test_rect(tag: u8) -> ShmRect {
        ShmRect {
            x: 0,
            y: 0,
            w: 1,
            h: 1,
            pixels: vec![tag; 4],
        }
    }

    #[test]
    fn full_copy_marks_shadow_valid() {
        let mut mb = LayerState::new(vp());
        assert!(matches!(mb.shadow, ShadowState::Stale));
        mb.store_shm(vec![], Some(vec![0u8; 4]), 1, 1);
        assert!(matches!(mb.shadow, ShadowState::Valid { size: (1, 1) }));
    }

    #[test]
    fn valid_shadow_at_wrong_size_still_full_copies() {
        let mut mb = LayerState::new(vp());
        mb.store_shm(vec![], Some(vec![0u8; 4 * 100 * 100]), 100, 100);
        mb.pending = None; // worker consumed the full frame
        assert!(!mb.needs_full_copy(100, 100));
        assert!(mb.needs_full_copy(200, 200));
    }

    #[test]
    fn hide_invalidates_shadow_forcing_full_copy() {
        let mut mb = LayerState::new(vp());
        mb.store_shm(vec![], Some(vec![0u8; 4 * 100 * 100]), 100, 100);
        assert!(matches!(mb.shadow, ShadowState::Valid { .. }));
        mb.pending = None; // worker consumed the full frame
        assert!(!mb.needs_full_copy(100, 100));
        mb.request_visibility(Visibility::Hidden);
        assert!(matches!(mb.shadow, ShadowState::Stale));
        assert!(mb.needs_full_copy(100, 100));
    }

    fn dmabuf_frame(coded_w: i32, coded_h: i32) -> SharedTexture {
        let fd = std::fs::File::open("/dev/null")
            .expect("open /dev/null")
            .into();
        let size = PhysicalSize {
            w: coded_w,
            h: coded_h,
        };
        SharedTexture::new(
            size,
            size,
            jfn_gpu_paint::DmabufFormat::Bgra8,
            0,
            vec![jfn_gpu_paint::DmabufPlane {
                fd,
                offset: 0,
                stride: 0,
            }],
        )
    }

    fn valid_shadow() -> ShadowState {
        ShadowState::Valid { size: (100, 100) }
    }

    #[test]
    fn resize_noop_when_unchanged() {
        let mut mb = LayerState::new(vp());
        mb.shadow = valid_shadow();
        mb.resize(vp());
        assert!(!mb.viewport_dirty);
        assert!(matches!(mb.shadow, ShadowState::Valid { .. }));

        mb.resize(ViewportState {
            lw: 200,
            lh: 200,
            pw: 200,
            ph: 200,
        });
        assert!(mb.viewport_dirty);
        assert!(matches!(mb.shadow, ShadowState::Stale));
    }

    #[test]
    fn dmabuf_hide_and_resize_invalidate_shadow() {
        let mut mb = LayerState::new(vp());
        mb.shadow = valid_shadow();
        mb.present_dmabuf(dmabuf_frame(64, 64));
        assert!(matches!(mb.shadow, ShadowState::Stale));

        mb.shadow = valid_shadow();
        mb.request_visibility(Visibility::Hidden);
        assert!(matches!(mb.shadow, ShadowState::Stale));

        let mut mb = LayerState::new(vp());
        mb.shadow = valid_shadow();
        mb.resize(ViewportState {
            lw: 200,
            lh: 200,
            pw: 200,
            ph: 200,
        });
        assert!(matches!(mb.shadow, ShadowState::Stale));
    }

    #[test]
    fn route_software_falls_back_after_gpu_failure() {
        assert_eq!(route_software(Kind::Gpu, false), Route::Gpu);
        assert_eq!(route_software(Kind::Gpu, true), Route::Shm);
        assert_eq!(route_software(Kind::Shm, false), Route::Shm);
        assert_eq!(route_software(Kind::Shm, true), Route::Shm);
    }

    #[test]
    fn gpu_error_degrades_backend() {
        assert!(!PresentError::ShmAlloc.is_degrading());
        assert!(!PresentError::DmabufCreate.is_degrading());

        let mut backend = Backend::Gpu { painter: None };
        let flag = AtomicBool::new(false);
        let old = degrade(&mut backend, &flag);
        assert!(old.is_none());
        assert!(matches!(backend, Backend::Shm { .. }));
        assert!(flag.load(Ordering::Acquire));
    }

    #[test]
    fn first_post_fallback_frame_full_copies() {
        let mb = LayerState::new(vp());
        assert!(mb.needs_full_copy(100, 100));
    }

    #[test]
    fn copy_dirty_rect_clamps_negative_origin() {
        let pixels = vec![0xABu8; 4 * 4 * 4];
        let r = copy_dirty_rect(&pixels, 16, 4, 4, &rect(-2, -2, 4, 4)).unwrap();
        assert_eq!((r.x, r.y, r.w, r.h), (0, 0, 2, 2));
        assert_eq!(r.pixels.len(), 2 * 2 * 4);
    }

    #[test]
    fn copy_dirty_rect_clamps_overflow() {
        let pixels = vec![0u8; 4 * 4 * 4];
        let r = copy_dirty_rect(&pixels, 16, 4, 4, &rect(2, 2, 10, 10)).unwrap();
        assert_eq!((r.w, r.h), (2, 2));
    }

    #[test]
    fn copy_dirty_rect_rejects_zero_area() {
        let pixels = vec![0u8; 4 * 4 * 4];
        assert!(copy_dirty_rect(&pixels, 16, 4, 4, &rect(0, 0, 0, 5)).is_none());
        assert!(copy_dirty_rect(&pixels, 16, 4, 4, &rect(4, 0, 4, 4)).is_none());
    }

    #[test]
    fn copy_dirty_rect_skips_row_past_buffer() {
        // A stride that lies about the buffer length pushes later rows out of
        // range; `get` skips them instead of panicking.
        let pixels = vec![0u8; 8];
        let r = copy_dirty_rect(&pixels, 1_000, 2, 2, &rect(0, 0, 2, 2)).unwrap();
        // First row fits (off 0..8); the second (off 1000..) is out of range.
        assert_eq!(r.pixels.len(), 8);
    }

    #[test]
    fn copy_dirty_rect_extreme_extent_does_not_panic() {
        let pixels = vec![0u8; 4 * 4 * 4];
        let r = copy_dirty_rect(&pixels, 16, 4, 4, &rect(3, 3, i32::MAX, i32::MAX)).unwrap();
        assert_eq!((r.w, r.h), (1, 1));
        assert!(copy_dirty_rect(&pixels, 16, 4, 4, &rect(i32::MAX, 0, i32::MAX, 4)).is_none());
    }

    #[test]
    fn apply_dirty_to_shadow_extreme_offsets_do_not_panic() {
        let mut shadow = vec![0u8; 4 * 2 * 2];
        let rects = vec![ShmRect {
            x: i32::MAX,
            y: i32::MAX,
            w: i32::MAX,
            h: 1,
            pixels: vec![0xFF; 4],
        }];
        apply_dirty_to_shadow(&mut shadow, 2, &rects);
        assert_eq!(shadow, vec![0u8; 16]);
    }

    #[test]
    fn apply_dirty_to_shadow_writes_offsets() {
        let mut shadow = vec![0u8; 4 * 2 * 2];
        let rects = vec![ShmRect {
            x: 1,
            y: 1,
            w: 1,
            h: 1,
            pixels: vec![0xFF; 4],
        }];
        apply_dirty_to_shadow(&mut shadow, 2, &rects);
        assert_eq!(&shadow[12..16], &[0xFF; 4]);
        assert_eq!(&shadow[0..12], &[0u8; 12]);
    }

    #[test]
    fn apply_dirty_to_shadow_skips_rect_exceeding_shadow() {
        let mut shadow = vec![0u8; 4 * 2 * 2];
        let rects = vec![ShmRect {
            x: 5,
            y: 5,
            w: 1,
            h: 1,
            pixels: vec![0xFF; 4],
        }];
        apply_dirty_to_shadow(&mut shadow, 2, &rects);
        assert_eq!(shadow, vec![0u8; 16]);
    }

    #[test]
    fn compose_shm_shadow_full_copy_is_byte_exact() {
        assert_eq!(route_software(Kind::Gpu, true), Route::Shm);

        let (w, h) = (3, 2);
        let source: Vec<u8> = (0..u8::try_from(w * h * 4).unwrap()).collect();
        let payload = ShmPayload {
            rects: Vec::new(),
            full_pixels: Some(source.clone()),
            width: w,
            height: h,
        };
        let mut shadow = ShmShadow::default();
        compose_shm_shadow(&mut shadow, &payload);
        assert_eq!(shadow.pixels, source);
        assert_eq!(shadow.size, (w, h));
    }
}
