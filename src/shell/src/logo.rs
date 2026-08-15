//! The Jellyfin logo the shell overlay draws, from `src/web/overlay.html`'s
//! `<img class="logo">`.

use std::sync::OnceLock;

include!(concat!(env!("OUT_DIR"), "/logo_dimensions.rs"));

const PIXELS: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/logo.rgba"));

/// The logo's pixels, one handle for the whole process: every view that draws
/// it names the same image, so it is uploaded once and measured without a
/// decode.
pub fn handle() -> iced_core::image::Handle {
    static HANDLE: OnceLock<iced_core::image::Handle> = OnceLock::new();
    HANDLE
        .get_or_init(|| iced_core::image::Handle::from_rgba(WIDTH, HEIGHT, PIXELS))
        .clone()
}
