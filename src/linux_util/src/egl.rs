pub use khronos_egl::{Display, EGLDisplay, Enum, Int, NativeDisplayType};

pub type Egl = khronos_egl::DynamicInstance<khronos_egl::EGL1_4>;

pub fn load() -> Result<Egl, String> {
    unsafe { Egl::load_required_from_filename("libEGL.so.1") }
        .map_err(|e| format!("libEGL not available: {e}"))
}
