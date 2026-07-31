//! Wayland-backend `Platform::init` / `Platform::cleanup` body.
//!
//! Drives the per-process Wayland subsystems in order: read mpv's
//! wayland-display and -surface handles, prime the cached fullscreen,
//! wire input, bring up the core state, install mpv's close-cb
//! trampoline, init EGL, probe dmabuf support, attach the KDE palette
//! manager, start the input thread, and bring up the clipboard reader.

use std::ffi::c_void;

use jfn_linux_util::egl_dyn as egl;

// =====================================================================
// FFI declarations consumed during init/cleanup.
// =====================================================================

use jfn_linux_util::dmabuf_probe::jfn_wl_dmabuf_probe;

// =====================================================================
// Helpers
// =====================================================================

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
    display: egl::EGLDisplay,
}

impl ProbeDisplay<'_> {
    fn init(egl: &egl::Egl, native: egl::NativeDisplayType) -> Option<ProbeDisplay<'_>> {
        let display = unsafe { (egl.get_display)(native) };
        if display.is_null() {
            return None;
        }
        let mut major = 0;
        let mut minor = 0;
        if unsafe { (egl.initialize)(display, &mut major, &mut minor) } != egl::TRUE {
            return None;
        }
        Some(ProbeDisplay { egl, display })
    }
}

impl Drop for ProbeDisplay<'_> {
    fn drop(&mut self) {
        unsafe { (self.egl.terminate)(self.display) };
    }
}

fn dmabuf_available(native_display: *mut c_void) -> bool {
    let Ok(egl) = egl::Egl::load_default() else {
        return false;
    };
    let Some(display) = ProbeDisplay::init(&egl, native_display.cast()) else {
        return false;
    };
    unsafe { jfn_wl_dmabuf_probe(c"wayland".as_ptr(), display.display) }
}

// =====================================================================
// init / cleanup
// =====================================================================

pub(crate) fn init(rt: &'static crate::runtime::WlRuntime) -> bool {
    let Some(display) = crate::app_conn::app_display(rt) else {
        tracing::error!("Failed to get app Wayland display");
        return false;
    };
    let display = display.as_ptr();

    // Prepare the input layer first so its xkb context is ready before
    // any seat_caps wires up keyboard listeners that need xkb.
    crate::input_lifecycle::lifecycle_init(rt, display);

    let mut core = match unsafe { crate::wl_state::init(rt, display) } {
        Ok(state) => state,
        Err(e) => {
            tracing::error!("wayland core init failed: {e}");
            return false;
        }
    };

    // Seed Rust state with mpv's current fullscreen — first configure
    // after this point won't start a spurious transition.
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
            jfn_platform_abi::get().set_shared_texture_unsupported();
        }
        Req::Gpu => {
            tracing::info!("paint: Vulkan WSI pixel-upload");
            jfn_platform_abi::get().set_shared_texture_unsupported();
            want_gpu_paint = true;
            resolved = Req::Gpu;
        }
        Req::Dmabuf => {
            if dmabuf_available(display) {
                tracing::info!("paint: EGL/GBM dmabuf shared texture");
                resolved = Req::Dmabuf;
            } else {
                tracing::info!("paint: EGL dmabuf unavailable; trying gpu");
                jfn_platform_abi::get().set_shared_texture_unsupported();
                want_gpu_paint = true;
                resolved = Req::Gpu;
            }
        }
    }

    if want_gpu_paint {
        match jfn_gpu_paint::Surfaces::init(None, None) {
            Some(gpu) => core.install_gpu_paint(Box::leak(Box::new(gpu))),
            None => {
                tracing::info!("paint: no usable GPU device; using wl_shm");
                resolved = Req::Shm;
            }
        }
    }

    if rt.set_core(core).is_err() {
        tracing::error!("wayland core already initialised");
        return false;
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

    rt.clipboard().init();
    if !rt.clipboard().available() {
        jfn_platform_abi::get().clear_clipboard_handler();
    }

    true
}

pub(crate) fn cleanup(rt: &'static crate::runtime::WlRuntime) {
    // KDE palette: KWin atomically drops the palette object with the
    // window. The scheme file is unlinked separately via
    // kde_palette::post_window_cleanup after mpv tears down the surface.
    jfn_linux_util::idle_inhibit::cleanup();
    rt.clipboard().cleanup();
    // Stop the app-owned toplevel thread before mpv's VO-teardown roundtrip;
    // otherwise it holds a wl_display read barrier and the roundtrip hangs when
    // no video ever played (a quiet display never wakes its poll).
    crate::root_window::cleanup(rt);
    crate::input_lifecycle::lifecycle_cleanup(rt);
    // Rust-side WlState lives until process exit (mirrors C++ globals).
}
