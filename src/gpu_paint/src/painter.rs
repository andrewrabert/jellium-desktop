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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SizePolicy {
    FollowFrame,
    FollowTarget,
}

impl SizePolicy {
    const fn for_target(target: &WindowTarget) -> Self {
        match target {
            WindowTarget::Xcb { .. } => Self::FollowTarget,
            WindowTarget::Wayland { .. } => Self::FollowFrame,
            WindowTarget::CompositionVisual { .. } => Self::FollowFrame,
            WindowTarget::CoreAnimationLayer { .. } => Self::FollowTarget,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum AlphaSource {
    Premultiplied,
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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PresentPolicy {
    Fifo,
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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ConfigureSite {
    Painter,
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
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    importer: Importer,
    upload: Option<UploadTexture>,
    pending_size: (u32, u32),
    needs_configure: bool,
    policy: SizePolicy,
    alpha: AlphaSource,
    configure_site: ConfigureSite,
    #[cfg(target_os = "macos")]
    metal_layer: MetalLayer,
    mode: Option<PaintMode>,
    shared_bind: Option<wgpu::BindGroup>,
    configured: Cell<bool>,
}

#[cfg(target_os = "macos")]
struct MetalLayer(Option<std::ptr::NonNull<std::ffi::c_void>>);

#[cfg(target_os = "macos")]
unsafe impl Send for MetalLayer {}

struct UploadTexture {
    tex: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    w: u32,
    h: u32,
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
        painter.configure_now();
        Ok(painter)
    }

    fn configure_now(&self) {
        self.ctx.configure_surface(&self.surface, &self.config);
        self.configured.set(true);
        self.after_configure();
    }

    pub fn take_configured(&self) -> bool {
        self.configured.replace(false)
    }

    #[cfg(target_os = "macos")]
    fn after_configure(&self) {
        let Some(layer) = self.metal_layer.0 else {
            return;
        };
        let layer = layer.as_ptr().cast::<objc2::runtime::AnyObject>();
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

    pub fn resize(&mut self, size: FrameSize) {
        let Some(size) = texels(size) else { return };
        self.pending_size = size;
        if self.configure_site == ConfigureSite::Owner {
            let (w, h) = self.clamp_extent(size);
            self.reconfigure_to(w, h);
        }
    }

    pub fn content_detached(&mut self) {
        self.needs_configure = true;
    }

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

    fn frame_extent(&self, size: FrameSize) -> Result<(u32, u32), SurfaceLost> {
        let (w, h) = texels(size).ok_or(Kind::BadDimensions(size))?;
        let max = self.ctx.max_texture_dim;
        if w > max || h > max {
            return Err(Kind::BadDimensions(size).into());
        }
        Ok((w, h))
    }

    pub fn present_pixels(
        &mut self,
        frame: Pixels<'_>,
        on_present: impl FnOnce(),
    ) -> Result<Presented, PresentFailed> {
        self.latch(PaintMode::Copied)?;
        let (fw, fh) = self.frame_extent(frame.size)?;
        check_buffer(&frame, fw, fh)?;

        let (cw, ch) = self.extent_for((fw, fh));

        let mut upload = self.take_upload(cw, ch);
        upload.write(&self.ctx.queue, &frame, cw, ch);
        let bind_group = upload.bind_group.clone();
        self.upload = Some(upload);
        self.draw_and_present(&bind_group, None, None, on_present)
    }

    pub fn present_shared(
        &mut self,
        frame: &SharedTexture,
        on_present: impl FnOnce(),
    ) -> Result<Presented, PresentFailed> {
        self.latch(PaintMode::Shared)?;
        let (fw, fh) = self.frame_extent(frame.coded())?;

        let (cw, ch) = self.extent_for((fw, fh));

        let viewport = match self.policy {
            SizePolicy::FollowFrame => None,
            SizePolicy::FollowTarget => Some((0.0, 0.0, fw.min(cw) as f32, fh.min(ch) as f32)),
        };

        let imported = match self.importer.import(&self.ctx.device, frame) {
            Ok(imported) => imported,
            Err(e) => {
                tracing::warn!("gpu_paint: {e}");
                return Err(PresentFailed::Import);
            }
        };
        let bind_group = match self.shared_bind.take() {
            Some(bind_group) if imported.reused => bind_group,
            _ => bind_texture(self.ctx, "jfn_gpu_paint shared bg", &imported.texture),
        };
        let result = self.draw_and_present(&bind_group, imported.acquire, viewport, on_present);
        self.shared_bind = Some(bind_group);
        result
    }

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
                Suboptimal(_) | Lost | Outdated if self.configure_site == ConfigureSite::Owner => {
                    return Err(PresentFailed::Deferred(Deferred::new()));
                }
                Suboptimal(frame) => {
                    return Ok(AcquiredFrame {
                        frame,
                        suboptimal: true,
                    });
                }
                Lost | Outdated if !reconfigured => {
                    reconfigured = true;
                    self.configure_now();
                }
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
            if let Some((x, y, w, h)) = viewport
                && w > 0.0
                && h > 0.0
            {
                pass.set_viewport(x, y, w, h, 0.0, 1.0);
            }
            pass.draw(0..3, 0..1);
        }
        self.ctx.queue.submit(std::iter::once(encoder.finish()));
        drop(gate);
        on_present();
        frame.present();
        if suboptimal {
            self.configure_now();
        }
        Ok(Presented::issued())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PresentFailed {
    #[error(transparent)]
    Lost(#[from] SurfaceLost),
    #[error("swapchain deferred the frame")]
    Deferred(Deferred),
    #[error("shared frame import failed")]
    Import,
    #[error("frame kind changed on a live surface")]
    Kind,
}

struct AcquiredFrame {
    frame: wgpu::SurfaceTexture,
    suboptimal: bool,
}

const fn configure_needed(resized: bool, detached: bool) -> bool {
    resized || detached
}

pub(crate) fn texels(size: FrameSize) -> Option<(u32, u32)> {
    match (u32::try_from(size.w).ok()?, u32::try_from(size.h).ok()?) {
        (0, _) | (_, 0) => None,
        wh => Some(wh),
    }
}

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
