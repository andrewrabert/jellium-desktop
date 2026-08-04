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

use std::thread::{self, JoinHandle};

use jfn_gpu_paint::{
    Frame, FrameSize as PhysicalSize, Pixels, Presented, SharedTexture, WindowTarget,
};
use jfn_mailbox::Mailbox;
use jfn_platform_abi::JfnRect;
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
        dirty: Vec<JfnRect>,
        width: i32,
        height: i32,
        stride: usize,
    },
    Shared(Box<SharedTexture>),
}

struct OverlayState {
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
    mailbox: Mailbox<OverlayState>,
    thread: Option<JoinHandle<()>>,
}

impl OverlayActor {
    pub(crate) fn new(visible: bool) -> Self {
        let mailbox = Mailbox::new(OverlayState {
            pending: None,
            content: None,
            target_size: (1, 1),
            visible,
            shutdown: false,
        });
        let worker_mailbox = mailbox.clone();
        let thread = thread::Builder::new()
            .name("jfn-x11-overlay".into())
            .spawn(move || run_worker(worker_mailbox))
            .ok();
        Self { mailbox, thread }
    }

    /// Hand the freshly-created window's content capability to the actor.
    pub(crate) fn attach_content(&self, content: ContentSurface) {
        self.mailbox.update(|s| s.content = Some(content));
    }

    /// Desired swapchain target extent, set by the geometry thread in lockstep
    /// with the overlay window size.
    pub(crate) fn resize(&self, w: i32, h: i32) {
        if w <= 0 || h <= 0 {
            return;
        }
        self.mailbox
            .update(|s| s.target_size = (w as u32, h as u32));
    }

    pub(crate) fn set_visible(&self, visible: bool) {
        self.mailbox.update(|s| {
            s.visible = visible;
            if !visible {
                s.pending = None;
            }
        });
    }

    pub(crate) fn present_software(
        &self,
        dirty: &[JfnRect],
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
        self.mailbox.update(|s| {
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

    pub(crate) fn present_shared(&self, frame: SharedTexture) -> bool {
        self.mailbox.update(|s| {
            if s.visible {
                s.pending = Some(PendingFrame::Shared(Box::new(frame)));
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
        self.mailbox.update(|s| {
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
    Gpu(Option<Box<jfn_gpu_paint::Surface<'static>>>),
    Shm(ShmState),
}

fn initial_backend() -> Backend {
    if crate::paint::gpu().is_some() {
        Backend::Gpu(None)
    } else {
        Backend::Shm(ShmState::default())
    }
}

fn run_worker(mailbox: Mailbox<OverlayState>) {
    let mut backend = initial_backend();
    let content_conn = crate::x11_state::x11rb_conn();

    loop {
        let (frame, content_window, content_gc, visible, target_size, shutdown) = mailbox.wait(
            |s| s.pending.is_some() || s.shutdown,
            |s| {
                let (win, gc) = s
                    .content
                    .as_ref()
                    .map_or((None, None), |c| (Some(c.window()), Some(c.gc())));
                (
                    s.pending.take(),
                    win,
                    gc,
                    s.visible,
                    s.target_size,
                    s.shutdown,
                )
            },
        );

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

    teardown(backend, content_conn.as_deref(), &mailbox);
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
            // A fatal GPU failure: degrade to SHM. Take the painter out and
            // shut it down BEFORE switching — wgpu's swapchain and hand-rolled
            // SHM must never both be writing this window.
            if let Backend::Gpu(p) = backend
                && let Some(p) = p.take()
            {
                drop(p);
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

/// Present through the GPU surface. Returns `false` only when the failure was
/// fatal and the caller should degrade to SHM. A failed shared frame is never
/// fatal — dmabuf has no CPU fallback, so degrading would strand the surface
/// with no output at all; the frame is logged and dropped instead.
fn present_gpu(
    painter: &mut Option<Box<jfn_gpu_paint::Surface<'static>>>,
    window: u32,
    target_size: (u32, u32),
    frame: PendingFrame,
) -> bool {
    if painter.is_none() {
        let (Some(conn_ptr), Some(paint), Some(gpu)) = (
            crate::x11_state::raw_xcb_connection(),
            crate::x11_state::paint(),
            crate::paint::gpu(),
        ) else {
            return true;
        };
        let target = WindowTarget::Xcb {
            connection: conn_ptr,
            window,
            screen: crate::x11_state::host().map_or(0, |h| h.screen_num),
            visual: paint.argb_visual,
        };
        // Seed with the parent-derived target extent so the first configure
        // already matches the window the geometry thread sized.
        let init = PhysicalSize {
            w: target_size.0.max(1) as i32,
            h: target_size.1.max(1) as i32,
        };
        match gpu.new_surface(target, init) {
            Ok(p) => *painter = Some(Box::new(p)),
            Err(e) => {
                // Degrading a shared frame strands the surface: SHM cannot
                // present it, so it would be dropped here and so would every
                // frame after it. Stay on GPU and retry creation next frame.
                if matches!(frame, PendingFrame::Shared(_)) {
                    tracing::warn!("[x11] overlay actor gpu init failed: {e}; dropping frame");
                    return true;
                }
                eprintln!("[x11] overlay actor gpu init failed: {e}; using SHM");
                return false;
            }
        }
    }
    let Some(painter) = painter.as_mut() else {
        return true;
    };
    painter.set_visible(true);
    painter.resize(PhysicalSize {
        w: target_size.0 as i32,
        h: target_size.1 as i32,
    });

    let outcome = match &frame {
        PendingFrame::Pixels {
            pixels,
            dirty,
            width,
            height,
            stride,
        } => painter.present(
            Frame::Copied(Pixels {
                size: PhysicalSize {
                    w: *width,
                    h: *height,
                },
                stride: *stride as u32,
                bgra: pixels,
                dirty,
            }),
            || {},
        ),
        PendingFrame::Shared(tex) => painter.present(Frame::Shared(tex), || {}),
    };

    match outcome {
        Ok(Presented::Yes) => true,
        Ok(Presented::Skipped) => {
            tracing::debug!("[x11] overlay actor frame skipped (surface unavailable)");
            true
        }
        // An `Err` is the surface saying it is done; anything recoverable came
        // back as `Skipped`.
        Err(e) => {
            eprintln!("[x11] overlay actor present failed: {e}; using SHM");
            false
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
        // Shared frames never reach the SHM backend: a shared failure is not
        // fatal, so it never degrades.
        return;
    };
    let depth = crate::x11_state::paint().map_or(32, |p| p.argb_depth);
    let buf = &mut state.bufs[state.idx];
    if !shm_alloc(buf, conn, width, height) {
        eprintln!("[x11] overlay actor shm allocation failed");
        return;
    }
    let seg = buf.seg();
    let dst_stride = (width as usize) * 4;
    let dst = buf.pixels_mut();
    for rect in &dirty {
        let Some((rx, ry, rw, rh)) = clip_rect(rect, width, height) else {
            continue;
        };
        for row in 0..rh {
            let src_off = ((ry + row) as usize) * stride + (rx as usize) * 4;
            let dst_off = ((ry + row) as usize) * dst_stride + (rx as usize) * 4;
            let row_bytes = (rw as usize) * 4;
            let (Some(src), Some(dst_row)) = (
                pixels.get(src_off..src_off + row_bytes),
                dst.get_mut(dst_off..dst_off + row_bytes),
            ) else {
                continue;
            };
            dst_row.copy_from_slice(src);
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
            seg,
            0,
        );
    }
    state.idx ^= 1;
    let _ = conn.flush();
}

fn clip_rect(rect: &JfnRect, width: i32, height: i32) -> Option<(i32, i32, i32, i32)> {
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
    mailbox: &Mailbox<OverlayState>,
) {
    match backend {
        Backend::Gpu(Some(painter)) => drop(painter),
        Backend::Gpu(None) => {}
        Backend::Shm(mut state) => {
            for buf in &mut state.bufs {
                shm_free(buf, content_conn);
            }
        }
    }
    // Free the content GC on the content connection.
    if let Some(conn) = content_conn {
        mailbox.peek(|s| {
            if let Some(content) = s.content.as_ref() {
                content.free_gc(conn);
            }
        });
        let _ = conn.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, w: i32, h: i32) -> JfnRect {
        JfnRect { x, y, w, h }
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
