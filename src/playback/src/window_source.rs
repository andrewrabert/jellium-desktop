use jfn_platform_abi::{MpvCreatedWindow, WindowSnapshot, WindowSource};

pub struct MpvWindowSource;

pub static MPV_WINDOW_SOURCE: MpvWindowSource = MpvWindowSource;

impl WindowSource for MpvWindowSource {
    fn snapshot(&self) -> WindowSnapshot {
        WindowSnapshot {
            extent: crate::ingest_driver::jfn_playback_window_extent(),
            position: jfn_platform_abi::try_lease()
                .and_then(|lease| lease.platform().query_window_position()),
            maximized: crate::ingest_driver::jfn_playback_window_maximized(),
            fullscreen: crate::ingest_driver::jfn_playback_fullscreen(),
        }
    }
}

impl MpvCreatedWindow for MpvWindowSource {}
