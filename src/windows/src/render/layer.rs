//! One visual and the painter bound to it — the single shape the content view
//! and the OSR popup both take. A layer owns its painter for its whole life;
//! nothing checks one out.

use std::ptr::NonNull;

use jfn_gpu_paint::{Frame, FrameSize, Presented, SharedTexture, Surface as Painter, WindowTarget};
use windows::Win32::Graphics::DirectComposition::IDCompositionVisual;
use windows_core::Interface;

use crate::render::device;

pub(crate) struct Layer {
    visual: IDCompositionVisual,
    /// `None` only until the first frame builds the swapchain, and again
    /// after a present reported the surface lost.
    painter: Option<Painter<'static>>,
    visible: bool,
}

impl Layer {
    pub(crate) fn new(visual: IDCompositionVisual, visible: bool) -> Layer {
        Layer {
            visual,
            painter: None,
            visible,
        }
    }

    pub(crate) fn visual(&self) -> &IDCompositionVisual {
        &self.visual
    }

    /// Returns whether visibility changed; a hide detaches the content.
    pub(crate) fn set_visible(&mut self, visible: bool) -> bool {
        if self.visible == visible {
            return false;
        }
        self.visible = visible;
        if !visible {
            self.detach();
        }
        // visible=true: content rebinds on the next present's configure.
        true
    }

    /// Physical-pixel offset of the visual inside its parent.
    pub(crate) fn set_offset(&mut self, x: f32, y: f32) {
        unsafe {
            let _ = self.visual.SetOffsetX2(x);
            let _ = self.visual.SetOffsetY2(y);
        }
    }

    /// Sever the visual's content and mark the painter for a rebind: wgpu
    /// binds the swapchain to the visual inside `configure` and nowhere else,
    /// so an owner-side `SetContent(None)` leaves a painter whose extent never
    /// moved and whose content is unbound.
    pub(crate) fn detach(&mut self) {
        self.clear_content();
        if let Some(painter) = self.painter.as_mut() {
            painter.content_detached();
        }
    }

    fn clear_content(&self) {
        unsafe {
            let _ = self.visual.SetContent(None::<&windows_core::IUnknown>);
        }
    }

    /// Presents one frame, building the swapchain from `size` on first use.
    pub(crate) fn present(&mut self, frame: Frame<'_>, size: FrameSize) -> bool {
        if !self.visible {
            return false;
        }
        // The frame that opens the device is also what names the adapter to
        // open it on, so it has to reach the painter build and not just the
        // present.
        let sample = match &frame {
            Frame::Shared(tex) => Some(*tex),
            Frame::Copied(_) => None,
        };
        if self.painter.is_none() && !self.build_painter(size, sample) {
            return false;
        }
        let Some(painter) = self.painter.as_mut() else {
            return false;
        };
        match painter.present(frame, || {}) {
            Ok(presented) => presented == Presented::Yes,
            Err(e) => {
                tracing::error!(target: "platform", "gpu_paint present failed: {e}");
                // The next frame builds a fresh swapchain.
                self.painter = None;
                self.clear_content();
                false
            }
        }
    }

    fn build_painter(&mut self, size: FrameSize, sample: Option<&SharedTexture>) -> bool {
        let Some(gpu) = device::gpu(sample) else {
            return false;
        };
        let Some(visual) = NonNull::new(self.visual.as_raw()) else {
            return false;
        };
        match gpu.new_surface(WindowTarget::CompositionVisual { visual }, size) {
            Ok(painter) => {
                self.painter = Some(painter);
                true
            }
            Err(e) => {
                tracing::error!(target: "platform", "gpu_paint surface creation failed: {e}");
                false
            }
        }
    }
}
