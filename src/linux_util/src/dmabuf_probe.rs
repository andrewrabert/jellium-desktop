//! Test whether the GPU stack can import a GBM-allocated dmabuf as an EGL
//! image and bind it to a GL texture. Run once during Wayland init to decide
//! whether CEF's shared-texture path will work; if not, we fall back to
//! software CEF rendering.
//!
//! libEGL, libX11, and libgbm are all dlopened so the binary keeps no link
//! dependency on them (the X11 case only fires when CEF runs under
//! `--ozone-platform=x11` over XWayland).

use crate::egl;
use drm_fourcc::DrmFourcc;
use libloading::Library;
use nix::fcntl::{OFlag, open};
use nix::sys::stat::Mode;
use std::ffi::{CStr, CString, c_char, c_int, c_uint, c_void};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::ptr;

// GL is two constants and five entry points reached through
// `eglGetProcAddress`; `glow` needs a loader context to give anything back.
const GL_TEXTURE_2D: c_uint = 0x0DE1;
const GL_NO_ERROR: c_uint = 0;
const GBM_BO_USE_RENDERING: u32 = 0x0002;

const EGL_PLATFORM_X11_KHR: egl::Enum = 0x31D5;
const EGL_LINUX_DMA_BUF_EXT: egl::Enum = 0x3270;
const EGL_LINUX_DRM_FOURCC_EXT: egl::Int = 0x3271;
const EGL_DMA_BUF_PLANE0_FD_EXT: egl::Int = 0x3272;
const EGL_DMA_BUF_PLANE0_OFFSET_EXT: egl::Int = 0x3273;
const EGL_DMA_BUF_PLANE0_PITCH_EXT: egl::Int = 0x3274;
const EGL_DEVICE_EXT: egl::Int = 0x322C;
const EGL_DRM_RENDER_NODE_FILE_EXT: egl::Int = 0x3377;

type GbmDevice = c_void;
type GbmBo = c_void;
type X11Display = c_void;

type FnGbmCreateDevice = unsafe extern "C" fn(c_int) -> *mut GbmDevice;
type FnGbmDeviceDestroy = unsafe extern "C" fn(*mut GbmDevice);
type FnGbmBoCreate = unsafe extern "C" fn(*mut GbmDevice, u32, u32, u32, u32) -> *mut GbmBo;
type FnGbmBoDestroy = unsafe extern "C" fn(*mut GbmBo);
type FnGbmBoGetFd = unsafe extern "C" fn(*mut GbmBo) -> c_int;
type FnGbmBoGetStride = unsafe extern "C" fn(*mut GbmBo) -> u32;

type FnXOpenDisplay = unsafe extern "C" fn(*const c_char) -> *mut X11Display;
type FnXCloseDisplay = unsafe extern "C" fn(*mut X11Display) -> c_int;

type FnGlGenTextures = unsafe extern "C" fn(c_int, *mut c_uint);
type FnGlBindTexture = unsafe extern "C" fn(c_uint, c_uint);
type FnGlDeleteTextures = unsafe extern "C" fn(c_int, *const c_uint);
type FnGlGetError = unsafe extern "C" fn() -> c_uint;
type FnGlEglImageTargetTexture2DOes = unsafe extern "C" fn(c_uint, *mut c_void);

type FnEglGetPlatformDisplayExt =
    unsafe extern "C" fn(egl::Enum, *mut c_void, *const egl::Int) -> egl::EGLDisplay;
type FnEglCreateImageKhr = unsafe extern "C" fn(
    egl::EGLDisplay,
    *mut c_void,
    egl::Enum,
    *mut c_void,
    *const egl::Int,
) -> *mut c_void;
type FnEglDestroyImageKhr = unsafe extern "C" fn(egl::EGLDisplay, *mut c_void) -> c_uint;
type FnEglQueryDisplayAttribExt =
    unsafe extern "C" fn(egl::EGLDisplay, egl::Int, *mut isize) -> c_uint;
type FnEglQueryDeviceStringExt = unsafe extern "C" fn(*mut c_void, egl::Int) -> *const c_char;

/// Returns true if a GBM-allocated ARGB8888 dmabuf can be imported as an EGL
/// image and bound to a GL texture on the EGL display CEF will use. The
/// `ozone_platform` selects which display type to test (`"wayland"` uses the
/// passed `wayland_egl_dpy`; anything else opens an XWayland display).
///
/// When libgbm or the DRM render node is unavailable the probe returns true
/// (assume supported) — same fallback the C++ version used, so the platform
/// can opt into shared textures and let Chromium fail loudly if the runtime
/// stack disagrees.
///
/// `wayland_egl_dpy` may be NULL when `ozone_platform != "wayland"`.
///
/// # Safety
/// `ozone_platform` must be NUL-terminated or null. `wayland_egl_dpy`
/// must be a live `*mut wl_display` when `ozone_platform == "wayland"`.
pub unsafe fn jfn_wl_dmabuf_probe(
    ozone_platform: *const c_char,
    wayland_egl_dpy: *mut c_void,
) -> bool {
    let ozone = if ozone_platform.is_null() {
        ""
    } else {
        unsafe { CStr::from_ptr(ozone_platform) }
            .to_str()
            .unwrap_or_default()
    };
    match probe(ozone, wayland_egl_dpy) {
        Ok(b) => b,
        Err(msg) => {
            tracing::warn!("dmabuf probe: {}", msg);
            false
        }
    }
}

fn probe(ozone: &str, wayland_egl_dpy: *mut c_void) -> Result<bool, String> {
    let egl = egl::load()?;

    let (display, owns_display, _x11_state) = acquire_display(&egl, ozone, wayland_egl_dpy)?;

    let result = (|| -> Result<bool, String> {
        egl.bind_api(khronos_egl::OPENGL_ES_API)
            .map_err(|e| format!("eglBindAPI failed: {e}"))?;

        let cfg_attrs = [
            khronos_egl::RENDERABLE_TYPE,
            khronos_egl::OPENGL_ES2_BIT,
            khronos_egl::SURFACE_TYPE,
            khronos_egl::PBUFFER_BIT,
            khronos_egl::NONE,
        ];
        let Some(config) = egl
            .choose_first_config(display, &cfg_attrs)
            .map_err(|e| format!("eglChooseConfig failed: {e}"))?
        else {
            return Err("no suitable EGL config".to_string());
        };

        let ctx_attrs = [khronos_egl::CONTEXT_CLIENT_VERSION, 2, khronos_egl::NONE];
        let ctx = egl
            .create_context(display, config, None, &ctx_attrs)
            .map_err(|e| format!("can't create GLES context: {e}"))?;

        let pb_attrs = [
            khronos_egl::WIDTH,
            1,
            khronos_egl::HEIGHT,
            1,
            khronos_egl::NONE,
        ];
        // A pbuffer may legitimately be unavailable; a surfaceless context
        // still makes current on drivers that advertise
        // EGL_KHR_surfaceless_context.
        let pbuf = egl.create_pbuffer_surface(display, config, &pb_attrs).ok();

        if egl.make_current(display, pbuf, pbuf, Some(ctx)).is_err() {
            if let Some(pbuf) = pbuf {
                let _ = egl.destroy_surface(display, pbuf);
            }
            let _ = egl.destroy_context(display, ctx);
            return Err("eglMakeCurrent failed".into());
        }

        let gl_result = run_gl_test(&egl, display);

        let _ = egl.make_current(display, None, None, None);
        if let Some(pbuf) = pbuf {
            let _ = egl.destroy_surface(display, pbuf);
        }
        let _ = egl.destroy_context(display, ctx);

        gl_result
    })();

    if owns_display {
        let _ = egl.terminate(display);
    }

    match &result {
        Ok(true) => tracing::info!("dmabuf probe: GBM -> EGL -> GL import OK"),
        Ok(false) => tracing::warn!("dmabuf probe: ARGB8888 dmabuf import failed"),
        Err(e) => tracing::warn!("dmabuf probe: {}", e),
    }
    result
}

struct X11Owned {
    _lib: Library,
    dpy: *mut X11Display,
    close: FnXCloseDisplay,
}

impl Drop for X11Owned {
    fn drop(&mut self) {
        if !self.dpy.is_null() {
            unsafe { (self.close)(self.dpy) };
        }
    }
}

fn acquire_display(
    egl: &egl::Egl,
    ozone: &str,
    wayland_egl_dpy: *mut c_void,
) -> Result<(egl::Display, bool, Option<X11Owned>), String> {
    if ozone == "wayland" {
        tracing::info!("dmabuf probe: testing on Wayland EGL display");
        // SAFETY: the caller guarantees a live EGL display for the Wayland
        // case.
        let display = unsafe { egl::Display::from_ptr(wayland_egl_dpy) };
        return Ok((display, false, None));
    }

    let lib = unsafe { Library::new("libX11.so.6") }
        .map_err(|e| format!("libX11 not available: {}", e))?;
    let open: libloading::Symbol<FnXOpenDisplay> = unsafe { lib.get(b"XOpenDisplay\0") }
        .map_err(|e| format!("XOpenDisplay missing: {}", e))?;
    let close: libloading::Symbol<FnXCloseDisplay> = unsafe { lib.get(b"XCloseDisplay\0") }
        .map_err(|e| format!("XCloseDisplay missing: {}", e))?;
    let close_fn: FnXCloseDisplay = *close;
    let dpy = unsafe { open(ptr::null()) };
    if dpy.is_null() {
        return Err("XOpenDisplay failed (no XWayland?)".into());
    }
    let owned = X11Owned {
        _lib: lib,
        dpy,
        close: close_fn,
    };

    let display = if let Some(fp) = egl.get_proc_address("eglGetPlatformDisplayEXT") {
        let f: FnEglGetPlatformDisplayExt = unsafe { std::mem::transmute(fp) };
        let raw = unsafe { f(EGL_PLATFORM_X11_KHR, dpy, ptr::null()) };
        if raw.is_null() {
            return Err("no EGL display for X11".into());
        }
        // SAFETY: `raw` is a non-null display just returned by EGL.
        unsafe { egl::Display::from_ptr(raw) }
    } else {
        // SAFETY: `dpy` is a live Xlib display owned by `owned`.
        let Some(display) = (unsafe { egl.get_display(dpy) }) else {
            return Err("no EGL display for X11".into());
        };
        display
    };

    let (major, minor) = egl
        .initialize(display)
        .map_err(|e| format!("EGL init on X11 failed: {e}"))?;
    tracing::info!(
        "dmabuf probe: testing on X11 EGL display ({}.{})",
        major,
        minor
    );

    Ok((display, true, Some(owned)))
}

fn run_gl_test(egl: &egl::Egl, display: egl::Display) -> Result<bool, String> {
    let gen_tex = get_gl::<FnGlGenTextures>(egl, "glGenTextures")?;
    let bind_tex = get_gl::<FnGlBindTexture>(egl, "glBindTexture")?;
    let del_tex = get_gl::<FnGlDeleteTextures>(egl, "glDeleteTextures")?;
    let get_err = get_gl::<FnGlGetError>(egl, "glGetError")?;
    let img_target = get_gl::<FnGlEglImageTargetTexture2DOes>(egl, "glEGLImageTargetTexture2DOES")?;
    let create_image = get_gl::<FnEglCreateImageKhr>(egl, "eglCreateImageKHR")?;
    let destroy_image = get_gl::<FnEglDestroyImageKhr>(egl, "eglDestroyImageKHR")?;

    let gbm_lib = match unsafe { Library::new("libgbm.so.1") } {
        Ok(l) => l,
        Err(_) => {
            tracing::warn!("dmabuf probe: libgbm not available, assuming supported");
            return Ok(true);
        }
    };
    let gbm = match GbmFns::load(&gbm_lib) {
        Some(g) => g,
        None => {
            tracing::warn!("dmabuf probe: libgbm missing symbols, assuming supported");
            return Ok(true);
        }
    };

    let drm_fd = match find_drm_node(egl, display).or_else(open_legacy_node) {
        Some(fd) => fd,
        None => {
            tracing::warn!("dmabuf probe: no DRM render node, assuming supported");
            return Ok(true);
        }
    };

    let device = unsafe { (gbm.create_device)(drm_fd.as_raw_fd()) };
    if device.is_null() {
        return Err("gbm_create_device failed".into());
    }

    let bo = unsafe {
        (gbm.bo_create)(
            device,
            64,
            64,
            DrmFourcc::Argb8888 as u32,
            GBM_BO_USE_RENDERING,
        )
    };
    if bo.is_null() {
        unsafe { (gbm.device_destroy)(device) };
        return Err("gbm_bo_create ARGB8888 failed".into());
    }

    let raw_dmabuf_fd = unsafe { (gbm.bo_get_fd)(bo) };
    let dmabuf_fd = (raw_dmabuf_fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(raw_dmabuf_fd) });
    let stride = unsafe { (gbm.bo_get_stride)(bo) };

    let result = if let Some(dmabuf_fd) = &dmabuf_fd {
        let img_attrs: [egl::Int; 13] = [
            khronos_egl::WIDTH,
            64,
            khronos_egl::HEIGHT,
            64,
            EGL_LINUX_DRM_FOURCC_EXT,
            DrmFourcc::Argb8888 as egl::Int,
            EGL_DMA_BUF_PLANE0_FD_EXT,
            dmabuf_fd.as_raw_fd() as egl::Int,
            EGL_DMA_BUF_PLANE0_OFFSET_EXT,
            0,
            EGL_DMA_BUF_PLANE0_PITCH_EXT,
            stride as egl::Int,
            khronos_egl::NONE,
        ];
        let image = unsafe {
            create_image(
                display.as_ptr(),
                ptr::null_mut(),
                EGL_LINUX_DMA_BUF_EXT,
                ptr::null_mut(),
                img_attrs.as_ptr(),
            )
        };
        if image.is_null() {
            tracing::warn!(
                "dmabuf probe: eglCreateImageKHR failed ({:?})",
                egl.get_error()
            );
            Ok(false)
        } else {
            let mut tex: c_uint = 0;
            unsafe {
                gen_tex(1, &mut tex);
                bind_tex(GL_TEXTURE_2D, tex);
                img_target(GL_TEXTURE_2D, image);
                let err = get_err();
                let ok = err == GL_NO_ERROR;
                if !ok {
                    tracing::warn!(
                        "dmabuf probe: glEGLImageTargetTexture2DOES failed (0x{:x})",
                        err
                    );
                }
                del_tex(1, &tex);
                destroy_image(display.as_ptr(), image);
                Ok(ok)
            }
        }
    } else {
        Err("gbm_bo_get_fd failed".to_string())
    };

    drop(dmabuf_fd);
    unsafe {
        (gbm.bo_destroy)(bo);
        (gbm.device_destroy)(device);
    }
    result
}

struct GbmFns {
    create_device: FnGbmCreateDevice,
    device_destroy: FnGbmDeviceDestroy,
    bo_create: FnGbmBoCreate,
    bo_destroy: FnGbmBoDestroy,
    bo_get_fd: FnGbmBoGetFd,
    bo_get_stride: FnGbmBoGetStride,
}

impl GbmFns {
    fn load(lib: &Library) -> Option<Self> {
        unsafe {
            Some(Self {
                create_device: *lib.get::<FnGbmCreateDevice>(b"gbm_create_device\0").ok()?,
                device_destroy: *lib
                    .get::<FnGbmDeviceDestroy>(b"gbm_device_destroy\0")
                    .ok()?,
                bo_create: *lib.get::<FnGbmBoCreate>(b"gbm_bo_create\0").ok()?,
                bo_destroy: *lib.get::<FnGbmBoDestroy>(b"gbm_bo_destroy\0").ok()?,
                bo_get_fd: *lib.get::<FnGbmBoGetFd>(b"gbm_bo_get_fd\0").ok()?,
                bo_get_stride: *lib.get::<FnGbmBoGetStride>(b"gbm_bo_get_stride\0").ok()?,
            })
        }
    }
}

fn find_drm_node_path(egl: &egl::Egl, display: egl::Display) -> Option<CString> {
    let query_display_ptr = egl.get_proc_address("eglQueryDisplayAttribEXT")?;
    let query_device_str_ptr = egl.get_proc_address("eglQueryDeviceStringEXT")?;
    let query_display: FnEglQueryDisplayAttribExt =
        unsafe { std::mem::transmute(query_display_ptr) };
    let query_device_str: FnEglQueryDeviceStringExt =
        unsafe { std::mem::transmute(query_device_str_ptr) };

    let mut device_attrib: isize = 0;
    let ok = unsafe { query_display(display.as_ptr(), EGL_DEVICE_EXT, &mut device_attrib) };
    if ok == 0 || device_attrib == 0 {
        return None;
    }
    let egl_device = device_attrib as *mut c_void;
    let node_ptr = unsafe { query_device_str(egl_device, EGL_DRM_RENDER_NODE_FILE_EXT) };
    if node_ptr.is_null() {
        return None;
    }
    Some(unsafe { CStr::from_ptr(node_ptr) }.to_owned())
}

fn find_drm_node(egl: &egl::Egl, display: egl::Display) -> Option<OwnedFd> {
    let node = find_drm_node_path(egl, display)?;
    let fd = open(
        node.as_c_str(),
        OFlag::O_RDWR | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .ok()?;
    tracing::info!("dmabuf probe using render node: {}", node.to_string_lossy());
    Some(fd)
}

/// # Safety
/// Same contract as [`jfn_wl_dmabuf_probe`].
pub unsafe fn cef_render_node(
    ozone_platform: *const c_char,
    wayland_egl_dpy: *mut c_void,
) -> Option<(i64, i64)> {
    let ozone = if ozone_platform.is_null() {
        ""
    } else {
        unsafe { CStr::from_ptr(ozone_platform) }
            .to_str()
            .unwrap_or_default()
    };
    match render_node(ozone, wayland_egl_dpy) {
        Ok(node) => node,
        Err(msg) => {
            tracing::warn!("cef render node: {}", msg);
            None
        }
    }
}

fn render_node(ozone: &str, wayland_egl_dpy: *mut c_void) -> Result<Option<(i64, i64)>, String> {
    let egl = egl::load()?;

    let (display, owns_display, _x11_state) = acquire_display(&egl, ozone, wayland_egl_dpy)?;
    let path = find_drm_node_path(&egl, display);
    if owns_display {
        let _ = egl.terminate(display);
    }
    Ok(path.as_deref().and_then(node_major_minor))
}

fn node_major_minor(path: &CStr) -> Option<(i64, i64)> {
    let st = nix::sys::stat::stat(path).ok()?;
    let major = nix::sys::stat::major(st.st_rdev);
    let minor = nix::sys::stat::minor(st.st_rdev);
    Some((major as i64, minor as i64))
}

fn open_legacy_node() -> Option<OwnedFd> {
    (128..136).find_map(|i| {
        let path = format!("/dev/dri/renderD{}", i);
        open(
            path.as_str(),
            OFlag::O_RDWR | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .ok()
    })
}

fn get_gl<T>(egl: &egl::Egl, name: &str) -> Result<T, String> {
    egl.get_proc_address(name)
        .map(|p| unsafe { std::mem::transmute_copy::<extern "system" fn(), T>(&p) })
        .ok_or_else(|| format!("missing {}", name))
}
