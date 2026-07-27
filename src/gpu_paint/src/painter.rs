//! Per-surface Vulkan compositor. Owns a swapchain, render pipeline,
//! and a persistent upload texture; presents BGRA pixels uploaded via
//! `queue.write_texture` (dirty-rect granularity).

use std::num::NonZeroU32;
use std::sync::Arc;

use raw_window_handle::{
    RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle, XcbDisplayHandle,
    XcbWindowHandle,
};

use crate::context::GpuContext;
use crate::error::GpuPaintError;
use crate::types::{DmabufFrame, PixelFrame, WindowTarget};

const SURFACE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8Unorm;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PresentOutcome {
    Presented,
    Skipped,
}

/// How the painter chooses its swapchain extent.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SizePolicy {
    /// The swapchain tracks each incoming frame's size. Used where another layer
    /// (Wayland's `wp_viewport`) rescales the buffer to the surface's logical
    /// size, so presenting at the producer's size keeps content 1:1.
    FollowFrame,
    /// The swapchain tracks the target extent set via [`GpuPainter::resize`]
    /// (the parent-derived window size), clamped to device limits — NOT the
    /// incoming frame size. Frames render 1:1 into the top-left; a frame smaller
    /// than the target leaves a transparent strip, a larger one is clipped. Used
    /// on X11, where the swapchain IS the window drawable and its geometry owner
    /// (the geometry thread) sizes the window, not the painter.
    FollowTarget,
}

pub struct GpuPainter {
    ctx: Arc<GpuContext>,
    // 'static is a lie that wgpu accepts via `create_surface_unsafe`;
    // the caller guarantees the window outlives the painter (X11 owns
    // the xcb_window for the surface lifetime, Wayland likewise).
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    bind_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    // Persistent upload texture sized to the swapchain. Recreated on
    // resize. `None` until the first frame establishes a size.
    upload: Option<UploadTexture>,
    // Stored target size from the most recent `resize` call. Acts as
    // the gate: we only reconfigure (and present) once an incoming
    // frame matches it.
    pending_size: (u32, u32),
    visible: bool,
    policy: SizePolicy,
}

struct UploadTexture {
    tex: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    w: u32,
    h: u32,
    // Dirty-only writes assume a prior base; a freshly (re)created texture has
    // none, so the first frame after must be a full write.
    needs_base: bool,
}

impl UploadTexture {
    fn write(&mut self, queue: &wgpu::Queue, frame: &PixelFrame<'_>, cw: u32, ch: u32) {
        let bound_w = frame.width.min(cw) as i32;
        let bound_h = frame.height.min(ch) as i32;
        if self.needs_base || frame.dirty.is_empty() {
            write_rect(queue, self, frame, 0, 0, bound_w, bound_h);
            self.needs_base = false;
        } else {
            for r in frame.dirty {
                let (x, y, w, h) = clip_rect(r.x, r.y, r.w, r.h, bound_w, bound_h);
                if w <= 0 || h <= 0 {
                    continue;
                }
                write_rect(queue, self, frame, x, y, w, h);
            }
        }
    }
}

impl GpuPainter {
    pub fn new(
        ctx: Arc<GpuContext>,
        target: WindowTarget,
        size: (u32, u32),
    ) -> Result<Self, GpuPaintError> {
        Self::with_policy(ctx, target, size, SizePolicy::FollowFrame)
    }

    pub fn with_policy(
        ctx: Arc<GpuContext>,
        target: WindowTarget,
        size: (u32, u32),
        policy: SizePolicy,
    ) -> Result<Self, GpuPaintError> {
        if size.0 == 0 || size.1 == 0 {
            return Err(GpuPaintError::BadDimensions(size.0, size.1));
        }
        let max = ctx.device.limits().max_texture_dimension_2d;
        if size.0 > max || size.1 > max {
            return Err(GpuPaintError::BadDimensions(size.0, size.1));
        }

        let surface = unsafe { create_surface(&ctx.instance, target)? };

        if !ctx.adapter.is_surface_supported(&surface) {
            return Err(GpuPaintError::SurfaceUnsupported);
        }

        let caps = surface.get_capabilities(&ctx.adapter);
        let alpha_mode = pick_alpha_mode(&caps);

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: SURFACE_FORMAT,
            width: size.0,
            height: size.1,
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            alpha_mode,
            view_formats: vec![],
            color_space: wgpu::SurfaceColorSpace::Auto,
        };
        // Other surfaces may be submitting on the shared device while this
        // painter is created, so the first configure must be gated too.
        ctx.configure_surface(&surface, &config);

        let shader = ctx
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("jfn_gpu_paint overlay"),
                source: wgpu::ShaderSource::Wgsl(include_str!("shaders/overlay.wgsl").into()),
            });

        let bind_layout = ctx
            .device
            .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("jfn_gpu_paint bgl"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: false },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::NonFiltering),
                        count: None,
                    },
                ],
            });

        let pipeline_layout = ctx
            .device
            .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("jfn_gpu_paint pl"),
                bind_group_layouts: &[Some(&bind_layout)],
                immediate_size: 0,
            });

        let pipeline = ctx
            .device
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("jfn_gpu_paint pipeline"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs_main"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: SURFACE_FORMAT,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            });

        // Nearest, no anisotropy — 1:1 sampling, never stretch.
        let sampler = ctx.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("jfn_gpu_paint sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });

        Ok(Self {
            ctx,
            surface,
            config,
            pipeline,
            bind_layout,
            sampler,
            upload: None,
            pending_size: size,
            visible: true,
            policy,
        })
    }

    fn clamp_extent(&self, size: (u32, u32)) -> (u32, u32) {
        let max = self.ctx.device.limits().max_texture_dimension_2d.max(1);
        (size.0.clamp(1, max), size.1.clamp(1, max))
    }

    /// Store a new target size. Does not reconfigure the swapchain —
    /// next matching-size `push_pixels` does that. Mirrors the wayland
    /// `transitioning` gate: gaps acceptable during resize, stretching
    /// forbidden.
    pub fn resize(&mut self, size: (u32, u32)) {
        if size.0 == 0 || size.1 == 0 {
            return;
        }
        self.pending_size = size;
    }

    pub fn set_visible(&mut self, v: bool) {
        self.visible = v;
    }

    pub fn push_pixels(
        &mut self,
        frame: PixelFrame<'_>,
        on_present: impl FnOnce(),
    ) -> Result<PresentOutcome, GpuPaintError> {
        if frame.width == 0 || frame.height == 0 {
            return Err(GpuPaintError::BadDimensions(frame.width, frame.height));
        }
        let max = self.ctx.device.limits().max_texture_dimension_2d;
        if frame.width > max || frame.height > max {
            return Err(GpuPaintError::BadDimensions(frame.width, frame.height));
        }
        if !self.visible {
            return Ok(PresentOutcome::Skipped);
        }

        // Choose the swapchain extent. `FollowFrame` tracks the producer's size
        // (Wayland rescales via viewport). `FollowTarget` tracks the
        // parent-derived window size set through `resize` — the swapchain IS the
        // X11 window drawable, so it must match the window the geometry thread
        // sized, not the (possibly lagging) frame.
        let (cw, ch) = match self.policy {
            SizePolicy::FollowFrame => (frame.width, frame.height),
            SizePolicy::FollowTarget => self.clamp_extent(self.pending_size),
        };
        if (self.config.width, self.config.height) != (cw, ch) {
            self.config.width = cw;
            self.config.height = ch;
            self.ctx.configure_surface(&self.surface, &self.config);
            // Fresh (transparent) upload so a frame smaller than the swapchain
            // leaves a transparent remainder rather than stale pixels.
            self.upload = None;
            if self.policy == SizePolicy::FollowFrame {
                self.pending_size = (cw, ch);
            }
        }

        // Upload matches the swapchain, so the fullscreen quad is always 1:1.
        self.ensure_upload(cw, ch);
        let Some(upload) = self.upload.as_mut() else {
            return Ok(PresentOutcome::Skipped);
        };
        upload.write(&self.ctx.queue, &frame, cw, ch);

        let Some(upload) = self.upload.as_ref() else {
            return Ok(PresentOutcome::Skipped);
        };
        let bind_group = &upload.bind_group;
        self.draw_and_present(bind_group, None, None, on_present)
    }

    pub fn push_dmabuf(&mut self, frame: DmabufFrame) -> Result<PresentOutcome, GpuPaintError> {
        if frame.width == 0 || frame.height == 0 {
            return Err(GpuPaintError::BadDimensions(frame.width, frame.height));
        }
        let max = self.ctx.device.limits().max_texture_dimension_2d;
        if frame.width > max || frame.height > max {
            return Err(GpuPaintError::BadDimensions(frame.width, frame.height));
        }
        if !self.visible {
            return Ok(PresentOutcome::Skipped);
        }

        let (cw, ch) = match self.policy {
            SizePolicy::FollowFrame => (frame.width, frame.height),
            SizePolicy::FollowTarget => self.clamp_extent(self.pending_size),
        };
        if (self.config.width, self.config.height) != (cw, ch) {
            self.config.width = cw;
            self.config.height = ch;
            self.ctx.configure_surface(&self.surface, &self.config);
            self.upload = None;
            if self.policy == SizePolicy::FollowFrame {
                self.pending_size = (cw, ch);
            }
        }

        // FollowTarget: the imported frame texture is frame-sized; render it 1:1
        // into the top-left of the (window-sized) swapchain via the viewport, so
        // a size mismatch during resize is a transparent strip / crop, not a
        // stretch. FollowFrame draws fullscreen (swapchain == frame).
        let viewport = match self.policy {
            SizePolicy::FollowFrame => None,
            SizePolicy::FollowTarget => Some((
                0.0,
                0.0,
                frame.width.min(cw) as f32,
                frame.height.min(ch) as f32,
            )),
        };

        let (texture, image) = unsafe { crate::dmabuf_import::import(&self.ctx.device, &frame) }?;
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = self
            .ctx
            .device
            .create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("jfn_gpu_paint dmabuf bg"),
                layout: &self.bind_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                ],
            });

        self.draw_and_present(&bind_group, Some(image), viewport, || {})
    }

    pub fn shutdown(self) {
        // Drop order matters: surface before device queue; wgpu handles
        // it via field order. Explicit method is here so callers
        // signal intent and we can extend later if needed.
        drop(self);
    }

    // ----- internals -----

    fn ensure_upload(&mut self, w: u32, h: u32) {
        let needs_new = self.upload.as_ref().is_none_or(|u| u.w != w || u.h != h);
        if needs_new {
            let tex = self.ctx.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("jfn_gpu_paint upload"),
                size: wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: SURFACE_FORMAT,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
            let bind_group = self
                .ctx
                .device
                .create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("jfn_gpu_paint bg"),
                    layout: &self.bind_layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(&view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::Sampler(&self.sampler),
                        },
                    ],
                });
            self.upload = Some(UploadTexture {
                tex,
                bind_group,
                w,
                h,
                needs_base: true,
            });
        }
    }

    fn draw_and_present(
        &self,
        bind_group: &wgpu::BindGroup,
        external_image: Option<u64>,
        viewport: Option<(f32, f32, f32, f32)>,
        on_present: impl FnOnce(),
    ) -> Result<PresentOutcome, GpuPaintError> {
        use wgpu::CurrentSurfaceTexture::*;
        // Hold the read side of the submit gate across the whole
        // acquire→submit→present so concurrent surfaces submit in parallel but
        // never overlap a configure (write). The gate is non-reentrant, so it is
        // dropped before every configure and re-taken afterward.
        let mut read = self.ctx.submit_gate.read();
        // Track SUBOPTIMAL explicitly: the frame is usable, but the swapchain no
        // longer matches the surface, so rebuild it after presenting.
        let mut suboptimal = false;
        let mut reconfigured = false;
        let frame = loop {
            match self.surface.get_current_texture() {
                Success(f) => break f,
                Suboptimal(f) => {
                    suboptimal = true;
                    break f;
                }
                // Stale swapchain (typically a resize). Reconfigure and retry
                // ONCE, presenting THIS frame — overlay content is event-driven
                // and may not repaint for a long time, so a drop leaves it stale.
                Lost | Outdated if !reconfigured => {
                    reconfigured = true;
                    drop(read);
                    self.ctx.configure_surface(&self.surface, &self.config);
                    read = self.ctx.submit_gate.read();
                }
                // Transient (occluded, timed out, or still stale after reconfigure):
                // skip without faulting — an Err would degrade the backend to SHM.
                Lost | Outdated | Timeout | Occluded => return Ok(PresentOutcome::Skipped),
                Validation => return Err(GpuPaintError::Acquire("validation")),
            }
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        // The acquire barrier must precede the render pass, in its own
        // command buffer: wgpu 29 forbids mixing raw HAL encoding
        // (`as_hal_mut`) and normal wgpu encoding on one CommandEncoder.
        if let Some(image) = external_image {
            let mut acquire_encoder =
                self.ctx
                    .device
                    .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                        label: Some("jfn_gpu_paint dmabuf acquire enc"),
                    });
            crate::dmabuf_import::acquire_barrier(&self.ctx.device, &mut acquire_encoder, image);
            self.ctx
                .queue
                .submit(std::iter::once(acquire_encoder.finish()));
        }

        let mut encoder = self
            .ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("jfn_gpu_paint enc"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("jfn_gpu_paint pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                occlusion_query_set: None,
                timestamp_writes: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, bind_group, &[]);
            // A viewport smaller than the attachment draws the frame 1:1 in the
            // top-left; the cleared remainder stays transparent.
            if let Some((x, y, w, h)) = viewport
                && w > 0.0
                && h > 0.0
            {
                pass.set_viewport(x, y, w, h, 0.0, 1.0);
            }
            pass.draw(0..3, 0..1);
        }
        self.ctx.queue.submit(std::iter::once(encoder.finish()));
        // Run after the early-return present failures above, so the closure's
        // surface-state updates only latch on a frame that actually presents.
        on_present();
        self.ctx.queue.present(frame);
        // SUBOPTIMAL: the presented frame was fine, but rebuild the swapchain so
        // the next acquire is fresh rather than repeatedly suboptimal. Drop the
        // read guard first — configure takes the write side, which is exclusive.
        drop(read);
        if suboptimal {
            self.ctx.configure_surface(&self.surface, &self.config);
        }
        Ok(PresentOutcome::Presented)
    }
}

fn clip_rect(x: i32, y: i32, w: i32, h: i32, fw: i32, fh: i32) -> (i32, i32, i32, i32) {
    let mut nx = x.max(0);
    let mut ny = y.max(0);
    let mut nw = w + x.min(0);
    let mut nh = h + y.min(0);
    if nx + nw > fw {
        nw = fw - nx;
    }
    if ny + nh > fh {
        nh = fh - ny;
    }
    if nw < 0 {
        nw = 0;
    }
    if nh < 0 {
        nh = 0;
    }
    // Shadow check: starting offset still in-bounds.
    if nx >= fw {
        nx = fw - 1;
        nw = 0;
    }
    if ny >= fh {
        ny = fh - 1;
        nh = 0;
    }
    (nx, ny, nw, nh)
}

fn write_rect(
    queue: &wgpu::Queue,
    upload: &UploadTexture,
    frame: &PixelFrame<'_>,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) {
    let stride = frame.stride as usize;
    let start = (y as usize) * stride + (x as usize) * 4;
    let end = start + ((h - 1) as usize) * stride + (w as usize) * 4;
    let slice = &frame.bgra[start..end];
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &upload.tex,
            mip_level: 0,
            origin: wgpu::Origin3d {
                x: x as u32,
                y: y as u32,
                z: 0,
            },
            aspect: wgpu::TextureAspect::All,
        },
        slice,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(frame.stride),
            rows_per_image: NonZeroU32::new(h as u32).map(|n| n.get()),
        },
        wgpu::Extent3d {
            width: w as u32,
            height: h as u32,
            depth_or_array_layers: 1,
        },
    );
}

fn pick_alpha_mode(caps: &wgpu::SurfaceCapabilities) -> wgpu::CompositeAlphaMode {
    use wgpu::CompositeAlphaMode::*;
    [PreMultiplied, PostMultiplied, Inherit, Opaque, Auto]
        .into_iter()
        .find(|m| caps.alpha_modes.contains(m))
        .unwrap_or(Auto)
}

unsafe fn create_surface(
    instance: &wgpu::Instance,
    target: WindowTarget,
) -> Result<wgpu::Surface<'static>, GpuPaintError> {
    let (display, window) = match target {
        WindowTarget::Xcb {
            connection,
            window,
            screen,
            visual,
        } => {
            let display = XcbDisplayHandle::new(Some(connection.cast()), screen);
            let mut wh = XcbWindowHandle::new(
                NonZeroU32::new(window).ok_or(GpuPaintError::SurfaceUnsupported)?,
            );
            wh.visual_id = NonZeroU32::new(visual);
            (RawDisplayHandle::Xcb(display), RawWindowHandle::Xcb(wh))
        }
        WindowTarget::Wayland { display, surface } => {
            let dh = WaylandDisplayHandle::new(display);
            let wh = WaylandWindowHandle::new(surface);
            (RawDisplayHandle::Wayland(dh), RawWindowHandle::Wayland(wh))
        }
    };
    let surface = unsafe {
        instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
            raw_display_handle: Some(display),
            raw_window_handle: window,
        })?
    };
    Ok(surface)
}
