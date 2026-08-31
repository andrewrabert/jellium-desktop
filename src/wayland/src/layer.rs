use std::ffi::c_void;
use std::ptr::NonNull;

use thiserror::Error;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Proxy, QueueHandle};
use wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;

use jfn_gpu_paint::WindowTarget;

use crate::wl_state::{Acked, Callbacks, DispatchState, FrameBuffer};

/// Proof that the layer's surface was committed, minted at the commit.
pub(crate) struct Committed(());

impl Committed {
    pub(crate) fn issued() -> Committed {
        Committed(())
    }
}

#[derive(Debug, Error)]
pub(crate) enum PresentError {
    #[error("gpu paint failed: {0}")]
    Gpu(#[from] jfn_gpu_paint::PresentFailed),
    #[error("shm buffer allocation failed")]
    ShmAlloc,
    #[error("dmabuf buffer creation failed")]
    DmabufCreate,
}

impl PresentError {
    /// Only a lost surface degrades. Every other GPU failure names what its
    /// producer still owes — a deferred frame is presented again, a failed
    /// shared import has no CPU fallback to degrade to — and the backend stays
    /// put.
    pub(crate) fn is_degrading(&self) -> bool {
        matches!(
            self,
            PresentError::Gpu(jfn_gpu_paint::PresentFailed::Lost(_))
        )
    }
}

#[derive(Copy, Clone, PartialEq, Eq)]
pub(crate) struct ViewportState {
    pub(crate) lw: i32,
    pub(crate) lh: i32,
    pub(crate) pw: i32,
    pub(crate) ph: i32,
}

impl ViewportState {
    /// The viewport of a surface created before the window published an
    /// extent: no source and no destination.
    ///
    /// [`LayerSurface::set_viewport`] sends neither for a non-positive axis,
    /// and `wl_ops::accepts` refuses every frame until the first publish, so
    /// nothing is presented against it.
    pub(crate) const UNPUBLISHED: ViewportState = ViewportState {
        lw: 0,
        lh: 0,
        pw: 0,
        ph: 0,
    };
}

pub(crate) struct LayerSurface {
    conn: Connection,
    surface: WlSurface,
    viewport: WpViewport,
}

impl LayerSurface {
    pub(crate) fn new(conn: Connection, surface: WlSurface, viewport: WpViewport) -> Self {
        Self {
            conn,
            surface,
            viewport,
        }
    }

    pub(crate) fn window_target(&self) -> Option<WindowTarget> {
        let display = NonNull::new(self.conn.backend().display_ptr().cast::<c_void>())?;
        let surface = NonNull::new(self.surface.id().as_ptr().cast::<c_void>())?;
        Some(WindowTarget::Wayland { display, surface })
    }

    pub(crate) fn attach_none(&self) {
        self.surface.attach(None, 0, 0);
    }

    pub(crate) fn set_viewport(&self, src_w: i32, src_h: i32, dst_w: i32, dst_h: i32) {
        if src_w > 0 && src_h > 0 {
            self.viewport
                .set_source(0.0, 0.0, f64::from(src_w), f64::from(src_h));
        }
        if dst_w > 0 && dst_h > 0 {
            self.viewport.set_destination(dst_w, dst_h);
        }
    }

    pub(crate) fn present(&self, frame: FrameCommit<'_>) {
        self.set_viewport(frame.src_w, frame.src_h, frame.dst_w, frame.dst_h);
        frame.buf.attach_to(&self.surface);
        self.surface.damage_buffer(0, 0, frame.buf_w, frame.buf_h);
        self.surface.commit();
    }

    pub(crate) fn commit(&self) {
        self.surface.commit();
    }

    /// Commits the surface and arms the acknowledgement this commit has: the
    /// frame callback when it carries a buffer, the display sync when it
    /// empties the surface.
    pub(crate) fn commit_acked(
        &self,
        callbacks: &'static Callbacks,
        qh: &QueueHandle<DispatchState>,
        carries_buffer: bool,
    ) -> Acked {
        if carries_buffer {
            // Requested before the commit: a `wl_surface.frame` applies to the
            // commit that follows it.
            let acked = callbacks.arm(&self.surface.frame(qh, ()));
            self.surface.commit();
            return acked;
        }
        self.surface.commit();
        // An emptied surface is never painted again, so it has no frame
        // callback; the display round-trip is what tells us the commit landed.
        callbacks.arm(&self.conn.display().sync(qh, ()))
    }

    pub(crate) fn flush(&self) {
        let _ = self.conn.flush();
    }
}

pub(crate) struct FrameCommit<'a> {
    buf: FrameBuffer<'a>,
    buf_w: i32,
    buf_h: i32,
    src_w: i32,
    src_h: i32,
    dst_w: i32,
    dst_h: i32,
}

impl<'a> FrameCommit<'a> {
    /// Clamps `src_*` to the buffer dimensions: a `wp_viewport` source larger
    /// than the attached buffer is a fatal protocol error that kills the client.
    pub(crate) fn new(
        buf: FrameBuffer<'a>,
        buf_w: i32,
        buf_h: i32,
        src_w: i32,
        src_h: i32,
        dst_w: i32,
        dst_h: i32,
    ) -> Self {
        Self {
            buf,
            buf_w,
            buf_h,
            src_w: src_w.min(buf_w),
            src_h: src_h.min(buf_h),
            dst_w,
            dst_h,
        }
    }
}

pub(crate) struct SurfaceRef {
    surface: WlSurface,
    viewport: WpViewport,
}

impl SurfaceRef {
    pub(crate) fn new(surface: WlSurface, viewport: WpViewport) -> Self {
        Self { surface, viewport }
    }

    pub(crate) fn as_arg(&self) -> &WlSurface {
        &self.surface
    }

    pub(crate) fn window_target(&self) -> Option<WindowTarget> {
        let backend = self.surface.backend().upgrade()?;
        let display = NonNull::new(backend.display_ptr().cast::<c_void>())?;
        let surface = NonNull::new(self.surface.id().as_ptr().cast::<c_void>())?;
        Some(WindowTarget::Wayland { display, surface })
    }

    pub(crate) fn set_destination(&self, w: i32, h: i32) {
        if w > 0 && h > 0 {
            self.viewport.set_destination(w, h);
        }
    }

    pub(crate) fn destroy(self) {
        self.viewport.destroy();
        self.surface.destroy();
    }
}
