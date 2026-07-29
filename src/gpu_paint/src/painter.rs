//! One window's swapchain, render pipeline, and persistent upload texture.
//!
//! Copied frames are uploaded via `queue.write_texture` at dirty-rect
//! granularity; shared frames are imported as a texture and sampled.

use std::num::NonZeroU32;

#[cfg(target_os = "linux")]
use raw_window_handle::{
    RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle, XcbDisplayHandle,
    XcbWindowHandle,
};

use crate::context::{SURFACE_FORMAT, Surfaces};
use crate::error::{Kind, SurfaceLost};
use crate::shared::Importer;
use crate::types::{Frame, PaintMode, Pixels, Presented, WindowTarget};
use jfn_platform_abi::{PhysicalSize, SharedTexture};

/// How a surface chooses its swapchain extent.
///
/// Derived from [`WindowTarget`], never chosen by a caller: whether the
/// swapchain *is* the window is a fact about the window system.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SizePolicy {
    /// Track each incoming frame's size. Used where another layer (Wayland's
    /// `wp_viewport`, DirectComposition's 1:1 visual) shows the buffer at the
    /// surface's logical size, so presenting at the producer's size keeps
    /// content 1:1.
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
            // DirectComposition shows the swapchain 1:1 under the visual.
            WindowTarget::CompositionVisual { .. } => Self::FollowFrame,
            // The layer is the drawable, and its owner sizes it.
            WindowTarget::CoreAnimationLayer { .. } => Self::FollowTarget,
        }
    }
}

/// What the producer's alpha means, and so whether the shader has to
/// premultiply before handing pixels to the compositor.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum AlphaSource {
    /// The surface composites premultiplied, which is what CEF delivers.
    Premultiplied,
    /// The surface has no premultiplied composite mode (metal offers only
    /// `PostMultiplied` and `Opaque`) but its compositor still expects
    /// premultiplied pixels, so the shader does it.
    Straight,
}

impl AlphaSource {
    const fn for_target(target: &WindowTarget) -> Self {
        match target {
            WindowTarget::Xcb { .. }
            | WindowTarget::Wayland { .. }
            | WindowTarget::CompositionVisual { .. } => Self::Premultiplied,
            WindowTarget::CoreAnimationLayer { .. } => Self::Straight,
        }
    }
}

/// The present mode this target's backend actually advertises.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PresentPolicy {
    Fifo,
    /// dx12 only: `Mailbox` maps to `Present(0, 0)`.
    Mailbox,
}

impl PresentPolicy {
    const fn for_target(target: &WindowTarget) -> Self {
        match target {
            WindowTarget::Xcb { .. }
            | WindowTarget::Wayland { .. }
            | WindowTarget::CoreAnimationLayer { .. } => Self::Fifo,
            WindowTarget::CompositionVisual { .. } => Self::Mailbox,
        }
    }

    const fn mode(self) -> wgpu::PresentMode {
        match self {
            Self::Fifo => wgpu::PresentMode::Fifo,
            Self::Mailbox => wgpu::PresentMode::Mailbox,
        }
    }
}

/// Who is allowed to configure the swapchain.
///
/// This exists for exactly one reason: a `configure` on metal writes the
/// `CAMetalLayer` — device, pixel format, colorspace, drawable size — and layer
/// writes belong to the layer's owner thread, which is the main thread. A
/// present that reconfigured inline would move that write to whatever thread
/// CEF painted on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ConfigureSite {
    /// The painter reconfigures from the present path when the extent moves or
    /// the swapchain reports itself stale.
    Painter,
    /// Only [`Surface::resize`] configures, on the owner's thread; a present
    /// that finds a stale swapchain skips the frame instead.
    Owner,
}

impl ConfigureSite {
    const fn for_target(target: &WindowTarget) -> Self {
        match target {
            WindowTarget::Xcb { .. }
            | WindowTarget::Wayland { .. }
            | WindowTarget::CompositionVisual { .. } => Self::Painter,
            WindowTarget::CoreAnimationLayer { .. } => Self::Owner,
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
    importer: Importer,
    // Persistent upload texture sized to the swapchain. Recreated on
    // resize. `None` until the first frame establishes a size.
    upload: Option<UploadTexture>,
    // Stored target size from the most recent `resize` call. Acts as
    // the gate: we only reconfigure (and present) once an incoming
    // frame matches it.
    pending_size: (u32, u32),
    // Set by `content_detached`: the target's owner severed the binding
    // between the swapchain and what is on screen, so the next configure must
    // happen even though the extent did not move.
    needs_configure: bool,
    visible: bool,
    policy: SizePolicy,
    alpha: AlphaSource,
    configure_site: ConfigureSite,
    // The `CAMetalLayer` this surface presents to, as an address; zero on
    // every other target. Read only by the post-configure hook.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    metal_layer: usize,
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
        let alpha = AlphaSource::for_target(&target);
        let present = PresentPolicy::for_target(&target);
        let configure_site = ConfigureSite::for_target(&target);
        let metal_layer = match &target {
            WindowTarget::CoreAnimationLayer { layer } => layer.as_ptr() as usize,
            _ => 0,
        };
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
            present_mode: present.mode(),
            desired_maximum_frame_latency: 2,
            alpha_mode,
            view_formats: vec![],
            color_space: wgpu::SurfaceColorSpace::Auto,
        };

        let painter = Self {
            ctx,
            surface,
            config,
            importer: Importer::new(),
            upload: None,
            pending_size: extent,
            needs_configure: false,
            visible: true,
            policy,
            alpha,
            configure_site,
            metal_layer,
            mode: None,
        };
        // Other surfaces may be submitting on the shared device while this
        // painter is created, so the first configure must be gated too.
        painter.configure_now();
        Ok(painter)
    }

    /// Configure the swapchain and restore anything wgpu overwrote doing it.
    fn configure_now(&self) {
        self.ctx.configure_surface(&self.surface, &self.config);
        self.after_configure();
    }

    /// `metal::Surface::configure` clears `allowsNextDrawableTimeout`
    /// unconditionally, and on macOS `nextDrawable` runs on the main thread —
    /// the runloop, the CEF pump and input. Unbounded there wedges the app;
    /// with the timeout back, a drawable that never arrives is a skipped
    /// frame.
    #[cfg(target_os = "macos")]
    fn after_configure(&self) {
        if self.metal_layer == 0 {
            return;
        }
        let layer = self.metal_layer as *mut objc2::runtime::AnyObject;
        unsafe {
            let _: () = objc2::msg_send![layer, setAllowsNextDrawableTimeout: true];
        }
    }

    #[cfg(not(target_os = "macos"))]
    fn after_configure(&self) {}

    fn clamp_extent(&self, size: (u32, u32)) -> (u32, u32) {
        let max = self.ctx.device.limits().max_texture_dimension_2d.max(1);
        (size.0.clamp(1, max), size.1.clamp(1, max))
    }

    /// Store a new target size.
    ///
    /// Under [`ConfigureSite::Painter`] this does not reconfigure the
    /// swapchain — the next matching-size present does that. Gaps during a
    /// resize are acceptable; stretching is not. Under
    /// [`ConfigureSite::Owner`] this *is* the configure, because the caller is
    /// the thread that owns the drawable; an unchanged extent still costs
    /// nothing, since the configure is skipped.
    pub fn resize(&mut self, size: PhysicalSize) {
        let Some(size) = texels(size) else { return };
        self.pending_size = size;
        if self.configure_site == ConfigureSite::Owner {
            let (w, h) = self.clamp_extent(size);
            self.reconfigure_to(w, h);
        }
    }

    /// The target's content binding was severed by its owner. Rebind — i.e.
    /// reconfigure — before the next present, whatever the extent says.
    ///
    /// On dx12 the swapchain is bound to the composition visual *inside*
    /// `configure` and nowhere else, so an owner that calls `SetContent(None)`
    /// leaves a painter whose extent is unchanged and whose content is
    /// unbound. Destroying the painter instead would mean a present-queue-idle
    /// wait on the thread that detached it.
    pub fn content_detached(&mut self) {
        self.needs_configure = true;
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

    /// The swapchain extent to draw this frame into, reconfiguring first where
    /// the painter is the one allowed to.
    ///
    /// `FollowFrame` tracks the producer's size (another layer shows it 1:1);
    /// `FollowTarget` tracks the parent-derived window size set through
    /// `resize` — the swapchain IS the window drawable, so it must match the
    /// window its geometry owner sized, not the (possibly lagging) frame.
    /// Under [`ConfigureSite::Owner`] neither applies here: the extent is
    /// whatever the owner last configured.
    fn extent_for(&mut self, frame: (u32, u32)) -> (u32, u32) {
        if self.configure_site == ConfigureSite::Painter {
            let (cw, ch) = match self.policy {
                SizePolicy::FollowFrame => frame,
                SizePolicy::FollowTarget => self.clamp_extent(self.pending_size),
            };
            self.reconfigure_to(cw, ch);
        }
        (self.config.width, self.config.height)
    }

    /// Reconfigure if the extent moved or the content binding was severed. An
    /// extent change drops the upload texture so a frame smaller than the
    /// swapchain leaves a transparent remainder rather than stale pixels.
    fn reconfigure_to(&mut self, cw: u32, ch: u32) {
        let resized = (self.config.width, self.config.height) != (cw, ch);
        if !configure_needed(resized, self.needs_configure) {
            return;
        }
        self.config.width = cw;
        self.config.height = ch;
        self.needs_configure = false;
        self.configure_now();
        if resized {
            self.upload = None;
            if self.policy == SizePolicy::FollowFrame {
                self.pending_size = (cw, ch);
            }
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
        let imported = match self.importer.import(&self.ctx.device, frame) {
            Ok(imported) => imported,
            Err(e) => {
                tracing::warn!("gpu_paint: {e}");
                return Ok(Presented::Skipped);
            }
        };
        let view = imported
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = self
            .ctx
            .device
            .create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("jfn_gpu_paint shared bg"),
                layout: &self.ctx.bind_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&self.ctx.sampler),
                    },
                ],
            });

        self.draw_and_present(&bind_group, imported.acquire, viewport, on_present)
    }

    /// Drop anything the importer is holding across frames. Touches no
    /// swapchain, no drawable and no configure — only the imported textures,
    /// which wgpu keeps alive until the submissions using them retire.
    pub fn clear_import_cache(&mut self) {
        self.importer.clear_cache();
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
                    layout: &self.ctx.bind_layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(&view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::Sampler(&self.ctx.sampler),
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
                // The owner configures, and it is not this thread. Skip; the
                // owner's next resize rebuilds the swapchain. On metal these
                // three are unreachable — `acquire_texture` returns only
                // success, `Timeout` or `Occluded` — so this is a guard against
                // a later edit reintroducing an off-main configure, not a live
                // path.
                Suboptimal(_) | Lost | Outdated if self.configure_site == ConfigureSite::Owner => {
                    return Ok(Presented::Skipped);
                }
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
                    self.configure_now();
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
                        label: Some("jfn_gpu_paint shared acquire enc"),
                    });
            crate::shared::acquire_barrier(&self.ctx.device, &mut acquire_encoder, image);
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
            pass.set_pipeline(self.ctx.pipeline(self.alpha));
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
            self.configure_now();
        }
        Ok(Presented::Yes)
    }
}

/// Whether the swapchain has to be configured.
///
/// A detached target is the whole reason this is not just `resized`: on dx12
/// the swapchain is bound to the visual inside `configure` and nowhere else, so
/// a painter whose extent never moved still has to configure to get back on
/// screen.
const fn configure_needed(resized: bool, detached: bool) -> bool {
    resized || detached
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
    let unsafe_target = match target {
        #[cfg(target_os = "linux")]
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
            wgpu::SurfaceTargetUnsafe::RawHandle {
                raw_display_handle: Some(RawDisplayHandle::Xcb(display)),
                raw_window_handle: RawWindowHandle::Xcb(wh),
            }
        }
        #[cfg(target_os = "linux")]
        WindowTarget::Wayland { display, surface } => {
            let dh = WaylandDisplayHandle::new(display);
            let wh = WaylandWindowHandle::new(surface);
            wgpu::SurfaceTargetUnsafe::RawHandle {
                raw_display_handle: Some(RawDisplayHandle::Wayland(dh)),
                raw_window_handle: RawWindowHandle::Wayland(wh),
            }
        }
        #[cfg(windows)]
        WindowTarget::CompositionVisual { visual } => {
            wgpu::SurfaceTargetUnsafe::CompositionVisual(visual.as_ptr())
        }
        #[cfg(target_os = "macos")]
        WindowTarget::CoreAnimationLayer { layer } => {
            wgpu::SurfaceTargetUnsafe::CoreAnimationLayer(layer.as_ptr())
        }
        // A target belonging to a window system this build cannot present to.
        _ => return Err(Kind::SurfaceUnsupported.into()),
    };
    let surface = unsafe { instance.create_surface_unsafe(unsafe_target)? };
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

    fn xcb() -> WindowTarget {
        WindowTarget::Xcb {
            connection: dangling(),
            window: 1,
            screen: 0,
            visual: 0,
        }
    }

    fn wayland() -> WindowTarget {
        WindowTarget::Wayland {
            display: dangling(),
            surface: dangling(),
        }
    }

    fn composition_visual() -> WindowTarget {
        WindowTarget::CompositionVisual { visual: dangling() }
    }

    fn core_animation_layer() -> WindowTarget {
        WindowTarget::CoreAnimationLayer { layer: dangling() }
    }

    #[test]
    fn size_policy_follows_the_window_target() {
        assert_eq!(SizePolicy::for_target(&xcb()), SizePolicy::FollowTarget);
        assert_eq!(SizePolicy::for_target(&wayland()), SizePolicy::FollowFrame);
        assert_eq!(
            SizePolicy::for_target(&composition_visual()),
            SizePolicy::FollowFrame
        );
        assert_eq!(
            SizePolicy::for_target(&core_animation_layer()),
            SizePolicy::FollowTarget
        );
    }

    #[test]
    fn alpha_source_follows_the_window_target() {
        assert_eq!(AlphaSource::for_target(&xcb()), AlphaSource::Premultiplied);
        assert_eq!(
            AlphaSource::for_target(&wayland()),
            AlphaSource::Premultiplied
        );
        assert_eq!(
            AlphaSource::for_target(&composition_visual()),
            AlphaSource::Premultiplied
        );
        assert_eq!(
            AlphaSource::for_target(&core_animation_layer()),
            AlphaSource::Straight
        );
    }

    #[test]
    fn present_policy_follows_the_window_target() {
        assert_eq!(PresentPolicy::for_target(&xcb()), PresentPolicy::Fifo);
        assert_eq!(PresentPolicy::for_target(&wayland()), PresentPolicy::Fifo);
        assert_eq!(
            PresentPolicy::for_target(&composition_visual()),
            PresentPolicy::Mailbox
        );
        assert_eq!(
            PresentPolicy::for_target(&core_animation_layer()),
            PresentPolicy::Fifo
        );
    }

    #[test]
    fn configure_site_follows_the_window_target() {
        assert_eq!(ConfigureSite::for_target(&xcb()), ConfigureSite::Painter);
        assert_eq!(
            ConfigureSite::for_target(&wayland()),
            ConfigureSite::Painter
        );
        assert_eq!(
            ConfigureSite::for_target(&composition_visual()),
            ConfigureSite::Painter
        );
        assert_eq!(
            ConfigureSite::for_target(&core_animation_layer()),
            ConfigureSite::Owner
        );
    }

    #[test]
    fn content_detached_forces_a_configure_at_an_unchanged_extent() {
        assert!(!configure_needed(false, false));
        assert!(configure_needed(false, true));
        assert!(configure_needed(true, false));
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
