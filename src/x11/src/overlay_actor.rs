//! One content actor per overlay surface: the sole owner of pixel upload.
//!
//! One thread + mailbox (mirroring `jfn_wayland::layer_actor`). It holds a
//! [`ContentSurface`] and so CANNOT configure geometry — the geometry thread is
//! the sole structure writer. Degradation (GPU present failure → SHM) happens
//! INSIDE the actor; there is no CEF-thread fallback.
//!
//! The content surface is attached after the geometry thread creates the
//! window ([`OverlayActor::attach_content`]); frames that arrive before then
//! are dropped (the surface has nowhere to land yet).

use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::{self, JoinHandle};

use jfn_gpu_paint::{
    DirtyRect, DmabufFrame, GpuPainter, PixelFrame, PresentOutcome, SizePolicy, WindowTarget,
};
use x11rb::connection::Connection;
use x11rb::protocol::shm::ConnectionExt as _;
use x11rb::protocol::xproto;
use x11rb::rust_connection::RustConnection;

use crate::registry::ContentSurface;
use crate::shm::{shm_alloc, shm_free};
use crate::x11_state::ShmBuffer;

enum PendingFrame {
    Pixels {
        pixels: Vec<u8>,
        dirty: Vec<DirtyRect>,
        width: i32,
        height: i32,
        stride: usize,
    },
    Dmabuf(Box<DmabufFrame>),
}

struct Mailbox {
    pending: Option<PendingFrame>,
    /// Handed over once the geometry thread has created the window.
    content: Option<ContentSurface>,
    /// Desired swapchain target extent (parent-derived); the geometry thread is
    /// the authority for it.
    target_size: (u32, u32),
    visible: bool,
    shutdown: bool,
}

/// X11 content presenter for one overlay. See the module docs.
pub(crate) struct OverlayActor {
    shared: Arc<(Mutex<Mailbox>, Condvar)>,
    thread: Option<JoinHandle<()>>,
}

impl OverlayActor {
    pub(crate) fn new(visible: bool) -> Self {
        let shared = Arc::new((
            Mutex::new(Mailbox {
                pending: None,
                content: None,
                target_size: (1, 1),
                visible,
                shutdown: false,
            }),
            Condvar::new(),
        ));
        let worker_shared = Arc::clone(&shared);
        let thread = thread::Builder::new()
            .name("jfn-x11-overlay".into())
            .spawn(move || run_worker(worker_shared))
            .ok();
        Self { shared, thread }
    }

    fn with_state(&self, f: impl FnOnce(&mut Mailbox)) {
        let (lock, cv) = &*self.shared;
        let mut state = lock.lock().unwrap_or_else(PoisonError::into_inner);
        f(&mut state);
        cv.notify_one();
    }

    /// Hand the freshly-created window's content capability to the actor.
    pub(crate) fn attach_content(&self, content: ContentSurface) {
        self.with_state(|s| s.content = Some(content));
    }

    /// Desired swapchain target extent, set by the geometry thread in lockstep
    /// with the overlay window size.
    pub(crate) fn resize(&self, w: i32, h: i32) {
        if w <= 0 || h <= 0 {
            return;
        }
        self.with_state(|s| s.target_size = (w as u32, h as u32));
    }

    pub(crate) fn set_visible(&self, visible: bool) {
        self.with_state(|s| {
            s.visible = visible;
            if !visible {
                s.pending = None;
            }
        });
    }

    pub(crate) fn present_software(
        &self,
        dirty: &[DirtyRect],
        pixels: &[u8],
        width: i32,
        height: i32,
    ) -> bool {
        if width <= 0 || height <= 0 {
            return false;
        }
        let stride = (width as usize).saturating_mul(4);
        let Some(len) = (height as usize).checked_mul(stride) else {
            return false;
        };
        if pixels.len() < len {
            return false;
        }
        self.with_state(|s| {
            if !s.visible {
                return;
            }
            s.pending = Some(PendingFrame::Pixels {
                pixels: pixels[..len].to_vec(),
                dirty: dirty.to_vec(),
                width,
                height,
                stride,
            });
        });
        true
    }

    pub(crate) fn present_dmabuf(&self, frame: DmabufFrame) -> bool {
        self.with_state(|s| {
            if s.visible {
                s.pending = Some(PendingFrame::Dmabuf(Box::new(frame)));
            }
        });
        true
    }

    /// Deterministic teardown: signal shutdown and join the worker, which frees
    /// the content GC + SHM segments + GPU resources on its own thread.
    pub(crate) fn shutdown(mut self) {
        self.signal_shutdown();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }

    fn signal_shutdown(&self) {
        self.with_state(|s| {
            s.shutdown = true;
            s.pending = None;
        });
    }
}

impl Drop for OverlayActor {
    fn drop(&mut self) {
        // Safety net for a dropped-without-shutdown actor.
        self.signal_shutdown();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

// ===================================================================
// Worker
// ===================================================================

#[derive(Default)]
struct ShmState {
    bufs: [ShmBuffer; 2],
    idx: usize,
}

enum Backend {
    Gpu(Option<Box<GpuPainter>>),
    Shm(ShmState),
}

fn initial_backend() -> Backend {
    let gpu_available = crate::x11_state::paint().is_some_and(|p| p.gpu_caps.gpu_available);
    if gpu_available {
        Backend::Gpu(None)
    } else {
        Backend::Shm(ShmState::default())
    }
}

fn run_worker(shared: Arc<(Mutex<Mailbox>, Condvar)>) {
    let mut backend = initial_backend();
    let content_conn = crate::x11_state::x11rb_conn();

    loop {
        let (frame, content_window, content_gc, visible, target_size, shutdown) = {
            let (lock, cv) = &*shared;
            let mut state = lock.lock().unwrap_or_else(PoisonError::into_inner);
            while state.pending.is_none() && !state.shutdown {
                state = cv.wait(state).unwrap_or_else(PoisonError::into_inner);
            }
            let (win, gc) = state
                .content
                .as_ref()
                .map_or((None, None), |c| (Some(c.window()), Some(c.gc())));
            (
                state.pending.take(),
                win,
                gc,
                state.visible,
                state.target_size,
                state.shutdown,
            )
        };

        if shutdown {
            break;
        }
        let (Some(window), Some(gc)) = (content_window, content_gc) else {
            // No window yet: nothing can be presented.
            continue;
        };
        let Some(frame) = frame else {
            continue;
        };
        if !visible {
            continue;
        }

        present_frame(
            &mut backend,
            content_conn.as_deref(),
            window,
            gc,
            target_size,
            frame,
        );
    }

    teardown(backend, content_conn.as_deref(), &shared);
}

fn present_frame(
    backend: &mut Backend,
    content_conn: Option<&RustConnection>,
    window: u32,
    gc: u32,
    target_size: (u32, u32),
    frame: PendingFrame,
) {
    match backend {
        Backend::Gpu(painter) => {
            if present_gpu(painter, window, target_size, frame) {
                return;
            }
            // GPU failed on a pixel frame: degrade to SHM. (A dmabuf frame that
            // fails is dropped by present_gpu without signalling degrade, since
            // dmabuf has no CPU fallback — so we only reach here for pixels.)
            if let Backend::Gpu(p) = backend
                && let Some(p) = p.take()
            {
                p.shutdown();
            }
            *backend = Backend::Shm(ShmState::default());
        }
        Backend::Shm(state) => {
            if let Some(conn) = content_conn {
                present_shm(state, conn, window, gc, frame);
            }
        }
    }
}

/// Present through the GPU painter. Returns `false` only when a PIXEL frame
/// failed and the caller should degrade to SHM; a failed dmabuf frame returns
/// `true` (logged, dropped) because dmabuf has no CPU fallback.
fn present_gpu(
    painter: &mut Option<Box<GpuPainter>>,
    window: u32,
    target_size: (u32, u32),
    frame: PendingFrame,
) -> bool {
    let is_dmabuf = matches!(frame, PendingFrame::Dmabuf(_));

    if painter.is_none() {
        let (Some(conn_ptr), Some(paint)) = (
            crate::x11_state::raw_xcb_connection(),
            crate::x11_state::paint(),
        ) else {
            return is_dmabuf;
        };
        let Some(ctx) = paint.gpu_ctx.clone() else {
            return is_dmabuf;
        };
        let target = WindowTarget::Xcb {
            connection: conn_ptr,
            window,
            screen: crate::x11_state::host().map_or(0, |h| h.screen_num),
            visual: paint.argb_visual,
        };
        // FollowTarget: the geometry thread sizes the overlay window; the
        // painter drives its swapchain from that parent-derived extent
        // (fed via resize), never from the CEF frame size or the window. Seed
        // with the target extent so the first configure already matches.
        let init = (target_size.0.max(1), target_size.1.max(1));
        match GpuPainter::with_policy(ctx, target, init, SizePolicy::FollowTarget) {
            Ok(p) => *painter = Some(Box::new(p)),
            Err(e) => {
                eprintln!("[x11] overlay actor gpu init failed: {e}; using SHM");
                return is_dmabuf;
            }
        }
    }
    let Some(painter) = painter.as_mut() else {
        return is_dmabuf;
    };
    painter.set_visible(true);
    painter.resize(target_size);

    match frame {
        PendingFrame::Pixels {
            pixels,
            dirty,
            width,
            height,
            stride,
        } => {
            let pf = PixelFrame {
                width: width as u32,
                height: height as u32,
                stride: stride as u32,
                bgra: &pixels,
                dirty: &dirty,
            };
            match painter.push_pixels(pf, || {}) {
                Ok(PresentOutcome::Presented) => true,
                Ok(PresentOutcome::Skipped) => {
                    tracing::debug!("[x11] overlay actor gpu frame skipped (surface unavailable)");
                    true
                }
                Err(e) => {
                    eprintln!("[x11] overlay actor push_pixels failed: {e}; using SHM");
                    false
                }
            }
        }
        PendingFrame::Dmabuf(frame) => {
            if let Err(e) = painter.push_dmabuf(*frame) {
                tracing::warn!("[x11] overlay actor push_dmabuf failed: {e}");
            }
            true
        }
    }
}

fn present_shm(
    state: &mut ShmState,
    conn: &RustConnection,
    window: u32,
    gc: u32,
    frame: PendingFrame,
) {
    let PendingFrame::Pixels {
        pixels,
        dirty,
        width,
        height,
        stride,
    } = frame
    else {
        // dmabuf frames never reach the SHM backend.
        return;
    };
    let depth = crate::x11_state::paint().map_or(32, |p| p.argb_depth);
    let buf = &mut state.bufs[state.idx];
    if !shm_alloc(buf, conn, width, height) {
        eprintln!("[x11] overlay actor shm allocation failed");
        return;
    }
    let dst_stride = (width as usize) * 4;
    for rect in &dirty {
        let Some((rx, ry, rw, rh)) = clip_rect(rect, width, height) else {
            continue;
        };
        for row in 0..rh {
            let src_off = ((ry + row) as usize) * stride + (rx as usize) * 4;
            let dst_off = ((ry + row) as usize) * dst_stride + (rx as usize) * 4;
            let row_bytes = (rw as usize) * 4;
            let (Some(src), true) = (
                pixels.get(src_off..src_off + row_bytes),
                dst_off + row_bytes <= buf.size,
            ) else {
                continue;
            };
            unsafe {
                std::ptr::copy_nonoverlapping(src.as_ptr(), buf.data.add(dst_off), row_bytes);
            }
        }
        let _ = conn.shm_put_image(
            window,
            gc,
            width as u16,
            height as u16,
            rx as u16,
            ry as u16,
            rw as u16,
            rh as u16,
            rx as i16,
            ry as i16,
            depth,
            u8::from(xproto::ImageFormat::Z_PIXMAP),
            false,
            buf.seg,
            0,
        );
    }
    state.idx ^= 1;
    let _ = conn.flush();
}

fn clip_rect(rect: &DirtyRect, width: i32, height: i32) -> Option<(i32, i32, i32, i32)> {
    let mut rx = rect.x;
    let mut ry = rect.y;
    let mut rw = rect.w;
    let mut rh = rect.h;
    if rx < 0 {
        rw += rx;
        rx = 0;
    }
    if ry < 0 {
        rh += ry;
        ry = 0;
    }
    if rx + rw > width {
        rw = width - rx;
    }
    if ry + rh > height {
        rh = height - ry;
    }
    if rw <= 0 || rh <= 0 {
        return None;
    }
    Some((rx, ry, rw, rh))
}

fn teardown(
    backend: Backend,
    content_conn: Option<&RustConnection>,
    shared: &Arc<(Mutex<Mailbox>, Condvar)>,
) {
    match backend {
        Backend::Gpu(Some(painter)) => painter.shutdown(),
        Backend::Gpu(None) => {}
        Backend::Shm(mut state) => {
            for buf in &mut state.bufs {
                shm_free(buf, content_conn);
            }
        }
    }
    // Free the content GC on the content connection.
    if let Some(conn) = content_conn {
        let (lock, _) = &**shared;
        let state = lock.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(content) = state.content.as_ref() {
            content.free_gc(conn);
        }
        let _ = conn.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, w: i32, h: i32) -> DirtyRect {
        DirtyRect { x, y, w, h }
    }

    #[test]
    fn clip_rect_clamps_negative_origin() {
        assert_eq!(clip_rect(&rect(-2, -2, 4, 4), 10, 10), Some((0, 0, 2, 2)));
    }

    #[test]
    fn clip_rect_clamps_overflow() {
        assert_eq!(clip_rect(&rect(8, 8, 10, 10), 10, 10), Some((8, 8, 2, 2)));
    }

    #[test]
    fn clip_rect_rejects_zero_and_off_screen() {
        assert_eq!(clip_rect(&rect(0, 0, 0, 5), 10, 10), None);
        assert_eq!(clip_rect(&rect(10, 0, 4, 4), 10, 10), None);
    }

    #[test]
    fn clip_rect_passes_through_in_bounds() {
        assert_eq!(clip_rect(&rect(1, 2, 3, 4), 10, 10), Some((1, 2, 3, 4)));
    }
}
