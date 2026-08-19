//! Digests mpv events into coordinator inputs.
//!
//! Consumes [`mpv::Event`] values from the Rust event loop and produces
//! coordinator [`Input`]s plus a couple of side outputs that don't fit
//! the [`Input`] vocabulary (the window-extent mirror for the
//! geometry-save cache).
//!
//! Per-process state (fullscreen, window_max, display_hz)
//! lives in [`IngestState`] so multiple
//! ingest calls observe the same change-suppression behavior.

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use crossbeam_utils::atomic::AtomicCell;
use jfn_mpv::{Event, ObserveId, PropertyValue};
use jfn_platform_abi::{LogicalSize, PhysicalSize, Scale, WindowExtent};

use crate::coordinator::Input;
use crate::types::{EndReason, PlaybackBufferedRange};

/// Property observe-IDs passed to `mpv_observe_property`.
pub mod observe_id {
    pub const OSD_DIMS: u64 = 2;
    pub const FULLSCREEN: u64 = 3;
    pub const PAUSE: u64 = 4;
    pub const TIME_POS: u64 = 5;
    pub const DURATION: u64 = 6;
    pub const SPEED: u64 = 7;
    pub const SEEKING: u64 = 8;
    pub const DISPLAY_FPS: u64 = 9;
    pub const CACHE_STATE: u64 = 10;
    pub const WINDOW_MAX: u64 = 11;
    pub const PAUSED_FOR_CACHE: u64 = 13;
    pub const CORE_IDLE: u64 = 14;
    pub const VIDEO_FRAME_INFO: u64 = 15;
    pub const WINDOW_ID: u64 = 16;
}

const MAX_BUFFERED_RANGES: usize = 8;

/// Caller-provided platform hooks. Implementations stay outside this crate so
/// jfn-playback doesn't grow a platform dep.
pub trait IngestCtx {
    /// The display scale the platform reports.
    fn scale(&self) -> Scale;

    /// The app window's logical size where the OS, not mpv's
    /// `osd-dimensions`, is the authority for it.
    fn os_logical_size(&self) -> Option<LogicalSize>;
}

/// One ingest-loop output. Most map to coordinator inputs; the two side
/// variants exist because the prior C++ path didn't route them through
/// the dispatcher queue either.
#[derive(Debug)]
pub(crate) enum IngestOut {
    Input(Input),
    /// The window-extent cell was rewritten; the driver wakes the
    /// platform-abi window subscribers, which pull the new snapshot.
    WindowExtentChanged,
    /// Terminal: libmpv has shut down. Caller breaks out of the event
    /// loop and triggers the rest of the app's teardown.
    Shutdown,
}

/// Shared atomic cache mirroring the prior C++ `s_*` statics. Holds
/// last-observed values so digest functions can suppress duplicate
/// emissions (display-fps) and so external readers
/// (`fullscreen`, `window_maximized`, `display_hz`)
/// see the current state without round-tripping through the
/// coordinator snapshot.
#[derive(Debug, Default)]
pub struct IngestState {
    fullscreen: AtomicBool,
    window_maximized: AtomicBool,
    /// Last known window extent, written whole by the osd-dimensions
    /// digest.
    extent: AtomicCell<Option<WindowExtent>>,
    display_hz: AtomicCell<f64>,
    /// mpv's native window handle; `0` until the VO has a window.
    window_id: AtomicI64,
}

impl IngestState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn fullscreen(&self) -> bool {
        self.fullscreen.load(Ordering::Relaxed)
    }
    pub fn window_maximized(&self) -> bool {
        self.window_maximized.load(Ordering::Relaxed)
    }
    pub fn window_extent(&self) -> Option<WindowExtent> {
        self.extent.load()
    }
    pub(crate) fn set_window_extent(&self, extent: WindowExtent) {
        self.extent.store(Some(extent));
    }
    pub fn display_hz(&self) -> f64 {
        self.display_hz.load()
    }
    pub fn set_display_hz(&self, hz: f64) {
        self.display_hz.store(hz);
    }
    /// mpv's native window handle as last reported by `window-id`; `None`
    /// until mpv's VO has a window.
    pub fn window_id(&self) -> Option<i64> {
        let id = self.window_id.load(Ordering::Relaxed);
        (id != 0).then_some(id)
    }
}

/// Decode one [`Event`] into zero or more [`IngestOut`]s.
/// Re-exported under stable FFI-facing name for [`crate::ingest_driver`].
pub(crate) fn ingest_event_for_ffi<C: IngestCtx>(
    event: &Event,
    state: &IngestState,
    ctx: &C,
) -> Vec<IngestOut> {
    ingest(event, state, ctx)
}

/// Run only the property-digest path. Used by the Wayland fast path
/// that synthesizes osd-dimension updates outside the mpv event stream.
pub(crate) fn ingest_property_for_ffi<C: IngestCtx>(
    id: ObserveId,
    value: &PropertyValue,
    state: &IngestState,
    ctx: &C,
) -> Vec<IngestOut> {
    digest_property(id, value, state, ctx)
}

pub(crate) fn ingest<C: IngestCtx>(event: &Event, state: &IngestState, ctx: &C) -> Vec<IngestOut> {
    match event {
        Event::Shutdown => vec![IngestOut::Shutdown],
        Event::FileLoaded => vec![IngestOut::Input(Input::FileLoaded)],
        Event::EndFile(reason) => Some(end_file_input(reason))
            .into_iter()
            .map(IngestOut::Input)
            .collect(),
        Event::PropertyChange { id, value, .. } => digest_property(*id, value, state, ctx),
        _ => Vec::new(),
    }
}

fn end_file_input(reason: &jfn_mpv::EndFileReason) -> Input {
    use jfn_mpv::EndFileReason as R;
    match reason {
        R::Eof | R::Redirect => Input::EndFile {
            reason: EndReason::Eof,
            error_message: String::new(),
        },
        R::Stop | R::Quit => Input::EndFile {
            reason: EndReason::Canceled,
            error_message: String::new(),
        },
        R::Error(e) => Input::EndFile {
            reason: EndReason::Error,
            error_message: e.to_string(),
        },
        R::Unknown(_) => Input::EndFile {
            reason: EndReason::Canceled,
            error_message: String::new(),
        },
    }
}

fn digest_property<C: IngestCtx>(
    id: ObserveId,
    value: &PropertyValue,
    state: &IngestState,
    ctx: &C,
) -> Vec<IngestOut> {
    use observe_id::*;
    match id {
        OSD_DIMS => digest_osd_dims(value, state, ctx).into_iter().collect(),
        PAUSE => as_flag(value)
            .map(|f| vec![IngestOut::Input(Input::PauseChanged(f))])
            .unwrap_or_default(),
        TIME_POS => as_double(value)
            .map(|d| vec![IngestOut::Input(Input::Position((d * 1_000_000.0) as i64))])
            .unwrap_or_default(),
        DURATION => as_double(value)
            .map(|d| vec![IngestOut::Input(Input::Duration((d * 1_000_000.0) as i64))])
            .unwrap_or_default(),
        FULLSCREEN => match as_flag(value) {
            Some(f) => {
                state.fullscreen.store(f, Ordering::Relaxed);
                vec![IngestOut::Input(Input::Fullscreen {
                    fullscreen: f,
                    was_maximized: if f { state.window_maximized() } else { false },
                })]
            }
            None => Vec::new(),
        },
        SPEED => as_double(value)
            .map(|d| vec![IngestOut::Input(Input::Speed(d))])
            .unwrap_or_default(),
        SEEKING => as_flag(value)
            .map(|f| vec![IngestOut::Input(Input::SeekingChanged(f))])
            .unwrap_or_default(),
        PAUSED_FOR_CACHE => as_flag(value)
            .map(|f| vec![IngestOut::Input(Input::PausedForCache(f))])
            .unwrap_or_default(),
        CORE_IDLE => as_flag(value)
            .map(|f| vec![IngestOut::Input(Input::CoreIdle(f))])
            .unwrap_or_default(),
        VIDEO_FRAME_INFO => vec![IngestOut::Input(Input::VideoFrameAvailable(!matches!(
            value,
            PropertyValue::None
        )))],
        WINDOW_ID => {
            if let Some(id) = as_int(value) {
                state.window_id.store(id, Ordering::Relaxed);
            }
            Vec::new()
        }
        WINDOW_MAX => {
            if let Some(f) = as_flag(value) {
                state.window_maximized.store(f, Ordering::Relaxed);
            }
            Vec::new()
        }
        DISPLAY_FPS => {
            let Some(fps) = as_double(value) else {
                return Vec::new();
            };
            if fps == state.display_hz() {
                return Vec::new();
            }
            state.display_hz.store(fps);
            vec![IngestOut::Input(Input::DisplayHz(fps))]
        }
        CACHE_STATE => digest_cache_state(value),
        _ => Vec::new(),
    }
}

fn digest_osd_dims<C: IngestCtx>(
    value: &PropertyValue,
    state: &IngestState,
    ctx: &C,
) -> Vec<IngestOut> {
    let PropertyValue::Node(node) = value else {
        return Vec::new();
    };
    let w = node.get("w").and_then(|v| v.as_int()).unwrap_or(0);
    let h = node.get("h").and_then(|v| v.as_int()).unwrap_or(0);
    if w <= 0 || h <= 0 {
        return Vec::new();
    }
    let (Ok(w), Ok(h)) = (i32::try_from(w), i32::try_from(h)) else {
        return Vec::new();
    };
    let physical = PhysicalSize { w, h };
    let scale = ctx.scale();
    let extent = match ctx.os_logical_size().filter(|l| l.w > 0 && l.h > 0) {
        Some(logical) => extent_at(scale, logical),
        None => extent_of(scale, physical),
    };
    let Some(extent) = extent else {
        return Vec::new();
    };
    state.set_window_extent(extent);
    vec![IngestOut::WindowExtentChanged]
}

/// The extent a reported scale and an exact logical content size name; the
/// physical size is the single conversion of the logical one.
pub(crate) fn extent_at(scale: Scale, logical: LogicalSize) -> Option<WindowExtent> {
    WindowExtent::new(logical.to_physical(scale)?, scale, logical)
}

/// The extent a reported scale and mpv's exact pixel size name.
///
/// mpv's pixel size is carried through verbatim; the logical size is the
/// single conversion of it.
///
/// `None` when the division names no logical size, or when either axis is
/// below two pixels.
pub(crate) fn extent_of(scale: Scale, physical: PhysicalSize) -> Option<WindowExtent> {
    WindowExtent::new(physical, scale, physical.to_logical(scale)?)
}

fn digest_cache_state(value: &PropertyValue) -> Vec<IngestOut> {
    let PropertyValue::Node(node) = value else {
        return Vec::new();
    };
    let Some(arr) = node.get("seekable-ranges").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut ranges = Vec::with_capacity(arr.len().min(MAX_BUFFERED_RANGES));
    for range in arr.iter().take(MAX_BUFFERED_RANGES) {
        let start = range
            .get("start")
            .and_then(|v| v.as_double())
            .unwrap_or(0.0);
        let end = range.get("end").and_then(|v| v.as_double()).unwrap_or(0.0);
        ranges.push(PlaybackBufferedRange {
            start_ticks: (start * 10_000_000.0) as i64,
            end_ticks: (end * 10_000_000.0) as i64,
        });
    }
    vec![IngestOut::Input(Input::BufferedRanges(ranges))]
}

fn as_flag(v: &PropertyValue) -> Option<bool> {
    if let PropertyValue::Flag(f) = v {
        Some(*f)
    } else {
        None
    }
}

fn as_int(v: &PropertyValue) -> Option<i64> {
    if let PropertyValue::Int(i) = v {
        Some(*i)
    } else {
        None
    }
}

fn as_double(v: &PropertyValue) -> Option<f64> {
    if let PropertyValue::Double(d) = v {
        Some(*d)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jfn_mpv::Node;
    use jfn_platform_abi::COVERED_SCALES;

    struct TestCtx {
        scale: Scale,
        mac: Option<LogicalSize>,
    }
    impl IngestCtx for TestCtx {
        fn scale(&self) -> Scale {
            self.scale
        }
        fn os_logical_size(&self) -> Option<LogicalSize> {
            self.mac
        }
    }

    fn ratio(numerator: u64, denominator: u64) -> Option<Scale> {
        Scale::from_ratio(numerator, std::num::NonZeroU64::new(denominator)?)
    }

    fn ctx(scale: Scale) -> TestCtx {
        TestCtx { scale, mac: None }
    }

    fn prop(id: u64, value: PropertyValue) -> Event {
        Event::PropertyChange {
            id,
            name: String::new(),
            value,
        }
    }

    #[test]
    fn pause_flag_round_trips() {
        let state = IngestState::new();
        let out = ingest(
            &prop(observe_id::PAUSE, PropertyValue::Flag(true)),
            &state,
            &ctx(Scale::ONE),
        );
        assert_eq!(out.len(), 1);
        matches!(out[0], IngestOut::Input(Input::PauseChanged(true)));
    }

    #[test]
    fn time_pos_scales_to_microseconds() {
        let state = IngestState::new();
        let out = ingest(
            &prop(observe_id::TIME_POS, PropertyValue::Double(1.5)),
            &state,
            &ctx(Scale::ONE),
        );
        let IngestOut::Input(Input::Position(p)) = &out[0] else {
            panic!("expected Position");
        };
        assert_eq!(*p, 1_500_000);
    }

    #[test]
    fn fullscreen_carries_maximized_when_entering() {
        let state = IngestState::new();
        // Window first reports maximized true.
        let _ = ingest(
            &prop(observe_id::WINDOW_MAX, PropertyValue::Flag(true)),
            &state,
            &ctx(Scale::ONE),
        );
        let out = ingest(
            &prop(observe_id::FULLSCREEN, PropertyValue::Flag(true)),
            &state,
            &ctx(Scale::ONE),
        );
        let IngestOut::Input(Input::Fullscreen {
            fullscreen,
            was_maximized,
        }) = out[0]
        else {
            panic!("expected Fullscreen");
        };
        assert!(fullscreen);
        assert!(was_maximized);

        // Leaving fullscreen always reports was_maximized = false.
        let out = ingest(
            &prop(observe_id::FULLSCREEN, PropertyValue::Flag(false)),
            &state,
            &ctx(Scale::ONE),
        );
        let IngestOut::Input(Input::Fullscreen {
            fullscreen,
            was_maximized,
        }) = out[0]
        else {
            panic!("expected Fullscreen");
        };
        assert!(!fullscreen);
        assert!(!was_maximized);
        assert!(!state.fullscreen());
    }

    fn digest_dims(state: &IngestState, w: i64, h: i64, scale: Scale) {
        let node = Node::Map(vec![("w".into(), Node::Int(w)), ("h".into(), Node::Int(h))]);
        let _ = ingest(
            &prop(observe_id::OSD_DIMS, PropertyValue::Node(node)),
            state,
            &ctx(scale),
        );
    }

    #[test]
    fn display_fps_suppresses_duplicates() {
        let state = IngestState::new();
        let v = PropertyValue::Double(60.0);
        let out = ingest(
            &prop(observe_id::DISPLAY_FPS, v.clone()),
            &state,
            &ctx(Scale::ONE),
        );
        matches!(out[0], IngestOut::Input(Input::DisplayHz(_)));
        assert_eq!(state.display_hz(), 60.0);
        let out = ingest(&prop(observe_id::DISPLAY_FPS, v), &state, &ctx(Scale::ONE));
        assert!(out.is_empty());
    }

    #[test]
    fn osd_dims_emits_logical_and_pixel_pairs() {
        let state = IngestState::new();
        let node = Node::Map(vec![
            ("w".into(), Node::Int(3840)),
            ("h".into(), Node::Int(2160)),
        ]);
        let observed = ratio(2, 1).map(|two| {
            let out = ingest(
                &prop(observe_id::OSD_DIMS, PropertyValue::Node(node)),
                &state,
                &ctx(two),
            );
            (
                out.len(),
                state
                    .window_extent()
                    .map(|e| (e.physical(), e.logical(), e.scale())),
            )
        });
        assert_eq!(
            observed,
            ratio(2, 1).map(|two| (
                1,
                Some((
                    PhysicalSize { w: 3840, h: 2160 },
                    LogicalSize { w: 1920, h: 1080 },
                    two
                ))
            ))
        );
    }

    #[test]
    fn osd_dims_uses_macos_logical_override() {
        let state = IngestState::new();
        let node = Node::Map(vec![
            ("w".into(), Node::Int(3840)),
            ("h".into(), Node::Int(2160)),
        ]);
        let observed = ratio(2, 1).map(|two| {
            let mut c = ctx(two);
            c.mac = Some(LogicalSize { w: 1280, h: 720 });
            let _ = ingest(
                &prop(observe_id::OSD_DIMS, PropertyValue::Node(node)),
                &state,
                &c,
            );
            state.window_extent().map(|e| (e.logical(), e.physical()))
        });
        assert_eq!(
            observed,
            Some(Some((
                LogicalSize { w: 1280, h: 720 },
                PhysicalSize { w: 2560, h: 1440 }
            )))
        );
    }

    #[test]
    fn the_extent_carries_the_reported_scale_at_every_covered_scale() {
        let logical = LogicalSize { w: 1280, h: 720 };
        let observed: Vec<Option<(Scale, LogicalSize)>> = COVERED_SCALES
            .into_iter()
            .map(|scale| {
                let physical = logical.to_physical(scale)?;
                let state = IngestState::new();
                digest_dims(&state, i64::from(physical.w), i64::from(physical.h), scale);
                state.window_extent().map(|e| (e.scale(), e.logical()))
            })
            .collect();
        assert_eq!(
            observed,
            COVERED_SCALES.map(|s| Some((s, logical))).to_vec()
        );
    }

    #[test]
    fn the_extent_carries_the_host_s_logical_size_verbatim() {
        let logical = LogicalSize { w: 1281, h: 721 };
        let observed: Vec<Option<LogicalSize>> = COVERED_SCALES
            .into_iter()
            .map(|scale| extent_at(scale, logical).map(|e| e.logical()))
            .collect();
        assert_eq!(observed, vec![Some(logical); COVERED_SCALES.len()]);
    }

    #[test]
    fn mpv_s_pixel_size_survives_a_scale_that_does_not_divide_it() {
        // 1497 / 2.5 rounds to 599; mpv's own 1497 must reach the extent.
        let physical = PhysicalSize { w: 1497, h: 843 };
        let observed = Scale::from_f64(2.5).map(|scale| {
            let state = IngestState::new();
            digest_dims(&state, i64::from(physical.w), i64::from(physical.h), scale);
            state.window_extent().map(|e| e.physical())
        });
        assert_eq!(observed, Some(Some(physical)));
    }

    #[test]
    fn later_digest_overwrites_earlier() {
        let state = IngestState::new();
        let observed = ratio(2, 1).map(|two| {
            digest_dims(&state, 1600, 900, Scale::ONE);
            digest_dims(&state, 1196, 636, two);
            (
                state.window_extent().map(|e| (e.physical(), e.scale())),
                two,
            )
        });
        assert_eq!(
            observed,
            ratio(2, 1).map(|two| (Some((PhysicalSize { w: 1196, h: 636 }, two)), two))
        );
    }

    #[test]
    fn osd_dims_rejects_non_positive() {
        let state = IngestState::new();
        let node = Node::Map(vec![
            ("w".into(), Node::Int(0)),
            ("h".into(), Node::Int(1080)),
        ]);
        let out = ingest(
            &prop(observe_id::OSD_DIMS, PropertyValue::Node(node)),
            &state,
            &ctx(Scale::ONE),
        );
        assert!(out.is_empty());
    }

    #[test]
    fn cache_state_extracts_seekable_ranges() {
        let state = IngestState::new();
        let range = Node::Map(vec![
            ("start".into(), Node::Double(0.0)),
            ("end".into(), Node::Double(2.5)),
        ]);
        let root = Node::Map(vec![(
            "seekable-ranges".into(),
            Node::Array(vec![range.clone(), range]),
        )]);
        let out = ingest(
            &prop(observe_id::CACHE_STATE, PropertyValue::Node(root)),
            &state,
            &ctx(Scale::ONE),
        );
        let IngestOut::Input(Input::BufferedRanges(ref r)) = out[0] else {
            panic!("expected BufferedRanges");
        };
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].start_ticks, 0);
        assert_eq!(r[0].end_ticks, 25_000_000);
    }

    #[test]
    fn shutdown_event_returns_terminal() {
        let state = IngestState::new();
        let out = ingest(&Event::Shutdown, &state, &ctx(Scale::ONE));
        assert!(matches!(out[0], IngestOut::Shutdown));
    }

    #[test]
    fn end_file_maps_reason() {
        let state = IngestState::new();
        let out = ingest(
            &Event::EndFile(jfn_mpv::EndFileReason::Eof),
            &state,
            &ctx(Scale::ONE),
        );
        let IngestOut::Input(Input::EndFile { reason, .. }) = &out[0] else {
            panic!();
        };
        assert_eq!(*reason, EndReason::Eof);
    }

    #[test]
    fn file_loaded_emits_input() {
        let state = IngestState::new();
        let out = ingest(&Event::FileLoaded, &state, &ctx(Scale::ONE));
        assert!(matches!(out[0], IngestOut::Input(Input::FileLoaded)));
    }
}
