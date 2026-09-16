use parking_lot::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;

use crate::exec_js::call as call_exec_js;
use crate::types::{PlaybackEvent, PlaybackEventKind};

#[derive(Serialize)]
struct BufferedRange {
    start: i64,
    end: i64,
}

type SetHzCb = extern "C" fn(f64);

struct Handlers {
    set_hz: Option<SetHzCb>,
}

fn slot() -> &'static Mutex<Handlers> {
    static SLOT: OnceLock<Mutex<Handlers>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(Handlers { set_hz: None }))
}

pub fn jfn_playback_set_browsers_refresh_rate_handler(cb: Option<SetHzCb>) {
    slot().lock().set_hz = cb;
}

static WAS_MAXIMIZED: AtomicBool = AtomicBool::new(false);

pub fn jfn_playback_was_maximized_before_fullscreen() -> bool {
    WAS_MAXIMIZED.load(Ordering::Relaxed)
}

pub(crate) fn deliver(ev: &PlaybackEvent) {
    let snap = &ev.snapshot;
    match ev.kind {
        PlaybackEventKind::Started => call_exec_js("window._nativeEmit('playing')"),
        PlaybackEventKind::Paused => call_exec_js("window._nativeEmit('paused')"),
        PlaybackEventKind::Finished => call_exec_js("window._nativeEmit('finished')"),
        PlaybackEventKind::Canceled => call_exec_js("window._nativeEmit('canceled')"),
        PlaybackEventKind::Error => {
            let msg = if ev.error_message.is_empty() {
                "Playback error"
            } else {
                ev.error_message.as_str()
            };
            let text = jfn_js_json::to_js_json(msg).unwrap_or_else(|| "\"\"".to_string());
            call_exec_js(&format!("window._nativeEmit('error',{text})"));
        }
        PlaybackEventKind::SeekingChanged => {
            if ev.flag {
                call_exec_js("window._nativeEmit('seeking')");
            }
        }
        PlaybackEventKind::TrackLoaded => {
            if snap.variant_switch_pending {
                call_exec_js("window._nativeEmit('paused')");
            }
        }
        PlaybackEventKind::PositionChanged => {
            let ms = (snap.position_us / 1000) as i32;
            call_exec_js(&format!("window._nativeUpdatePosition({})", ms));
        }
        PlaybackEventKind::DurationChanged => {
            let ms = (snap.duration_us / 1000) as i32;
            call_exec_js(&format!("window._nativeUpdateDuration({})", ms));
        }
        PlaybackEventKind::RateChanged => {
            call_exec_js(&format!("window._nativeSetRate({})", snap.rate));
        }
        PlaybackEventKind::FullscreenChanged => {
            WAS_MAXIMIZED.store(snap.maximized_before_fullscreen, Ordering::Relaxed);
            call_exec_js(&format!(
                "window._nativeFullscreenChanged({})",
                if snap.fullscreen { "true" } else { "false" }
            ));
        }
        PlaybackEventKind::DisplayHzChanged => {
            if let Some(cb) = slot().lock().set_hz {
                cb(snap.display_hz);
            }
        }
        PlaybackEventKind::BufferedRangesChanged => {
            let ranges: Vec<BufferedRange> = snap
                .buffered
                .iter()
                .map(|r| BufferedRange {
                    start: r.start_ticks,
                    end: r.end_ticks,
                })
                .collect();
            let json = jfn_js_json::to_js_json(&ranges).unwrap_or_else(|| "[]".to_string());
            call_exec_js(&format!("window._nativeUpdateBufferedRanges({json})"));
        }
        PlaybackEventKind::BufferingChanged
        | PlaybackEventKind::MediaTypeChanged
        | PlaybackEventKind::MetadataChanged
        | PlaybackEventKind::ArtworkChanged
        | PlaybackEventKind::QueueCapsChanged
        | PlaybackEventKind::Seeked => {}
    }
}
