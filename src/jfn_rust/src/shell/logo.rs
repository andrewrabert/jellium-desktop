use std::sync::OnceLock;

include!(concat!(env!("OUT_DIR"), "/logo_dimensions.rs"));

const PIXELS: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/logo.rgba"));

pub const NATIVE: iced_core::Size<u32> = iced_core::Size {
    width: WIDTH,
    height: HEIGHT,
};

pub const CONNECT_WIDTH: f32 = 500.0;

pub const ABOUT_WIDTH: f32 = 240.0;

pub fn handle() -> iced_core::image::Handle {
    static HANDLE: OnceLock<iced_core::image::Handle> = OnceLock::new();
    HANDLE
        .get_or_init(|| iced_core::image::Handle::from_rgba(WIDTH, HEIGHT, PIXELS))
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

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
