//! One window's swapchain, render pipeline, and persistent upload texture.
//!
//! Copied frames are uploaded via `queue.write_texture` at dirty-rect
//! granularity; shared frames are imported as a texture and sampled.

use std::cell::Cell;
use std::num::NonZeroU32;

#[cfg(target_os = "linux")]
use raw_window_handle::{
    RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle, XcbDisplayHandle,
    XcbWindowHandle,
};

use crate::context::{FORMAT, Surfaces};
use crate::error::{Kind, SurfaceLost};
use crate::shared::Importer;
use crate::types::{Deferred, PaintMode, Pixels, Presented, WindowTarget};
use crate::{FrameSize, SharedTexture};

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
pub(crate) enum PresentPolicy {
    Fifo,
    /// dx12 only: `Mailbox` maps to `Present(0, 0)`.
    Mailbox,
}

impl PresentPolicy {
    pub(crate) const fn for_target(target: &WindowTarget) -> Self {
        match target {
            WindowTarget::Xcb { .. }
            | WindowTarget::Wayland { .. }
            | WindowTarget::CoreAnimationLayer { .. } => Self::Fifo,
            WindowTarget::CompositionVisual { .. } => Self::Mailbox,
        }
    }

    pub(crate) const fn mode(self) -> wgpu::PresentMode {
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
pub(crate) enum ConfigureSite {
    /// The painter reconfigures from the present path when the extent moves or
    /// the swapchain reports itself stale.
    Painter,
    /// Only [`Surface::resize`] configures, on the owner's thread; a present
    /// that finds a stale swapchain skips the frame instead.
    Owner,
}

impl ConfigureSite {
    pub(crate) const fn for_target(target: &WindowTarget) -> Self {
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
    policy: SizePolicy,
    alpha: AlphaSource,
    configure_site: ConfigureSite,
    // The `CAMetalLayer` this surface presents to. Read only by the
    // post-configure hook.
    #[cfg(target_os = "macos")]
    metal_layer: MetalLayer,
    mode: Option<PaintMode>,
    // Bind group over the last imported shared texture. Rebuilt whenever the
    // import is not a cache hit, so it can never sample a stale texture.
    shared_bind: Option<wgpu::BindGroup>,
    // Set by every configure, drained by `take_configured`. A Cell because
    // configures also happen on `&self` paths inside `draw_and_present`.
    configured: Cell<bool>,
}

/// The surface's `CAMetalLayer` pointer, when its target has one.
///
/// A raw pointer rather than `usize` so the auto traits ask the right
/// question, answered here: the pointer is dereferenced only by
/// `after_configure`, and every configure of a [`ConfigureSite::Owner`]
/// surface — the only kind with a layer — runs on the layer's owner thread
/// (`Surface::new`, `resize`; the present path skips instead of configuring).
#[cfg(target_os = "macos")]
struct MetalLayer(Option<std::ptr::NonNull<std::ffi::c_void>>);

// SAFETY: see the type doc — off the owner thread the pointer is inert data,
// never dereferenced.
#[cfg(target_os = "macos")]
unsafe impl Send for MetalLayer {}

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
        size: FrameSize,
    ) -> Result<Self, SurfaceLost> {
        let policy = SizePolicy::for_target(&target);
        let alpha = AlphaSource::for_target(&target);
        let present = PresentPolicy::for_target(&target);
        let configure_site = ConfigureSite::for_target(&target);
        #[cfg(target_os = "macos")]
        let metal_layer = MetalLayer(match &target {
            WindowTarget::CoreAnimationLayer { layer } => Some(*layer),
            _ => None,
        });
        let extent = texels(size).ok_or(Kind::BadDimensions(size))?;
        let max = ctx.max_texture_dim;
        if extent.0 > max || extent.1 > max {
            return Err(Kind::BadDimensions(size).into());
        }

        // SAFETY: the caller of `Surfaces::new_surface` guarantees the
        // target's window/layer outlives this painter (see the `surface`
        // field note).
        let surface = unsafe { create_surface(&ctx.instance, target)? };

        if !ctx.adapter.is_surface_supported(&surface) {
            return Err(Kind::SurfaceUnsupported.into());
        }

        let caps = surface.get_capabilities(&ctx.adapter);
        let alpha_mode = pick_alpha_mode(&caps);

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: FORMAT,
            width: extent.0,
            height: extent.1,
            present_mode: present.mode(),
            desired_maximum_frame_latency: 2,
            alpha_mode,
            view_formats: vec![],
        };

        let painter = Self {
            ctx,
            surface,
            config,
            importer: Importer::new(),
            upload: None,
            pending_size: extent,
            needs_configure: false,
            policy,
            alpha,
            configure_site,
            #[cfg(target_os = "macos")]
            metal_layer,
            mode: None,
            shared_bind: None,
            configured: Cell::new(false),
        };
        // Other surfaces may be submitting on the shared device while this
        // painter is created, so the first configure must be gated too.
        painter.configure_now();
        Ok(painter)
    }

    /// Configure the swapchain and restore anything wgpu overwrote doing it.
    fn configure_now(&self) {
        self.ctx.configure_surface(&self.surface, &self.config);
        self.configured.set(true);
        self.after_configure();
    }

    /// Whether any configure ran since the last call. On dx12 a configure is
    /// also the `SetContent` that binds the swapchain to the composition
    /// visual, so this is exactly when the owner has to `Commit` — plain
    /// presents update the bound swapchain without one.
    pub fn take_configured(&self) -> bool {
        self.configured.replace(false)
    }

    /// `metal::Surface::configure` clears `allowsNextDrawableTimeout`
    /// unconditionally, and on macOS `nextDrawable` runs on the main thread —
    /// the runloop, the CEF pump and input. Unbounded there wedges the app;
    /// with the timeout back, a drawable that never arrives is a skipped
    /// frame.
    #[cfg(target_os = "macos")]
    fn after_configure(&self) {
        let Some(layer) = self.metal_layer.0 else {
            return;
        };
        let layer = layer.as_ptr().cast::<objc2::runtime::AnyObject>();
        // SAFETY: `layer` is a live `CAMetalLayer` — its owner keeps it alive
        // for the painter's lifetime — and this runs on the layer's owner
        // thread (see [`MetalLayer`]).
        unsafe {
            let _: () = objc2::msg_send![layer, setAllowsNextDrawableTimeout: true];
        }
    }

    #[cfg(not(target_os = "macos"))]
    fn after_configure(&self) {}

    fn clamp_extent(&self, size: (u32, u32)) -> (u32, u32) {
        let max = self.ctx.max_texture_dim.max(1);
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
    pub fn resize(&mut self, size: FrameSize) {
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

    /// Latch this surface's frame kind, or reject a frame of the other one.
    ///
    /// A surface latches its kind from the first frame it presents and will not
    /// take the other kind afterwards: `Copied` maintains a persistent upload
    /// texture that a `Shared` frame would leave stale, and the next dirty-only
    /// frame would then patch onto a base two frames old. CEF fixes the kind
    /// per browser via `shared_texture_enabled`, so a mismatch means something
    /// upstream is wrong, not that this surface is lost — the producer owes the
    /// successor.
    fn latch(&mut self, mode: PaintMode) -> Result<(), PresentFailed> {
        match self.mode {
            Some(latched) if latched != mode => Err(PresentFailed::Kind),
            Some(_) => Ok(()),
            None => {
                self.mode = Some(mode);
                Ok(())
            }
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
    fn frame_extent(&self, size: FrameSize) -> Result<(u32, u32), SurfaceLost> {
        let (w, h) = texels(size).ok_or(Kind::BadDimensions(size))?;
        let max = self.ctx.max_texture_dim;
        if w > max || h > max {
            return Err(Kind::BadDimensions(size).into());
        }
        Ok((w, h))
    }

    /// Present CPU pixels, latching the copied kind on this surface's first
    /// frame.
    ///
    /// `on_present` runs between submit and present, so a caller can latch
    /// state against the frame actually being shown (Wayland sets its viewport
    /// source there) without that state applying to a frame that never
    /// commits.
    pub fn present_pixels(
        &mut self,
        frame: Pixels<'_>,
        on_present: impl FnOnce(),
    ) -> Result<Presented, PresentFailed> {
        self.latch(PaintMode::Copied)?;
        let (fw, fh) = self.frame_extent(frame.size)?;
        check_buffer(&frame, fw, fh)?;

        let (cw, ch) = self.extent_for((fw, fh));

        // Upload matches the swapchain, so the fullscreen quad is always 1:1.
        let mut upload = self.take_upload(cw, ch);
        upload.write(&self.ctx.queue, &frame, cw, ch);
        let bind_group = upload.bind_group.clone();
        self.upload = Some(upload);
        self.draw_and_present(&bind_group, None, None, on_present)
    }

    /// Present a shared texture, latching the shared kind on this surface's
    /// first frame. `on_present` runs as for [`Surface::present_pixels`].
    pub fn present_shared(
        &mut self,
        frame: &SharedTexture,
        on_present: impl FnOnce(),
    ) -> Result<Presented, PresentFailed> {
        self.latch(PaintMode::Shared)?;
        let (fw, fh) = self.frame_extent(frame.coded())?;

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
        // pixels, so there is nowhere to degrade to. The last good frame stays
        // on screen and the producer owes the successor.
        let imported = match self.importer.import(&self.ctx.device, frame) {
            Ok(imported) => imported,
            Err(e) => {
                tracing::warn!("gpu_paint: {e}");
                return Err(PresentFailed::Import);
            }
        };
        // A reused import is the importer's cached texture, so the bind group
        // over it is reusable too; anything else must be rebound.
        let bind_group = match self.shared_bind.take() {
            Some(bind_group) if imported.reused => bind_group,
            _ => bind_texture(self.ctx, "jfn_gpu_paint shared bg", &imported.texture),
        };
        let result = self.draw_and_present(&bind_group, imported.acquire, viewport, on_present);
        self.shared_bind = Some(bind_group);
        result
    }

    // ----- internals -----

    /// The persistent upload texture at exactly `w`×`h`, reusing the current
    /// one when it matches. Taken out of `self` rather than borrowed so the
    /// caller can write to it while also borrowing `self.ctx`.
    fn take_upload(&mut self, w: u32, h: u32) -> UploadTexture {
        match self.upload.take() {
            Some(upload) if upload.w == w && upload.h == h => upload,
            _ => self.new_upload(w, h),
        }
    }

    fn new_upload(&self, w: u32, h: u32) -> UploadTexture {
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
            format: FORMAT,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let bind_group = bind_texture(self.ctx, "jfn_gpu_paint bg", &tex);
        UploadTexture {
            tex,
            bind_group,
            w,
            h,
            needs_base: true,
        }
    }

    /// Acquire the next swapchain frame with no gate held, reconfiguring once
    /// if the swapchain is stale. A swapchain with no texture this cycle is a
    /// [`PresentFailed::Deferred`], never a fault.
    fn acquire_frame(&self) -> Result<AcquiredFrame, PresentFailed> {
        use wgpu::CurrentSurfaceTexture::*;
        let mut reconfigured = false;
        loop {
            match self.surface.get_current_texture() {
                Success(frame) => {
                    return Ok(AcquiredFrame {
                        frame,
                        suboptimal: false,
                    });
                }
                // The owner configures, and it is not this thread. Skip; the
                // owner's next resize rebuilds the swapchain. On metal these
                // three are unreachable — `acquire_texture` returns only
                // success, `Timeout` or `Occluded` — so this is a guard against
                // a later edit reintroducing an off-main configure, not a live
                // path.
                Suboptimal(_) | Lost | Outdated if self.configure_site == ConfigureSite::Owner => {
                    return Err(PresentFailed::Deferred(Deferred::new()));
                }
                // The frame is usable, but the swapchain no longer matches the
                // surface; the caller rebuilds it after presenting.
                Suboptimal(frame) => {
                    return Ok(AcquiredFrame {
                        frame,
                        suboptimal: true,
                    });
                }
                // Stale swapchain (typically a resize). Reconfigure and retry
                // ONCE, presenting THIS frame — overlay content is event-driven
                // and may not repaint for a long time, so a drop leaves it stale.
                Lost | Outdated if !reconfigured => {
                    reconfigured = true;
                    self.configure_now();
                }
                // Transient (occluded, timed out, or still stale after reconfigure):
                // the surface is fine and the frame is still owed, so the caller
                // presents it again rather than degrading the backend to SHM.
                Lost | Outdated | Timeout | Occluded => {
                    return Err(PresentFailed::Deferred(Deferred::new()));
                }
                Validation => return Err(SurfaceLost::from(Kind::Acquire("validation")).into()),
            }
        }
    }

    fn draw_and_present(
        &self,
        bind_group: &wgpu::BindGroup,
        external_image: Option<u64>,
        viewport: Option<(f32, f32, f32, f32)>,
        on_present: impl FnOnce(),
    ) -> Result<Presented, PresentFailed> {
        let AcquiredFrame { frame, suboptimal } = self.acquire_frame()?;
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        // The acquire barrier must precede the render pass, in its own
        // command buffer: wgpu 29 forbids mixing raw HAL encoding
        // (`as_hal_mut`) and normal wgpu encoding on one CommandEncoder.
        let gate = self.ctx.submit_gate.read();
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
        // The gate covers the submits alone: a FIFO present blocks until the
        // compositor releases the previous frame, and nothing may be forced to
        // wait behind it.
        drop(gate);
        // Run after the early-return present failures above, so the closure's
        // surface-state updates only latch on a frame that actually presents.
        on_present();
        frame.present();
        // SUBOPTIMAL: the presented frame was fine, but rebuild the swapchain so
        // the next acquire is fresh rather than repeatedly suboptimal.
        if suboptimal {
            self.configure_now();
        }
        Ok(Presented::issued())
    }
}

/// Why a frame did not reach the compositor.
#[derive(Debug, thiserror::Error)]
pub enum PresentFailed {
    /// The surface is done; abandon it.
    #[error(transparent)]
    Lost(#[from] SurfaceLost),
    /// The swapchain had no texture this cycle. The surface is fine, the frame
    /// is still owed, and the caller presents it again at the named instant.
    #[error("swapchain deferred the frame")]
    Deferred(Deferred),
    /// The shared frame could not be imported. The surface is fine, the last
    /// frame stays on screen, and the producer owes the successor.
    #[error("shared frame import failed")]
    Import,
    /// A frame of the other kind reached a surface already latched to one. The
    /// producer owes the successor.
    #[error("frame kind changed on a live surface")]
    Kind,
}

/// One acquired swapchain frame. No gate is held across the acquire or the
/// present; the caller takes the read side around its submits alone.
struct AcquiredFrame {
    frame: wgpu::SurfaceTexture,
    /// The frame is usable but the swapchain no longer matches the surface;
    /// reconfigure after presenting.
    suboptimal: bool,
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
pub(crate) fn texels(size: FrameSize) -> Option<(u32, u32)> {
    match (u32::try_from(size.w).ok()?, u32::try_from(size.h).ok()?) {
        (0, _) | (_, 0) => None,
        wh => Some(wh),
    }
}

/// Reject a frame whose buffer cannot cover its declared extent — the one
/// invariant [`Pixels`]'s public fields cannot enforce. Every slice
/// `write_rect` takes is clipped inside `(fw, fh)`, so this single check
/// bounds them all; without it a producer's stride bug is a slice-index panic
/// on the paint thread.
fn check_buffer(frame: &Pixels<'_>, fw: u32, fh: u32) -> Result<(), SurfaceLost> {
    let stride = frame.stride as usize;
    let row = fw as usize * 4;
    let needed = (fh as usize - 1) * stride + row;
    if stride < row || frame.bgra.len() < needed {
        return Err(Kind::BadPixelBuffer {
            size: frame.size,
            stride: frame.stride,
            len: frame.bgra.len(),
        }
        .into());
    }
    Ok(())
}

/// The one bind group shape this crate draws with: the texture at binding 0,
/// the shared nearest sampler at binding 1.
fn bind_texture(ctx: &Surfaces, label: &str, texture: &wgpu::Texture) -> wgpu::BindGroup {
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some(label),
        layout: &ctx.bind_layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(&ctx.sampler),
            },
        ],
    })
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

pub(crate) fn pick_alpha_mode(caps: &wgpu::SurfaceCapabilities) -> wgpu::CompositeAlphaMode {
    use wgpu::CompositeAlphaMode::*;
    [PreMultiplied, PostMultiplied, Inherit, Opaque, Auto]
        .into_iter()
        .find(|m| caps.alpha_modes.contains(m))
        .unwrap_or(Auto)
}

/// # Safety
///
/// The window-system objects inside `target` must be live, and must outlive
/// the returned surface: the `'static` lifetime is a promise the caller
/// makes, not one wgpu can check.
pub(crate) unsafe fn create_surface(
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
    // SAFETY: forwards this function's own contract — the caller guarantees
    // the handles in `unsafe_target` are live and outlive the surface.
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
