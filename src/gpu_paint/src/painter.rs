//! One window's swapchain, render pipeline, and persistent upload texture.
//!
//! Copied frames are uploaded via `queue.write_texture` at dirty-rect
//! granularity; shared frames are imported as a texture and sampled.

use std::num::NonZeroU32;

use raw_window_handle::{
    RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle, XcbDisplayHandle,
    XcbWindowHandle,
};

use crate::context::Surfaces;
use crate::error::{Kind, SurfaceLost};
use crate::types::{Frame, PaintMode, Pixels, Presented, WindowTarget};
use jfn_platform_abi::{PhysicalSize, SharedTexture};

const SURFACE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8Unorm;

/// How a surface chooses its swapchain extent.
///
/// Derived from [`WindowTarget`], never chosen by a caller: whether the
/// swapchain *is* the window is a fact about the window system.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SizePolicy {
    /// Track each incoming frame's size. Used where another layer (Wayland's
    /// `wp_viewport`) rescales the buffer to the surface's logical size, so
    /// presenting at the producer's size keeps content 1:1.
    FollowFrame,
    /// Track the target extent set via [`Surface::resize`] (the parent-derived
    /// window size), clamped to device limits — NOT the incoming frame size.
    /// Frames render 1:1 into the top-left; a frame smaller than the target
    /// leaves a transparent strip, a larger one is clipped. Used where the
    /// swapchain IS the window drawable and its geometry owner sizes the
    /// window, not the painter.
    FollowTarget,
}

impl SizePolicy {
    const fn for_target(target: &WindowTarget) -> Self {
        match target {
            // The swapchain is the window drawable.
            WindowTarget::Xcb { .. } => Self::FollowTarget,
            // `wp_viewport` rescales the buffer to the surface's logical size.
            WindowTarget::Wayland { .. } => Self::FollowFrame,
        }
    }
}

pub struct Surface<'a> {
    ctx: &'a Surfaces,
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
    mode: Option<PaintMode>,
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
    fn write(&mut self, queue: &wgpu::Queue, frame: &Pixels<'_>, cw: u32, ch: u32) {
        let bound_w = frame.size.w.min(cw as i32);
        let bound_h = frame.size.h.min(ch as i32);
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

impl<'a> Surface<'a> {
    pub(crate) fn new(
        ctx: &'a Surfaces,
        target: WindowTarget,
        size: PhysicalSize,
    ) -> Result<Self, SurfaceLost> {
        let policy = SizePolicy::for_target(&target);
        let extent = texels(size).ok_or(Kind::BadDimensions(size))?;
        let max = ctx.device.limits().max_texture_dimension_2d;
        if extent.0 > max || extent.1 > max {
            return Err(Kind::BadDimensions(size).into());
        }

        let surface = unsafe { create_surface(&ctx.instance, target)? };

        if !ctx.adapter.is_surface_supported(&surface) {
            return Err(Kind::SurfaceUnsupported.into());
        }

        let caps = surface.get_capabilities(&ctx.adapter);
        let alpha_mode = pick_alpha_mode(&caps);

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: SURFACE_FORMAT,
            width: extent.0,
            height: extent.1,
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
            pending_size: extent,
            visible: true,
            policy,
            mode: None,
        })
    }

    fn clamp_extent(&self, size: (u32, u32)) -> (u32, u32) {
        let max = self.ctx.device.limits().max_texture_dimension_2d.max(1);
        (size.0.clamp(1, max), size.1.clamp(1, max))
    }

    /// Store a new target size. Does not reconfigure the swapchain — the next
    /// matching-size present does that. Gaps during a resize are acceptable;
    /// stretching is not.
    pub fn resize(&mut self, size: PhysicalSize) {
        if let Some(size) = texels(size) {
            self.pending_size = size;
        }
    }

    pub fn set_visible(&mut self, v: bool) {
        self.visible = v;
    }

    /// Present one frame.
    ///
    /// `on_present` runs between submit and present, so a caller can latch
    /// state against the frame actually being shown (Wayland sets its viewport
    /// source there) without that state applying to a frame that was skipped.
    pub fn present(
        &mut self,
        frame: Frame<'_>,
        on_present: impl FnOnce(),
    ) -> Result<Presented, SurfaceLost> {
        // A surface latches its frame kind from the first frame it presents and
        // will not take the other kind afterwards: `Copied` maintains a
        // persistent upload texture that a `Shared` frame would leave stale,
        // and the next dirty-only frame would then patch onto a base two frames
        // old. CEF fixes the kind per browser via `shared_texture_enabled`, so
        // a mismatch means something upstream is wrong, not that this surface
        // is lost — drop the frame and say so.
        match self.mode {
            Some(mode) if mode != frame.mode() => {
                tracing::warn!("gpu_paint: frame kind changed on a live surface; dropping frame");
                return Ok(Presented::Skipped);
            }
            Some(_) => {}
            None => self.mode = Some(frame.mode()),
        }
        match frame {
            Frame::Copied(px) => self.present_pixels(px, on_present),
            Frame::Shared(tex) => self.present_shared(tex, on_present),
        }
    }

    /// The swapchain extent for this frame. `FollowFrame` tracks the producer's
    /// size (another layer rescales it); `FollowTarget` tracks the
    /// parent-derived window size set through `resize` — the swapchain IS the
    /// window drawable, so it must match the window its geometry owner sized,
    /// not the (possibly lagging) frame.
    fn extent_for(&self, frame: (u32, u32)) -> (u32, u32) {
        match self.policy {
            SizePolicy::FollowFrame => frame,
            SizePolicy::FollowTarget => self.clamp_extent(self.pending_size),
        }
    }

    /// Reconfigure if the extent moved. Drops the upload texture so a frame
    /// smaller than the swapchain leaves a transparent remainder rather than
    /// stale pixels.
    fn reconfigure_to(&mut self, cw: u32, ch: u32) {
        if (self.config.width, self.config.height) == (cw, ch) {
            return;
        }
        self.config.width = cw;
        self.config.height = ch;
        self.ctx.configure_surface(&self.surface, &self.config);
        self.upload = None;
        if self.policy == SizePolicy::FollowFrame {
            self.pending_size = (cw, ch);
        }
    }

    /// The frame's extent in texels, rejecting anything the device cannot hold.
    fn frame_extent(&self, size: PhysicalSize) -> Result<(u32, u32), SurfaceLost> {
        let (w, h) = texels(size).ok_or(Kind::BadDimensions(size))?;
        let max = self.ctx.device.limits().max_texture_dimension_2d;
        if w > max || h > max {
            return Err(Kind::BadDimensions(size).into());
        }
        Ok((w, h))
    }

    fn present_pixels(
        &mut self,
        frame: Pixels<'_>,
        on_present: impl FnOnce(),
    ) -> Result<Presented, SurfaceLost> {
        let (fw, fh) = self.frame_extent(frame.size)?;
        if !self.visible {
            return Ok(Presented::Skipped);
        }

        let (cw, ch) = self.extent_for((fw, fh));
        self.reconfigure_to(cw, ch);

        // Upload matches the swapchain, so the fullscreen quad is always 1:1.
        self.ensure_upload(cw, ch);
        let Some(upload) = self.upload.as_mut() else {
            return Ok(Presented::Skipped);
        };
        upload.write(&self.ctx.queue, &frame, cw, ch);

        let Some(upload) = self.upload.as_ref() else {
            return Ok(Presented::Skipped);
        };
        let bind_group = &upload.bind_group;
        self.draw_and_present(bind_group, None, None, on_present)
    }

    fn present_shared(
        &mut self,
        frame: &SharedTexture,
        on_present: impl FnOnce(),
    ) -> Result<Presented, SurfaceLost> {
        let (fw, fh) = self.frame_extent(frame.coded())?;
        if !self.visible {
            return Ok(Presented::Skipped);
        }

        let (cw, ch) = self.extent_for((fw, fh));
        self.reconfigure_to(cw, ch);

        // FollowTarget: the imported frame texture is frame-sized; render it 1:1
        // into the top-left of the (window-sized) swapchain via the viewport, so
        // a size mismatch during resize is a transparent strip / crop, not a
        // stretch. FollowFrame draws fullscreen (swapchain == frame).
        let viewport = match self.policy {
            SizePolicy::FollowFrame => None,
            SizePolicy::FollowTarget => Some((0.0, 0.0, fw.min(cw) as f32, fh.min(ch) as f32)),
        };

        // A failed import is not a lost surface: a shared frame has no CPU
        // pixels, so there is nowhere to degrade to. Drop it and keep the last
        // good frame on screen.
        let imported = unsafe { crate::shared_import::import(&self.ctx.device, frame) };
        let (texture, image) = match imported {
            Ok(pair) => pair,
            Err(e) => {
                tracing::warn!(error = %e, "gpu_paint: shared-texture import failed");
                return Ok(Presented::Skipped);
            }
        };
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

        self.draw_and_present(&bind_group, Some(image), viewport, on_present)
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
    ) -> Result<Presented, SurfaceLost> {
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
                Lost | Outdated | Timeout | Occluded => return Ok(Presented::Skipped),
                Validation => return Err(Kind::Acquire("validation").into()),
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
            crate::shared_import::acquire_barrier(&self.ctx.device, &mut acquire_encoder, image);
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
        Ok(Presented::Yes)
    }
}

/// A physical size as texels, or `None` when it is not a positive extent.
/// Sizes cross the ABI as `c_int` because that is what the window systems and
/// CEF use; wgpu wants unsigned, and a non-positive one is never presentable.
fn texels(size: PhysicalSize) -> Option<(u32, u32)> {
    match (u32::try_from(size.w).ok()?, u32::try_from(size.h).ok()?) {
        (0, _) | (_, 0) => None,
        wh => Some(wh),
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
    frame: &Pixels<'_>,
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
) -> Result<wgpu::Surface<'static>, SurfaceLost> {
    let (display, window) = match target {
        WindowTarget::Xcb {
            connection,
            window,
            screen,
            visual,
        } => {
            let display = XcbDisplayHandle::new(Some(connection.cast()), screen);
            let mut wh =
                XcbWindowHandle::new(NonZeroU32::new(window).ok_or(Kind::SurfaceUnsupported)?);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::c_void;
    use std::ptr::NonNull;

    fn dangling() -> NonNull<c_void> {
        NonNull::dangling()
    }

    // The policy is a fact about the window system, not a caller preference:
    // an X11 swapchain IS the window drawable, so it must track the extent its
    // geometry owner set; a Wayland one is rescaled by `wp_viewport`, so it
    // tracks the producer and stays 1:1.
    #[test]
    fn size_policy_follows_the_window_target() {
        let xcb = WindowTarget::Xcb {
            connection: dangling(),
            window: 1,
            screen: 0,
            visual: 0,
        };
        let wl = WindowTarget::Wayland {
            display: dangling(),
            surface: dangling(),
        };
        assert_eq!(SizePolicy::for_target(&xcb), SizePolicy::FollowTarget);
        assert_eq!(SizePolicy::for_target(&wl), SizePolicy::FollowFrame);
    }

    #[test]
    fn clip_rect_clamps_negative_origin() {
        assert_eq!(clip_rect(-2, -2, 4, 4, 10, 10), (0, 0, 2, 2));
    }

    #[test]
    fn clip_rect_clamps_overflow() {
        assert_eq!(clip_rect(8, 8, 10, 10, 10, 10), (8, 8, 2, 2));
    }

    #[test]
    fn clip_rect_passes_through_in_bounds() {
        assert_eq!(clip_rect(1, 2, 3, 4, 10, 10), (1, 2, 3, 4));
    }

    #[test]
    fn clip_rect_collapses_fully_off_frame() {
        assert_eq!(clip_rect(10, 0, 4, 4, 10, 10), (9, 0, 0, 4));
        assert_eq!(clip_rect(0, 10, 4, 4, 10, 10), (0, 9, 4, 0));
    }
}
