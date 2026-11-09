//! libEGL.so.1, loaded at runtime through `khronos-egl`'s dynamic instance.

pub use khronos_egl::{Display, EGLDisplay, Enum, Int, NativeDisplayType};

/// EGL 1.4 is the floor: `eglBindAPI`, pbuffer surfaces and
/// `eglGetProcAddress` must all resolve at load time.
pub type Egl = khronos_egl::DynamicInstance<khronos_egl::EGL1_4>;

pub fn load() -> Result<Egl, String> {
    unsafe { Egl::load_required_from_filename("libEGL.so.1") }
        .map_err(|e| format!("libEGL not available: {e}"))
}
