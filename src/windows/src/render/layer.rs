//! One visual and the painter bound to it — the single shape the content view
//! and the OSR popup both take. A layer owns its painter for its whole life;
//! nothing checks one out.

use std::ptr::NonNull;

use jfn_gpu_paint::{
    FrameSize, Pixels, PresentFailed, SharedTexture, Surface as Painter, WindowTarget,
};
use windows::Win32::Graphics::DirectComposition::{IDCompositionVisual, IDCompositionVisual3};
use windows_core::Interface;

use crate::render::device;

pub(crate) struct Layer {
    visual: IDCompositionVisual,
    /// `None` only until the first frame builds the swapchain, and again
    /// after a present reported the surface lost.
    painter: Option<Painter<'static>>,
    /// Set by [`Layer::window_target`]: the caller presents to this visual
    /// itself, so no painter is ever built for it and no frame is ever
    /// accepted here.
    external: bool,
    /// Set whenever a present changed the visual's content binding; drained by
    /// [`Layer::take_needs_commit`].
    needs_commit: bool,
}

impl Layer {
    pub(crate) fn new(visual: IDCompositionVisual) -> Layer {
        Layer {
            visual,
            painter: None,
            external: false,
            needs_commit: false,
        }
    }

    pub(crate) fn visual(&self) -> &IDCompositionVisual {
        &self.visual
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

    /// Sets the visual's visibility through `IDCompositionVisual3::SetVisible`.
    /// The swapchain's binding to the visual is untouched, so a layer shown
    /// again presents into the visual it was already bound to, and an
    /// externally presented layer keeps its content across a hide.
    pub(crate) fn set_visible(&self, visible: bool) {
        let visual3: IDCompositionVisual3 = match self.visual.cast() {
            Ok(visual3) => visual3,
            Err(e) => {
                tracing::error!(target: "platform", "IDCompositionVisual3 unavailable: {e:?}");
                return;
            }
        };
        if let Err(e) = unsafe { visual3.SetVisible(visible) } {
            tracing::error!(target: "platform", "SetVisible failed: {e:?}");
        }
    }

    fn clear_content(&self) {
        unsafe {
            let _ = self.visual.SetContent(None::<&windows_core::IUnknown>);
        }
    }

    /// The content `IDCompositionVisual` for this layer.
    ///
    /// The first call marks the layer external: it builds no painter and
    /// accepts no frame.
    pub(crate) fn window_target(&mut self) -> Option<WindowTarget> {
        self.external = true;
        self.painter = None;
        let visual = NonNull::new(self.visual.as_raw())?;
        Some(WindowTarget::CompositionVisual { visual })
    }

    /// Whether a frame of `size` can be handed to this layer, building the
    /// swapchain from `size` on first use. Asked before a frame is consumed,
    /// so a layer that cannot present one never claims it.
    pub(crate) fn ready(&mut self, size: FrameSize) -> bool {
        !self.external && (self.painter.is_some() || self.build_painter(size))
    }

    /// Presents CPU pixels, latching the copied kind on this layer's first
    /// frame. Reports whether the frame reached the swapchain's commit stream.
    pub(crate) fn present_pixels(&mut self, pixels: Pixels<'_>) -> bool {
        let Some(painter) = self.painter.as_mut() else {
            return false;
        };
        let outcome = painter.present_pixels(pixels, || {});
        self.settle(outcome)
    }

    /// Presents a shared texture, latching the shared kind on this layer's
    /// first frame. Reports whether the frame reached the commit stream.
    pub(crate) fn present_shared(&mut self, texture: &SharedTexture) -> bool {
        let Some(painter) = self.painter.as_mut() else {
            return false;
        };
        let outcome = painter.present_shared(texture, || {});
        self.settle(outcome)
    }

    /// Records whether the visual's content binding changed (a configure bound
    /// the swapchain, or a failure severed it), so the device must `Commit` to
    /// publish it; plain presents flip the bound swapchain without one.
    fn settle<T>(&mut self, outcome: Result<T, PresentFailed>) -> bool {
        match outcome {
            Ok(_presented) => {
                if let Some(painter) = self.painter.as_ref()
                    && painter.take_configured()
                {
                    self.needs_commit = true;
                }
                true
            }
            Err(PresentFailed::Deferred(_) | PresentFailed::Import | PresentFailed::Kind) => false,
            Err(PresentFailed::Lost(e)) => {
                tracing::error!(target: "platform", "gpu_paint present failed: {e}");
                self.painter = None;
                self.clear_content();
                self.needs_commit = true;
                false
            }
        }
    }

    /// Whether a `Commit` is owed for a content binding this layer changed.
    /// Drained, so one commit answers every present since the last.
    pub(crate) fn take_needs_commit(&mut self) -> bool {
        std::mem::take(&mut self.needs_commit)
    }

    fn build_painter(&mut self, size: FrameSize) -> bool {
        let Some(gpu) = device::gpu() else {
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
