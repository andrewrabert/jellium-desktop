use std::num::NonZeroU64;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::Duration;

mod subscribers;
use parking_lot::Mutex;
use std::sync::LazyLock;
use subscribers::Subscribers;
pub use subscribers::Subscription;

const NANOS_PER_SECOND_MILLIHERTZ: u128 = 1_000_000_000_000;

const MILLIHERTZ_PER_HERTZ: f64 = 1000.0;

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct RefreshRate(NonZeroU64);

impl RefreshRate {
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

    pub fn from_millihertz(mhz: i32) -> Option<RefreshRate> {
        RefreshRate::from_millihertz_u128(u128::try_from(mhz).ok()?)
    }

    fn from_millihertz_u128(mhz: u128) -> Option<RefreshRate> {
        if mhz > NANOS_PER_SECOND_MILLIHERTZ {
            return None;
        }
        NonZeroU64::new(u64::try_from(mhz).ok()?).map(RefreshRate)
    }

    pub fn millihertz(self) -> u64 {
        self.0.get()
    }

    pub fn period(self) -> Duration {
        let den = u128::from(self.0.get());
        let nanos = (NANOS_PER_SECOND_MILLIHERTZ * 2 + den) / (den * 2);
        Duration::from_nanos(nanos as u64)
    }
}

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

static RATE_MILLIHERTZ: AtomicU64 = AtomicU64::new(0);
static INTERVAL_NANOS: AtomicU64 = AtomicU64::new(0);
static SOURCE_RANK: AtomicU8 = AtomicU8::new(0);
static PUBLISH: Mutex<()> = Mutex::new(());

static SUBSCRIBERS: LazyLock<Subscribers> = LazyLock::new(Subscribers::new);

pub fn subscribe(on_change: fn()) -> Subscription {
    SUBSCRIBERS.subscribe(on_change)
}

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

fn notify() {
    SUBSCRIBERS.notify();
}

pub fn current_refresh_rate() -> Option<RefreshRate> {
    NonZeroU64::new(RATE_MILLIHERTZ.load(Ordering::Relaxed)).map(RefreshRate)
}

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
