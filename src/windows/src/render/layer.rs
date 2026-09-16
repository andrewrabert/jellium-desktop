use std::ptr::NonNull;

use jfn_gpu_paint::{
    FrameSize, Pixels, PresentFailed, SharedTexture, Surface as Painter, WindowTarget,
};
use windows::Win32::Graphics::DirectComposition::{IDCompositionVisual, IDCompositionVisual3};
use windows_core::Interface;

use crate::render::device;

pub(crate) struct Layer {
    visual: IDCompositionVisual,
    painter: Option<Painter<'static>>,
    external: bool,
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

    pub(crate) fn set_offset(&mut self, x: f32, y: f32) {
        unsafe {
            let _ = self.visual.SetOffsetX2(x);
            let _ = self.visual.SetOffsetY2(y);
        }
    }

    pub(crate) fn detach(&mut self) {
        self.clear_content();
        if let Some(painter) = self.painter.as_mut() {
            painter.content_detached();
        }
    }

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

    pub(crate) fn window_target(&mut self) -> Option<WindowTarget> {
        self.external = true;
        self.painter = None;
        let visual = NonNull::new(self.visual.as_raw())?;
        Some(WindowTarget::CompositionVisual { visual })
    }

    pub(crate) fn ready(&mut self, size: FrameSize) -> bool {
        !self.external && (self.painter.is_some() || self.build_painter(size))
    }

    pub(crate) fn present_pixels(&mut self, pixels: Pixels<'_>) -> bool {
        let Some(painter) = self.painter.as_mut() else {
            return false;
        };
        let outcome = painter.present_pixels(pixels, || {});
        self.settle(outcome)
    }

    pub(crate) fn present_shared(&mut self, texture: &SharedTexture) -> bool {
        let Some(painter) = self.painter.as_mut() else {
            return false;
        };
        let outcome = painter.present_shared(texture, || {});
        self.settle(outcome)
    }

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
