//! One content actor per overlay surface: the sole owner of pixel upload.
//!
//! One thread + mailbox (mirroring `jfn_wayland::layer_actor`). It holds a
//! [`ContentSurface`] and so CANNOT configure geometry — the geometry thread is
//! the sole structure writer. Degradation (GPU present failure → SHM) happens
//! INSIDE the actor; there is no CEF-thread fallback.
//!
//! The content surface is attached after the geometry thread creates the
//! window ([`OverlayActor::attach_content`]); a frame that arrives before then
//! stays owed until it has.

use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Instant;

use jfn_gpu_paint::{
    FrameSize as PhysicalSize, Pixels, PresentFailed, SharedTexture, WindowTarget,
};
use jfn_mailbox::Mailbox;
use jfn_platform_abi::{FrameRetry, FrameSource, JfnRect};
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
    /// The producer that owes the pending frame's successor, held so a frame
    /// the swapchain could not take can ask for the one that replaces it.
    source: Option<Arc<dyn FrameSource>>,
    /// Handed over once the geometry thread has created the window.
    content: Option<ContentSurface>,
    /// Desired swapchain target extent (parent-derived); the geometry thread is
    /// the authority for it.
    target_size: (u32, u32),
    shutdown: bool,
}

/// X11 content presenter for one overlay. See the module docs.
pub(crate) struct OverlayActor {
    mailbox: Mailbox<OverlayState>,
    thread: Option<JoinHandle<()>>,
}

impl OverlayActor {
    pub(crate) fn new() -> Self {
        let mailbox = Mailbox::new(OverlayState {
            pending: None,
            source: None,
            content: None,
            target_size: (1, 1),
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

    /// `pixels` must cover `width * height` BGRA (the caller checked it).
    pub(crate) fn present_software(
        &self,
        dirty: &[JfnRect],
        pixels: &[u8],
        width: i32,
        height: i32,
        source: Arc<dyn FrameSource>,
    ) {
        let stride = (width as usize).saturating_mul(4);
        let len = (height as usize).saturating_mul(stride);
        let Some(pixels) = pixels.get(..len) else {
            return;
        };
        self.mailbox.update(|s| {
            s.pending = Some(PendingFrame::Pixels {
                pixels: pixels.to_vec(),
                dirty: dirty.to_vec(),
                width,
                height,
                stride,
            });
            s.source = Some(source);
        });
    }

    pub(crate) fn present_shared(&self, frame: SharedTexture, source: Arc<dyn FrameSource>) {
        self.mailbox.update(|s| {
            s.pending = Some(PendingFrame::Shared(Box::new(frame)));
            s.source = Some(source);
        });
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

/// What one iteration did with the frame it took.
enum Outcome {
    /// The frame reached the surface's commit stream.
    Committed,
    /// Nothing could take the frame yet — no texture, or no painter for a
    /// shared frame — and it is owed again at this instant.
    Deferred(PendingFrame, Option<Instant>),
    /// The GPU path is done: the actor degrades to SHM and keeps the frame.
    Degraded(PendingFrame),
}

fn run_worker(mailbox: Mailbox<OverlayState>) {
    let mut backend = initial_backend();
    let content_conn = crate::x11_state::x11rb_conn();
    let mut retry = FrameRetry::default();

    loop {
        let ready = |s: &OverlayState| s.pending.is_some() || s.shutdown;
        let take = |s: &mut OverlayState| {
            let (win, gc) = s
                .content
                .as_ref()
                .map_or((None, None), |c| (Some(c.window()), Some(c.gc())));
            (
                s.pending.take().zip(s.source.take()),
                win,
                gc,
                s.target_size,
                s.shutdown,
            )
        };
        let taken = match retry.due() {
            Some(at) => mailbox.wait_until(at, ready, take),
            None => Some(mailbox.wait(ready, take)),
        };
        // Nothing arrived before the held frame came due: present it against
        // the window the mailbox still names.
        let (frame, content_window, content_gc, target_size, shutdown) =
            taken.unwrap_or_else(|| {
                mailbox.peek(|s| {
                    let (win, gc) = s
                        .content
                        .as_ref()
                        .map_or((None, None), |c| (Some(c.window()), Some(c.gc())));
                    (None, win, gc, s.target_size, s.shutdown)
                })
            });

        if shutdown {
            break;
        }
        let frame = retry.take(frame);
        let (Some(window), Some(gc)) = (content_window, content_gc) else {
            // The geometry thread has not created the window yet; the frame
            // stays owed until it has.
            if let Some((frame, source)) = frame {
                retry.defer(frame, source, jfn_gpu_paint::Deferred::new().retry_at());
            }
            continue;
        };
        let Some((frame, source)) = frame else {
            continue;
        };

        match present_frame(
            &mut backend,
            content_conn.as_deref(),
            window,
            gc,
            target_size,
            frame,
        ) {
            Outcome::Committed => {}
            Outcome::Deferred(frame, at) => retry.defer(frame, source, at),
            // The degraded backend is SHM now, so the frame it kept goes out
            // through it in the same iteration.
            Outcome::Degraded(frame) => {
                if let (Backend::Shm(state), Some(conn)) = (&mut backend, content_conn.as_deref()) {
                    present_shm(state, conn, window, gc, frame);
                }
            }
        }
    }

    teardown(backend, content_conn.as_deref(), &mailbox);
}

fn present_frame(
    backend: &mut Backend,
    content_conn: Option<&RustConnection>,
    window: xproto::Window,
    gc: xproto::Gcontext,
    target_size: (u32, u32),
    frame: PendingFrame,
) -> Outcome {
    match backend {
        Backend::Gpu(painter) => {
            let outcome = present_gpu(painter, window, target_size, &frame);
            match outcome {
                Gpu::Committed => Outcome::Committed,
                Gpu::Deferred(at) => Outcome::Deferred(frame, at),
                Gpu::Degrade => {
                    // Take the painter out and shut it down BEFORE switching —
                    // wgpu's swapchain and hand-rolled SHM must never both be
                    // writing this window.
                    if let Backend::Gpu(p) = backend
                        && let Some(p) = p.take()
                    {
                        drop(p);
                    }
                    *backend = Backend::Shm(ShmState::default());
                    Outcome::Degraded(frame)
                }
            }
        }
        Backend::Shm(state) => {
            if let Some(conn) = content_conn {
                present_shm(state, conn, window, gc, frame);
            }
            Outcome::Committed
        }
    }
}

/// What the GPU path did with the frame it was shown.
enum Gpu {
    Committed,
    Deferred(Option<Instant>),
    Degrade,
}

/// The frame is owed again one refresh from now.
fn owed_again() -> Gpu {
    Gpu::Deferred(jfn_gpu_paint::Deferred::new().retry_at())
}

/// Present through the GPU surface. Only a lost surface asks the caller to
/// degrade to SHM. A shared frame never does — dmabuf has no CPU fallback, so
/// degrading would strand the surface with no output at all.
fn present_gpu(
    painter: &mut Option<Box<jfn_gpu_paint::Surface<'static>>>,
    window: xproto::Window,
    target_size: (u32, u32),
    frame: &PendingFrame,
) -> Gpu {
    if painter.is_none() {
        let (Some(conn_ptr), Some(paint), Some(gpu)) = (
            crate::x11_state::raw_xcb_connection(),
            crate::x11_state::paint(),
            crate::paint::gpu(),
        ) else {
            return owed_again();
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
                    tracing::warn!("[x11] overlay actor gpu init failed: {e}; frame stays owed");
                    return owed_again();
                }
                eprintln!("[x11] overlay actor gpu init failed: {e}; using SHM");
                return Gpu::Degrade;
            }
        }
    }
    let Some(painter) = painter.as_mut() else {
        return owed_again();
    };
    painter.resize(PhysicalSize {
        w: target_size.0 as i32,
        h: target_size.1 as i32,
    });

    let outcome = match frame {
        PendingFrame::Pixels {
            pixels,
            dirty,
            width,
            height,
            stride,
        } => painter.present_pixels(
            Pixels {
                size: PhysicalSize {
                    w: *width,
                    h: *height,
                },
                stride: *stride as u32,
                bgra: pixels,
                dirty,
            },
            || {},
        ),
        PendingFrame::Shared(tex) => painter.present_shared(tex, || {}),
    };

    match outcome {
        Ok(_presented) => Gpu::Committed,
        Err(PresentFailed::Deferred(deferred)) => Gpu::Deferred(deferred.retry_at()),
        // The surface is fine and the producer owes the successor; nothing here
        // can conjure one, so this frame goes nowhere.
        Err(e @ (PresentFailed::Import | PresentFailed::Kind)) => {
            tracing::debug!("[x11] overlay actor frame not presented: {e}");
            owed_again()
        }
        Err(e @ PresentFailed::Lost(_)) => {
            eprintln!("[x11] overlay actor present failed: {e}; using SHM");
            Gpu::Degrade
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
        let Some(JfnRect {
            x: rx,
            y: ry,
            w: rw,
            h: rh,
        }) = rect.clamped(width, height)
        else {
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
