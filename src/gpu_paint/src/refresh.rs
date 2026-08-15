//! The process's one refresh interval, reported by the platform.

use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::Duration;

use parking_lot::Mutex;

/// Where a refresh interval came from. A compositor-reported output mode
/// outranks mpv's report, so a later mpv value never overwrites one the
/// platform gave.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum RefreshSource {
    MpvDisplayFps,
    OutputMode,
}

impl RefreshSource {
    fn rank(self) -> u8 {
        match self {
            RefreshSource::MpvDisplayFps => 1,
            RefreshSource::OutputMode => 2,
        }
    }
}

/// Nanoseconds of the published interval; zero while none has been reported.
static INTERVAL_NANOS: AtomicU64 = AtomicU64::new(0);
/// The rank of the source that published it; zero while none has.
static SOURCE_RANK: AtomicU8 = AtomicU8::new(0);
/// Serialises the compare-and-publish, so two reports racing cannot interleave
/// a rank with another source's interval.
static PUBLISH: Mutex<()> = Mutex::new(());

/// Subscribers woken after every report that changed the published interval.
static SUBSCRIBERS: Mutex<Vec<fn()>> = Mutex::new(Vec::new());

/// Registers `on_change`, called after every report that changed the published
/// interval, so work that has no cadence until a refresh is known is woken when
/// one arrives.
pub fn subscribe(on_change: fn()) {
    SUBSCRIBERS.lock().push(on_change);
}

/// Publishes `interval` as the display's refresh, keeping the highest-ranked
/// source reported so far.
pub fn report_refresh(source: RefreshSource, interval: Duration) {
    let nanos = interval.as_nanos();
    if nanos == 0 || nanos > u128::from(u64::MAX) {
        return;
    }
    let changed = {
        let _publishing = PUBLISH.lock();
        if SOURCE_RANK.load(Ordering::Relaxed) > source.rank() {
            return;
        }
        let nanos = nanos as u64;
        let changed = INTERVAL_NANOS.swap(nanos, Ordering::Relaxed) != nanos;
        SOURCE_RANK.store(source.rank(), Ordering::Relaxed);
        changed
    };
    if changed {
        notify();
    }
}

/// Runs every subscriber with the publish lock released: each one reads the
/// interval back.
fn notify() {
    let subscribers: Vec<fn()> = SUBSCRIBERS.lock().clone();
    for on_change in subscribers {
        on_change();
    }
}

/// The display's reported refresh interval, or `None` while no platform has
/// reported one.
pub fn refresh_interval() -> Option<Duration> {
    match INTERVAL_NANOS.load(Ordering::Relaxed) {
        0 => None,
        nanos => Some(Duration::from_nanos(nanos)),
    }
}
