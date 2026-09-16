//! Resolved X11 paint tier (dmabuf → gpu → shm).
//!
//! [`resolve_and_store`] opens the app's GPU device, so it must run before
//! [`crate::mpv_proxy::start`] repoints `DISPLAY`: NVIDIA's Vulkan ICD does a
//! lazy, one-time global init on first `vkCreateInstance` that includes an
//! internal `XOpenDisplay`. Doing that here keeps the ICD's connection on the
//! real server (not the proxy) and — because it also completes before mpv's VO
//! thread is spawned — wins the loader-scan race that otherwise crashes NVIDIA
//! proprietary on X11 (two threads reading a half-populated ICD dispatch
//! table). The result is stashed in [`RESOLVED`] and drained into the platform
//! state during `lifecycle::init`.

use std::sync::OnceLock;

use jfn_gpu_paint::Surfaces;

use crate::paint_override::X11PaintOverride;

/// The paint tier resolved once at startup. `None` until [`resolve_and_store`]
/// runs; [`crate::mpv_proxy::start`] asserts it is populated so a future
/// reorder that starts the proxy first fails loudly instead of resurrecting the
/// NVIDIA loader-scan crash.
static RESOLVED: OnceLock<PaintTier> = OnceLock::new();

/// The app's compositor tier, resolved down the dmabuf → gpu → shm chain.
pub(crate) struct PaintTier {
    /// Whether CEF should produce shared textures: both halves of the question
    /// answered yes.
    pub use_dmabuf: bool,
}

impl PaintTier {
    /// The SHM tier: software presents only.
    const SHM: Self = Self { use_dmabuf: false };

    /// Resolve the paint preference down the dmabuf → gpu → shm chain, where
    /// `--platform-paint` only picks the entry tier and an unusable tier
    /// degrades to the next. Opens the GPU device on every tier, SHM included:
    /// this is the last point before the mpv proxy repoints `DISPLAY` — see the
    /// module docs — and the shell overlay needs a device whatever CEF's frames
    /// travel over.
    fn resolve() -> Self {
        use X11PaintOverride as Req;
        let requested = crate::paint_override::paint_override();
        let want_gpu = !matches!(requested, Some(Req::Shm));
        let want_dmabuf = matches!(requested, None | Some(Req::Dmabuf));

        let producer = unsafe {
            jfn_linux_util::dmabuf_probe::cef_render_node(c"x11".as_ptr(), std::ptr::null_mut())
        };
        let gpu = Surfaces::init(producer);

        let (tier, resolved) = match gpu {
            _ if !want_gpu => {
                tracing::info!("paint: using SHM");
                (Self::SHM, Req::Shm)
            }
            None => {
                tracing::info!("paint: no usable GPU device; using SHM");
                (Self::SHM, Req::Shm)
            }
            Some(gpu) => {
                // Two independent halves. `can_import_shared` proves only
                // that our device can consume; CEF's producer must also
                // work, and it is broken on NVIDIA proprietary X11.
                let use_dmabuf = want_dmabuf && gpu.can_import_shared() && cef_dmabuf_producer_ok();
                if use_dmabuf {
                    tracing::info!("paint: dmabuf import");
                } else {
                    tracing::info!("paint: GPU pixel-upload");
                }
                let entry = if use_dmabuf { Req::Dmabuf } else { Req::Gpu };
                (Self { use_dmabuf }, entry)
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

/// Resolve the paint tier and store it. Must run before the mpv proxy repoints
/// `DISPLAY` and before mpv init — see module docs. Idempotent.
pub(crate) fn resolve_and_store() {
    let _ = RESOLVED.set(PaintTier::resolve());
}

/// The resolved paint tier. `'static` because it is set once and never
/// replaced, which is what lets a [`jfn_gpu_paint::Surface`] borrow the device
/// for its whole life.
pub(crate) fn resolved() -> Option<&'static PaintTier> {
    RESOLVED.get()
}

/// The process's GPU device, if one opened.
pub(crate) fn gpu() -> Option<&'static Surfaces> {
    jfn_gpu_paint::surfaces()
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
