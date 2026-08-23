//! The process's one refresh interval, reported by the platform.

use std::num::NonZeroU64;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::Duration;

use parking_lot::Mutex;

/// Nanoseconds per second, times millihertz per hertz: the numerator
/// `period` divides by a rate in millihertz.
const NANOS_PER_SECOND_MILLIHERTZ: u128 = 1_000_000_000_000;

/// Millihertz per hertz.
const MILLIHERTZ_PER_HERTZ: f64 = 1000.0;

/// A display's refresh rate, exact in millihertz — the unit
/// `wl_output.mode` reports and the finer of the two units a source uses.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct RefreshRate(NonZeroU64);

impl RefreshRate {
    /// `hz` in millihertz, integer round-half-up.
    ///
    /// `None` when `hz` is not finite or not greater than zero, and when the
    /// period it names is under one nanosecond.
    pub fn from_hz(hz: f64) -> Option<RefreshRate> {
        if !hz.is_finite() || hz <= 0.0 {
            return None;
        }
        let millihertz = (hz * MILLIHERTZ_PER_HERTZ).floor() as i128;
        let millihertz = if (hz * MILLIHERTZ_PER_HERTZ) - millihertz as f64 >= 0.5 {
            millihertz + 1
        } else {
            millihertz
        };
        RefreshRate::from_millihertz_u128(u128::try_from(millihertz).ok()?)
    }

    /// `None` for a rate at or below zero.
    pub fn from_millihertz(mhz: i32) -> Option<RefreshRate> {
        RefreshRate::from_millihertz_u128(u128::try_from(mhz).ok()?)
    }

    fn from_millihertz_u128(mhz: u128) -> Option<RefreshRate> {
        if mhz > NANOS_PER_SECOND_MILLIHERTZ {
            return None;
        }
        NonZeroU64::new(u64::try_from(mhz).ok()?).map(RefreshRate)
    }

    /// This rate in exact millihertz.
    pub fn millihertz(self) -> u64 {
        self.0.get()
    }

    /// The frame period this rate names: one second divided by the rate,
    /// integer round-half-up in nanoseconds. The only place a period is
    /// derived from a rate.
    pub fn period(self) -> Duration {
        let den = u128::from(self.0.get());
        let nanos = (NANOS_PER_SECOND_MILLIHERTZ * 2 + den) / (den * 2);
        Duration::from_nanos(nanos as u64)
    }
}

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

/// Millihertz of the published rate; zero while none has been reported.
static RATE_MILLIHERTZ: AtomicU64 = AtomicU64::new(0);
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

/// Publishes the period `rate` names as the display's refresh, keeping the
/// highest-ranked source reported so far.
pub fn report_refresh(source: RefreshSource, rate: RefreshRate) {
    let nanos = rate.period().as_nanos();
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
        RATE_MILLIHERTZ.store(rate.millihertz(), Ordering::Relaxed);
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

/// The highest-ranked refresh rate published so far, or `None` while no
/// accepted source has reported one.
pub fn current_refresh_rate() -> Option<RefreshRate> {
    NonZeroU64::new(RATE_MILLIHERTZ.load(Ordering::Relaxed)).map(RefreshRate)
}

/// The display's reported refresh interval, or `None` while no platform has
/// reported one.
pub fn refresh_interval() -> Option<Duration> {
    match INTERVAL_NANOS.load(Ordering::Relaxed) {
        0 => None,
        nanos => Some(Duration::from_nanos(nanos)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sixty_hertz_and_sixty_thousand_millihertz_name_one_period() {
        assert_eq!(
            RefreshRate::from_hz(60.0).map(RefreshRate::period),
            RefreshRate::from_millihertz(60_000).map(RefreshRate::period)
        );
        assert_eq!(
            RefreshRate::from_hz(60.0).map(RefreshRate::period),
            Some(Duration::from_nanos(16_666_667))
        );
    }

    #[test]
    fn a_rate_whose_period_is_under_a_nanosecond_is_rejected() {
        assert_eq!(RefreshRate::from_hz(1e9 + 1.0), None);
        assert_eq!(RefreshRate::from_hz(0.0), None);
        assert_eq!(RefreshRate::from_hz(f64::NAN), None);
        assert_eq!(RefreshRate::from_millihertz(0), None);
        assert_eq!(RefreshRate::from_millihertz(-1), None);
    }
}
