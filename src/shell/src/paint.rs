//! The shell overlay's swapchain, iced engine and renderer.
//!
//! Everything here runs on the render actor's thread, which is the only writer
//! of the swapchain and of the platform layer behind it.

use std::sync::Arc;

use iced_core::renderer::Renderer as _;
use iced_core::{Color, Size};
use iced_wgpu::graphics::{Antialiasing, Shell, Viewport};
use iced_wgpu::{Engine, Renderer};
use jfn_gpu_paint::{FrameSize, Presented, SurfaceLost, Surfaces, Swapchain, WindowTarget};
use jfn_platform_abi::WindowExtent;

pub struct Painter {
    swapchain: Swapchain<'static>,
    renderer: Renderer,
    viewport: Viewport,
}

struct Waker(Arc<dyn Fn() + Send + Sync>);

impl iced_wgpu::graphics::shell::Notifier for Waker {
    fn tick(&self) {
        (self.0)();
    }

    fn request_redraw(&self) {
        (self.0)();
    }

    fn invalidate_layout(&self) {
        (self.0)();
    }
}

impl Painter {
    pub fn new(
        gpu: &'static Surfaces,
        target: WindowTarget,
        extent: WindowExtent,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Painter, SurfaceLost> {
        let swapchain = gpu.new_swapchain(target, frame_size(extent))?;
        let engine = Engine::new(
            gpu.adapter(),
            gpu.device().clone(),
            gpu.queue().clone(),
            jfn_gpu_paint::FORMAT,
            Some(Antialiasing::MSAAx4),
            Shell::new(Waker(wake)),
        );
        let mut renderer = Renderer::new(
            engine,
            iced_core::renderer::Settings {
                default_font: crate::theme::FONT,
                ..iced_core::renderer::Settings::default()
            },
        );
        hint_scale(&mut renderer, extent);
        Ok(Painter {
            swapchain,
            renderer,
            viewport: viewport(extent),
        })
    }

    pub fn resize(&mut self, extent: WindowExtent) {
        self.swapchain.resize(frame_size(extent));
        self.viewport = viewport(extent);
        hint_scale(&mut self.renderer, extent);
    }

    pub fn renderer(&mut self) -> &mut Renderer {
        &mut self.renderer
    }

    pub fn viewport(&self) -> &Viewport {
        &self.viewport
    }

    /// Acquires one frame with no gate held. A stale or occluded swapchain
    /// hands back the retry it owes instead.
    pub fn acquire(&mut self) -> jfn_gpu_paint::Acquired<'static> {
        self.swapchain.acquire()
    }

    /// Encodes the renderer's scene into `frame` inside the submit gate and
    /// commits it outside.
    ///
    /// The frame is always cleared fully transparent; opacity is a widget's to
    /// draw.
    pub fn present(&mut self, frame: jfn_gpu_paint::Frame<'static>) -> Presented {
        let format = self.swapchain.format();
        let renderer = &mut self.renderer;
        let viewport = &self.viewport;
        frame.present(|view| {
            let _submitted = renderer.present(Some(Color::TRANSPARENT), format, view, viewport);
        })
    }
}

/// The swapchain size an extent names.
pub fn frame_size(extent: WindowExtent) -> FrameSize {
    let physical = extent.physical();
    FrameSize {
        w: physical.w,
        h: physical.h,
    }
}

/// The viewport an extent names: its physical size, and the platform's
/// reported scale as iced's window scale, application scale 1.0.
pub fn viewport(extent: WindowExtent) -> Viewport {
    let size = frame_size(extent);
    Viewport::with_physical_size(
        Size::new(size.w.max(1) as u32, size.h.max(1) as u32),
        iced_core::renderer::Scale {
            window: extent.scale().as_f32(),
            application: 1.0,
        },
    )
}

/// Text is hinted against the scale it is drawn at, so a scale change
/// re-hints it.
fn hint_scale(renderer: &mut Renderer, extent: WindowExtent) {
    renderer.hint(iced_core::renderer::Scale {
        window: extent.scale().as_f32(),
        application: 1.0,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use jfn_platform_abi::{COVERED_SCALES, LogicalSize, Scale};

    const LOGICAL: LogicalSize = LogicalSize { w: 1280, h: 720 };

    /// One extent per covered scale, each carrying that scale verbatim.
    fn extents() -> Vec<Option<WindowExtent>> {
        COVERED_SCALES
            .into_iter()
            .map(|s| WindowExtent::new(LOGICAL.to_physical(s)?, s, LOGICAL))
            .collect()
    }

    #[test]
    fn the_viewport_carries_the_reported_scale_at_every_covered_scale() {
        let reported: Vec<Option<f32>> = extents()
            .into_iter()
            .map(|e| Some(viewport(e?).scale().window))
            .collect();
        let expected: Vec<Option<f32>> = COVERED_SCALES
            .into_iter()
            .map(|s| Some(s.as_f32()))
            .collect();
        assert_eq!(reported, expected);
    }

    #[test]
    fn the_viewport_is_the_extent_s_physical_size_at_every_covered_scale() {
        let sizes: Vec<Option<(u32, u32)>> = extents()
            .into_iter()
            .map(|e| {
                let e = e?;
                let size = viewport(e).physical_size();
                Some((size.width, size.height))
            })
            .collect();
        let expected: Vec<Option<(u32, u32)>> = COVERED_SCALES
            .into_iter()
            .map(|s| {
                let p = LOGICAL.to_physical(s)?;
                Some((p.w as u32, p.h as u32))
            })
            .collect();
        assert_eq!(sizes, expected);
    }

    #[test]
    fn the_swapchain_size_is_the_extent_s_physical_size_at_every_covered_scale() {
        let sizes: Vec<Option<FrameSize>> = extents()
            .into_iter()
            .map(|e| Some(frame_size(e?)))
            .collect();
        let expected: Vec<Option<FrameSize>> = COVERED_SCALES
            .into_iter()
            .map(|s| {
                let p = LOGICAL.to_physical(s)?;
                Some(FrameSize { w: p.w, h: p.h })
            })
            .collect();
        assert_eq!(sizes, expected);
    }

    #[test]
    fn the_application_scale_is_one_at_every_covered_scale() {
        let applications: Vec<Option<f32>> = extents()
            .into_iter()
            .map(|e| Some(viewport(e?).scale().application))
            .collect();
        assert_eq!(applications, vec![Some(1.0); COVERED_SCALES.len()]);
        assert_eq!(Scale::ONE.as_f32(), 1.0);
    }
}
