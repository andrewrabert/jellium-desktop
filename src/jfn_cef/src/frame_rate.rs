use std::num::NonZeroU32;

use jfn_gpu_paint::RefreshRate;

const MILLIHERTZ_PER_HERTZ: u64 = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FrameRate(i32);

impl From<RefreshRate> for FrameRate {
    fn from(rate: RefreshRate) -> Self {
        let hz = (rate.millihertz() + MILLIHERTZ_PER_HERTZ / 2) / MILLIHERTZ_PER_HERTZ;
        Self(i32::try_from(hz).unwrap_or(i32::MAX).max(1))
    }
}

impl FrameRate {
    pub(crate) fn get(self) -> i32 {
        self.0
    }

    pub(crate) fn times(self, factor: NonZeroU32) -> Self {
        Self(
            self.0
                .saturating_mul(i32::try_from(factor.get()).unwrap_or(i32::MAX)),
        )
    }
}
