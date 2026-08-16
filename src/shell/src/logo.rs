//! The Jellyfin logo the shell overlay draws, from `src/web/overlay.html`'s
//! `<img class="logo">`.

use std::sync::OnceLock;

include!(concat!(env!("OUT_DIR"), "/logo_dimensions.rs"));

const PIXELS: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/logo.rgba"));

/// The logo's native raster size.
pub const NATIVE: iced_core::Size<u32> = iced_core::Size {
    width: WIDTH,
    height: HEIGHT,
};

/// Logical width on the connect screen.
pub const CONNECT_WIDTH: f32 = 500.0;

/// Logical width in the about panel.
pub const ABOUT_WIDTH: f32 = 240.0;

/// The logo's pixels, one handle for the whole process: every view that draws
/// it names the same image, so it is uploaded once and measured without a
/// decode.
pub fn handle() -> iced_core::image::Handle {
    static HANDLE: OnceLock<iced_core::image::Handle> = OnceLock::new();
    HANDLE
        .get_or_init(|| iced_core::image::Handle::from_rgba(WIDTH, HEIGHT, PIXELS))
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The largest scale in the covered set
    /// `dev/requirements/the-display-scale-every-consumer-overrules.md`
    /// records.
    fn largest_covered_scale() -> Option<f32> {
        jfn_platform_abi::COVERED_SCALES
            .into_iter()
            .max()
            .map(|s| s.as_f32())
    }

    #[test]
    fn the_asset_covers_the_connect_width_at_the_largest_covered_scale() {
        assert_eq!(
            largest_covered_scale().map(|s| CONNECT_WIDTH * s <= NATIVE.width as f32),
            Some(true)
        );
    }

    #[test]
    fn the_asset_covers_the_about_width_at_the_largest_covered_scale() {
        assert_eq!(
            largest_covered_scale().map(|s| ABOUT_WIDTH * s <= NATIVE.width as f32),
            Some(true)
        );
    }
}
