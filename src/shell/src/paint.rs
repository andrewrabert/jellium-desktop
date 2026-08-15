//! The shell overlay's swapchain, iced engine and renderer.
//!
//! Everything here runs on the render actor's thread, which is the only writer
//! of the swapchain and of the platform layer behind it.

use std::sync::Arc;

use iced_core::{Color, Size};
use iced_wgpu::graphics::{Antialiasing, Shell, Viewport};
use iced_wgpu::{Engine, Renderer};
use jfn_gpu_paint::{FrameSize, Presented, SurfaceLost, Surfaces, Swapchain, WindowTarget};

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
        size: FrameSize,
        scale: f64,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Painter, SurfaceLost> {
        let swapchain = gpu.new_swapchain(target, size)?;
        let engine = Engine::new(
            gpu.adapter(),
            gpu.device().clone(),
            gpu.queue().clone(),
            jfn_gpu_paint::FORMAT,
            Some(Antialiasing::MSAAx4),
            Shell::new(Waker(wake)),
        );
        let renderer = Renderer::new(
            engine,
            iced_core::renderer::Settings {
                default_font: crate::theme::FONT,
                ..iced_core::renderer::Settings::default()
            },
        );
        Ok(Painter {
            swapchain,
            renderer,
            viewport: viewport(size, scale),
        })
    }

    pub fn resize(&mut self, size: FrameSize, scale: f64) {
        self.swapchain.resize(size);
        self.viewport = viewport(size, scale);
    }

    pub fn renderer(&mut self) -> &mut Renderer {
        &mut self.renderer
    }

    pub fn viewport(&self) -> &Viewport {
        &self.viewport
    }

    pub fn logical_size(&self) -> Size {
        self.viewport.logical_size()
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

fn viewport(size: FrameSize, scale: f64) -> Viewport {
    Viewport::with_physical_size(
        Size::new(size.w.max(1) as u32, size.h.max(1) as u32),
        iced_core::renderer::Scale {
            window: scale as f32,
            application: 1.0,
        },
    )
}
