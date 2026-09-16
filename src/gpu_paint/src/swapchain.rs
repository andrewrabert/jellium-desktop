use crate::FrameSize;
use crate::context::{FORMAT, Surfaces};
use crate::error::{Kind, SurfaceLost};
use crate::painter::{ConfigureSite, PresentPolicy, create_surface, pick_alpha_mode, texels};
use crate::types::{Deferred, Presented, WindowTarget};

pub struct Swapchain<'ctx> {
    ctx: &'ctx Surfaces,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    configure_site: ConfigureSite,
}

impl<'ctx> Swapchain<'ctx> {
    pub(crate) fn new(
        ctx: &'ctx Surfaces,
        target: WindowTarget,
        size: FrameSize,
    ) -> Result<Self, SurfaceLost> {
        let present = PresentPolicy::for_target(&target);
        let configure_site = ConfigureSite::for_target(&target);
        let extent = texels(size).ok_or(Kind::BadDimensions(size))?;
        let max = ctx.max_texture_dim;
        if extent.0 > max || extent.1 > max {
            return Err(Kind::BadDimensions(size).into());
        }

        let surface = unsafe { create_surface(&ctx.instance, target)? };

        if !ctx.adapter.is_surface_supported(&surface) {
            return Err(Kind::SurfaceUnsupported.into());
        }

        let caps = surface.get_capabilities(&ctx.adapter);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: FORMAT,
            width: extent.0,
            height: extent.1,
            present_mode: present.mode(),
            desired_maximum_frame_latency: 2,
            alpha_mode: pick_alpha_mode(&caps),
            view_formats: vec![],
        };

        let swapchain = Self {
            ctx,
            surface,
            config,
            configure_site,
        };
        swapchain.configure();
        Ok(swapchain)
    }

    fn configure(&self) {
        self.ctx.configure_surface(&self.surface, &self.config);
    }

    pub fn resize(&mut self, size: FrameSize) {
        let Some((w, h)) = texels(size) else {
            return;
        };
        let (w, h) = (
            w.min(self.ctx.max_texture_dim),
            h.min(self.ctx.max_texture_dim),
        );
        if (w, h) == (self.config.width, self.config.height) {
            return;
        }
        self.config.width = w;
        self.config.height = h;
        self.configure();
    }

    pub fn format(&self) -> wgpu::TextureFormat {
        self.config.format
    }

    pub fn acquire(&mut self) -> Acquired<'ctx> {
        use wgpu::CurrentSurfaceTexture::*;
        let mut reconfigured = false;
        loop {
            let texture = match self.surface.get_current_texture() {
                Success(texture) => texture,
                Suboptimal(_) | Lost | Outdated if self.configure_site == ConfigureSite::Owner => {
                    return Acquired::Deferred(Deferred::new());
                }
                Suboptimal(texture) => texture,
                Lost | Outdated if !reconfigured => {
                    reconfigured = true;
                    self.configure();
                    continue;
                }
                Lost | Outdated | Timeout | Occluded => {
                    return Acquired::Deferred(Deferred::new());
                }
                Validation => {
                    tracing::error!("gpu_paint: swapchain acquire failed validation");
                    return Acquired::Deferred(Deferred::new());
                }
            };
            let view = texture
                .texture
                .create_view(&wgpu::TextureViewDescriptor::default());
            return Acquired::Frame(Frame {
                ctx: self.ctx,
                texture,
                view,
            });
        }
    }
}

#[must_use = "a frame is presented or superseded, never dropped"]
pub struct Frame<'ctx> {
    ctx: &'ctx Surfaces,
    texture: wgpu::SurfaceTexture,
    view: wgpu::TextureView,
}

impl<'ctx> Frame<'ctx> {
    pub fn view(&self) -> &wgpu::TextureView {
        &self.view
    }

    pub fn present(self, encode: impl FnOnce(&wgpu::TextureView)) -> Presented {
        let Frame { ctx, texture, view } = self;
        {
            let _gate = ctx.submit_gate.read();
            encode(&view);
        }
        drop(view);
        texture.present();
        Presented::issued()
    }

    pub fn supersede(self, successor: &mut Swapchain<'ctx>) -> Acquired<'ctx> {
        drop(self);
        successor.acquire()
    }
}

#[must_use = "each arm carries an obligation"]
pub enum Acquired<'ctx> {
    Frame(Frame<'ctx>),
    Deferred(Deferred),
}
