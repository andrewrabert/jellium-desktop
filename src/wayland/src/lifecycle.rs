use std::ffi::c_void;

use jfn_linux_util::egl;

use jfn_linux_util::dmabuf_probe::jfn_wl_dmabuf_probe;

fn paint_name(mode: crate::paint_override::WlPaintOverride) -> &'static str {
    use crate::paint_override::WlPaintOverride as M;
    match mode {
        M::Dmabuf => "dmabuf",
        M::Gpu => "gpu",
        M::Shm => "shm",
    }
}

struct ProbeDisplay<'a> {
    egl: &'a egl::Egl,
    display: egl::Display,
}

impl ProbeDisplay<'_> {
    fn init(egl: &egl::Egl, native: egl::NativeDisplayType) -> Option<ProbeDisplay<'_>> {
        let display = unsafe { egl.get_display(native) }?;
        egl.initialize(display).ok()?;
        Some(ProbeDisplay { egl, display })
    }
}

impl Drop for ProbeDisplay<'_> {
    fn drop(&mut self) {
        let _ = self.egl.terminate(self.display);
    }
}

fn dmabuf_available(native_display: *mut c_void) -> bool {
    let Ok(egl) = egl::load() else {
        return false;
    };
    let Some(probe) = ProbeDisplay::init(&egl, native_display.cast()) else {
        return false;
    };
    unsafe { jfn_wl_dmabuf_probe(c"wayland".as_ptr(), probe.display.as_ptr()) }
}

pub(crate) fn init(
    rt: &'static crate::runtime::WlRuntime,
) -> Result<(), jfn_platform_abi::PlatformInitError> {
    use jfn_platform_abi::PlatformInitError as Error;
    let Some(display) = crate::app_conn::app_display(rt) else {
        return Err(Error::backend(
            "Wayland display acquisition",
            "app display unavailable",
        ));
    };
    let display = display.as_ptr();

    crate::input_lifecycle::lifecycle_init(rt, display);

    let mut core = match unsafe { crate::wl_state::init(rt, display) } {
        Ok(state) => state,
        Err(e) => {
            return Err(Error::backend("Wayland core initialization", e));
        }
    };

    core.was_fullscreen = jfn_playback::ingest_driver::jfn_playback_fullscreen();

    use crate::paint_override::WlPaintOverride as Req;
    let requested = rt.paint_request();
    let explicit = requested.is_some();
    let entry = requested.unwrap_or(Req::Dmabuf);

    let mut want_gpu_paint = false;
    let mut resolved = Req::Shm;
    match entry {
        Req::Shm => {
            tracing::info!("paint: using wl_shm");
            unsafe { jfn_platform_abi::get() }.set_shared_texture_unsupported();
        }
        Req::Gpu => {
            tracing::info!("paint: Vulkan WSI pixel-upload");
            unsafe { jfn_platform_abi::get() }.set_shared_texture_unsupported();
            want_gpu_paint = true;
            resolved = Req::Gpu;
        }
        Req::Dmabuf => {
            if dmabuf_available(display) {
                tracing::info!("paint: EGL/GBM dmabuf shared texture");
                resolved = Req::Dmabuf;
            } else {
                tracing::info!("paint: EGL dmabuf unavailable; trying gpu");
                unsafe { jfn_platform_abi::get() }.set_shared_texture_unsupported();
                want_gpu_paint = true;
                resolved = Req::Gpu;
            }
        }
    }

    if want_gpu_paint {
        match jfn_gpu_paint::Surfaces::init(None) {
            Some(gpu) => core.install_gpu_paint(gpu),
            None => {
                tracing::info!("paint: no usable GPU device; using wl_shm");
                resolved = Req::Shm;
            }
        }
    }

    if rt.set_core(core).is_err() {
        return Err(Error::backend(
            "Wayland core installation",
            "already initialized",
        ));
    }

    if explicit
        && let Some(req) = requested
        && req != resolved
    {
        tracing::warn!(
            "--platform-paint={} unavailable; using {}",
            paint_name(req),
            paint_name(resolved)
        );
    }

    #[cfg(feature = "kde-palette")]
    crate::kde_palette::init(rt);

    jfn_platform_abi::MenuHost::warm(rt.menu());

    Ok(())
}

pub(crate) fn cleanup(rt: &'static crate::runtime::WlRuntime) {
    jfn_linux_util::idle_inhibit::cleanup();
    rt.selections().cleanup();
    if let Some(menu) = rt.try_menu() {
        jfn_platform_abi::MenuHost::shutdown(menu);
    }
    crate::root_window::cleanup(rt);
    crate::input_lifecycle::lifecycle_cleanup(rt);
}
