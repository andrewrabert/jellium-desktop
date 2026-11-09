use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::{Inner, now_ns, tasks};
use crate::frame_rate::FrameRate;

impl Inner {
    pub(crate) fn set_frame_rate(&self, rate: FrameRate) {
        if !self.browser_alive() {
            return;
        }
        self.cef_set_windowless_frame_rate(rate.get());
    }

    pub(super) fn apply_pending_resize(self: &Arc<Self>) {
        self.resize_scheduled.store(false, Ordering::Release);
        if !self.browser_alive() {
            return;
        }
        let now = now_ns();
        self.last_was_resized_ns.store(now, Ordering::Release);
        self.paint_scheduler.during_resize(self, || {
            self.notify_screen_info_changed();
            self.cef_was_resized();
        });
    }

    pub(crate) fn resize(self: &Arc<Self>, size: jfn_platform_abi::SurfaceSize) {
        let logical = size.extent.logical();
        self.width.store(logical.w, Ordering::Release);
        self.height.store(logical.h, Ordering::Release);
        self.scale.store(Some(size.extent.scale()));

        // Wayland viewport must update on every configure (not debounced) or
        // src/dst go stale.
        self.surface().resize(size);

        if !self.browser_alive() {
            return;
        }

        let now = now_ns();
        // A display that reports no refresh spaces nothing: the resize applies
        // on the spot rather than wait out an interval this process invented.
        let period_ns = jfn_gpu_paint::refresh_interval()
            .map_or(0, |period| period.as_nanos().min(i64::MAX as u128) as i64);
        let last = self.last_was_resized_ns.load(Ordering::Acquire);
        self.paint_scheduler.during_resize(self, || {
            if now - last >= period_ns {
                self.last_was_resized_ns.store(now, Ordering::Release);
                self.notify_screen_info_changed();
                self.cef_was_resized();
                return;
            }
            if self
                .resize_scheduled
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                let delay_ms = ((period_ns - (now - last)) / 1_000_000).max(1);
                tasks::post_apply_resize(Arc::clone(self), delay_ms);
            }
        });
    }

    pub(crate) fn set_refresh_rate(self: &Arc<Self>, target: FrameRate) {
        tasks::post_set_refresh(Arc::clone(self), target);
    }

    pub(super) fn apply_set_refresh(&self, target: FrameRate) {
        self.frame_rate.store(Some(target));
        if !self.paint_scheduler.refresh_rate_changed(target) {
            self.set_frame_rate(target);
        }
    }
}
