use parking_lot::Mutex;
use std::sync::OnceLock;

use crate::types::{PlaybackEvent, PlaybackEventKind};

type SetCb = extern "C" fn(bool);

fn cb_slot() -> &'static Mutex<Option<SetCb>> {
    static SLOT: OnceLock<Mutex<Option<SetCb>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

pub fn jfn_playback_set_theme_video_mode_handler(cb: Option<SetCb>) {
    *cb_slot().lock() = cb;
}

pub(crate) fn deliver(ev: &PlaybackEvent) {
    match ev.kind {
        PlaybackEventKind::Finished | PlaybackEventKind::Canceled | PlaybackEventKind::Error => {
            if let Some(cb) = *cb_slot().lock() {
                cb(false);
            }
            crate::chrome::set_video_active(false);
            crate::chrome::set_osd_visible(false);
        }
        _ => {}
    }
}
