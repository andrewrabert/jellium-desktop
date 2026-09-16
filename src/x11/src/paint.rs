use std::sync::OnceLock;

use jfn_gpu_paint::Surfaces;

use crate::paint_override::X11PaintOverride;

static RESOLVED: OnceLock<PaintTier> = OnceLock::new();

pub(crate) struct PaintTier {
    pub use_dmabuf: bool,
}

impl PaintTier {
    const SHM: Self = Self { use_dmabuf: false };

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

pub(crate) fn resolve_and_store() {
    let _ = RESOLVED.set(PaintTier::resolve());
}

pub(crate) fn resolved() -> Option<&'static PaintTier> {
    RESOLVED.get()
}

pub(crate) fn gpu() -> Option<&'static Surfaces> {
    jfn_gpu_paint::surfaces()
}

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
