//! Resolved X11 paint tier (dmabuf → gpu → shm).
//!
//! [`PaintTier::resolve`] creates the app's Vulkan instance, so it must run
//! before [`crate::mpv_proxy::start`] repoints `DISPLAY`: NVIDIA's Vulkan ICD
//! does a lazy, one-time global init on first `vkCreateInstance` that includes
//! an internal `XOpenDisplay`. Doing that here keeps the ICD's connection on
//! the real server (not the proxy) and — because it also completes before mpv's
//! VO thread is spawned — wins the loader-scan race that otherwise crashes
//! NVIDIA proprietary on X11 (two threads reading a half-populated ICD dispatch
//! table). The result is stashed in [`RESOLVED`] and drained into the platform
//! state during `lifecycle::init`.

use std::sync::{Arc, OnceLock};

use jfn_gpu_paint::{Capabilities, GpuContext};

use crate::paint_override::X11PaintOverride;

/// The paint tier resolved once at startup. `None` until [`resolve_and_store`]
/// runs; [`crate::mpv_proxy::start`] asserts it is populated so a future
/// reorder that starts the proxy first fails loudly instead of resurrecting the
/// NVIDIA loader-scan crash.
static RESOLVED: OnceLock<PaintTier> = OnceLock::new();

/// The app's compositor tier, resolved down the dmabuf → gpu → shm chain.
#[derive(Clone)]
pub struct PaintTier {
    /// Shared GPU compositor. `None` when no Vulkan adapter was found, in which
    /// case surface presents fall back to SHM.
    pub gpu_ctx: Option<Arc<GpuContext>>,
    pub gpu_caps: Capabilities,
    /// When set, the dmabuf-import tier is active.
    pub use_dmabuf: bool,
}

impl PaintTier {
    /// The SHM tier: no Vulkan instance, software presents only.
    const SHM: Self = Self {
        gpu_ctx: None,
        gpu_caps: Capabilities::NONE,
        use_dmabuf: false,
    };

    /// Resolve the paint preference down the dmabuf → gpu → shm chain, where
    /// `--platform-paint` only picks the entry tier and an unusable tier
    /// degrades to the next. Creates the app's Vulkan instance on the gpu/dmabuf
    /// path — see the module docs for why the timing matters.
    fn resolve() -> Self {
        use X11PaintOverride as Req;
        let requested = crate::paint_override::paint_override();
        let want_gpu = !matches!(requested, Some(Req::Shm));
        let want_dmabuf = matches!(requested, None | Some(Req::Dmabuf));

        let (tier, resolved) = if !want_gpu {
            tracing::info!("paint: using SHM");
            (Self::SHM, Req::Shm)
        } else {
            let target = cef_producer_target();
            if !GpuContext::probe(target).gpu_available {
                tracing::info!("paint: no Vulkan adapter; using SHM");
                (Self::SHM, Req::Shm)
            } else {
                match GpuContext::new(target) {
                    Ok(ctx) => {
                        let caps = ctx.capabilities();
                        // caps.dmabuf_import only proves our Vulkan side can
                        // consume; also probe CEF's producer, broken on NVIDIA
                        // proprietary X11.
                        if want_dmabuf
                            && caps.dmabuf_import
                            && caps.dmabuf_device_matched
                            && cef_dmabuf_producer_ok()
                        {
                            tracing::info!("paint: dmabuf import");
                            (tier_with(ctx, caps, true), Req::Dmabuf)
                        } else {
                            tracing::info!("paint: Vulkan pixel-upload");
                            (tier_with(ctx, caps, false), Req::Gpu)
                        }
                    }
                    Err(e) => {
                        tracing::info!("paint: Vulkan init failed: {e}; using SHM");
                        (Self::SHM, Req::Shm)
                    }
                }
            }
        };

        if let Some(req) = requested
            && req != resolved
        {
            tracing::warn!(
                "--platform-paint={} unavailable; using {}",
                paint_name(req),
                paint_name(resolved)
            );
        }
        tier
    }
}

fn tier_with(gpu_ctx: Arc<GpuContext>, gpu_caps: Capabilities, use_dmabuf: bool) -> PaintTier {
    PaintTier {
        gpu_ctx: Some(gpu_ctx),
        gpu_caps,
        use_dmabuf,
    }
}

/// Resolve the paint tier and store it. Must run before the mpv proxy repoints
/// `DISPLAY` and before mpv init — see module docs. Idempotent.
pub(crate) fn resolve_and_store() {
    let _ = RESOLVED.set(PaintTier::resolve());
}

/// The resolved paint tier, or the SHM fallback if resolution never ran.
pub(crate) fn resolved() -> PaintTier {
    RESOLVED.get().cloned().unwrap_or_else(|| {
        tracing::error!("paint tier unresolved at init; using SHM");
        PaintTier::SHM
    })
}

/// Whether the paint tier has been resolved. Used as an ordering tripwire.
pub(crate) fn is_resolved() -> bool {
    RESOLVED.get().is_some()
}

fn paint_name(mode: X11PaintOverride) -> &'static str {
    match mode {
        X11PaintOverride::Dmabuf => "dmabuf",
        X11PaintOverride::Gpu => "gpu",
        X11PaintOverride::Shm => "shm",
    }
}

fn cef_dmabuf_producer_ok() -> bool {
    unsafe {
        jfn_linux_util::dmabuf_probe::jfn_wl_dmabuf_probe(c"x11".as_ptr(), std::ptr::null_mut())
    }
}

fn cef_producer_target() -> jfn_gpu_paint::GpuTarget {
    let drm_render = unsafe {
        jfn_linux_util::dmabuf_probe::cef_render_node(c"x11".as_ptr(), std::ptr::null_mut())
    };
    jfn_gpu_paint::GpuTarget { drm_render }
}
