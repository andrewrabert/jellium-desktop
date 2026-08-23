use cef::rc::Rc;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicU64, Ordering};

use cef::{
    CefString, Frame, ImplFrame, ImplTask, Task, ThreadId, WrapTask, post_delayed_task, post_task,
    wrap_task,
};

use crate::client::{Inner, now_ns};
use crate::frame_rate::FrameRate;
use crossbeam_utils::atomic::AtomicCell;

const BOOST_MULTIPLIER: NonZeroU32 = match NonZeroU32::new(2) {
    Some(n) => n,
    None => unreachable!(),
};
const INVALIDATE_TICK_LIMIT: i32 = 1000;
const SKIP_PAINTS_AFTER_RESIZE: i32 = 1;

// After each window resize, keep producing compositor frames until
// `CefLayer::noteStableSize` calls `window.__cefStopRaf`.
const JS_PAINT_NUDGE: &str = r#"
(function () {
    console.debug('CEF paint nudge installed');
    var running = false;
    var stop = false;
    function tick() {
        if (stop) {
            stop = false;
            running = false;
            return;
        }
        requestAnimationFrame(tick);
    }
    window.addEventListener('resize', function () {
        stop = false;
        if (!running) {
            running = true;
            requestAnimationFrame(tick);
        }
    });
    window.__cefStopRaf = function () { stop = true; };
})();
"#;

struct PaintState {
    /// The rate the boost displaced; `None` while no boost is live.
    saved_frame_rate: AtomicCell<Option<FrameRate>>,
    resize_gen: AtomicU64,
    invalidate_running: AtomicBool,
    invalidate_stop: AtomicBool,
    invalidate_tick_count: AtomicI32,
    last_paint_gen: AtomicU64,
    paints_since_resize: AtomicI32,
    pump_paint_count: AtomicI32,
    last_skip_reset_ns: AtomicI64,
}

impl PaintState {
    fn new() -> Self {
        Self {
            saved_frame_rate: AtomicCell::new(None),
            resize_gen: AtomicU64::new(0),
            invalidate_running: AtomicBool::new(false),
            invalidate_stop: AtomicBool::new(false),
            invalidate_tick_count: AtomicI32::new(0),
            last_paint_gen: AtomicU64::new(0),
            paints_since_resize: AtomicI32::new(SKIP_PAINTS_AFTER_RESIZE),
            pump_paint_count: AtomicI32::new(0),
            last_skip_reset_ns: AtomicI64::new(0),
        }
    }

    fn begin_resize(&self) {
        self.resize_gen.fetch_add(1, Ordering::AcqRel);
    }

    fn stop_invalidate_loop(&self) {
        self.invalidate_stop.store(true, Ordering::Release);
    }

    fn start_invalidate_loop(&self) -> bool {
        self.invalidate_stop.store(false, Ordering::Release);
        if self
            .invalidate_running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        self.invalidate_tick_count.store(0, Ordering::Release);
        true
    }

    fn update_boost_saved_frame_rate(&self, target: FrameRate) -> bool {
        if self.saved_frame_rate.load().is_none() {
            return false;
        }
        self.saved_frame_rate.store(Some(target));
        true
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PaintMode {
    shared_textures: bool,
}

impl PaintMode {
    pub(crate) fn new(shared_textures: bool) -> Self {
        Self { shared_textures }
    }

    pub(crate) fn shared_textures(&self) -> bool {
        self.shared_textures
    }

    pub(crate) fn make_scheduler(&self) -> PaintScheduler {
        PaintScheduler::new(self.shared_textures)
    }
}

#[derive(Clone)]
pub(crate) struct PaintScheduler {
    mode: Arc<dyn PaintSchedulerMode>,
}

trait PaintSchedulerMode: Send + Sync {
    fn before_resize(&self) {}
    fn after_resize(&self, _scheduler: PaintScheduler, _inner: &Arc<Inner>) {}
    fn before_close(&self) {}
    fn refresh_rate_changed(&self, _target: FrameRate) -> bool {
        false
    }
    fn verdict(&self, _inner: &Inner) -> Verdict {
        Verdict::Present
    }
    fn kick_task(&self, _scheduler: PaintScheduler, _inner: &Arc<Inner>) {}
    fn tick_task(&self, _scheduler: PaintScheduler, _inner: &Arc<Inner>) {}
}

impl PaintScheduler {
    fn new(shared_textures: bool) -> Self {
        let mode: Arc<dyn PaintSchedulerMode> = if shared_textures {
            Arc::new(ActivePaintScheduler {
                state: PaintState::new(),
            })
        } else {
            Arc::new(PassivePaintScheduler)
        };
        Self { mode }
    }

    pub(crate) fn on_context_created(shared_textures: bool, frame: &Frame) {
        if !shared_textures {
            return;
        }
        let code = CefString::from(JS_PAINT_NUDGE);
        let url_uf = frame.url();
        let url = CefString::from(&url_uf);
        frame.execute_java_script(Some(&code), Some(&url), 0);
    }

    pub(crate) fn during_resize<R>(&self, inner: &Arc<Inner>, resize: impl FnOnce() -> R) -> R {
        self.mode.before_resize();
        let result = resize();
        self.mode.after_resize(self.clone(), inner);
        result
    }

    pub(crate) fn before_close(&self) {
        self.mode.before_close();
    }

    pub(crate) fn refresh_rate_changed(&self, target: FrameRate) -> bool {
        self.mode.refresh_rate_changed(target)
    }

    /// [`Verdict::Supersede`] is returned only while the invalidate loop that
    /// produces the successor is running.
    pub(crate) fn verdict(&self, inner: &Inner) -> Verdict {
        self.mode.verdict(inner)
    }

    fn kick_task(&self, inner: &Arc<Inner>) {
        self.mode.kick_task(self.clone(), inner);
    }

    fn tick_task(&self, inner: &Arc<Inner>) {
        self.mode.tick_task(self.clone(), inner);
    }
}

struct PassivePaintScheduler;

impl PaintSchedulerMode for PassivePaintScheduler {}

struct ActivePaintScheduler {
    state: PaintState,
}

impl PaintSchedulerMode for ActivePaintScheduler {
    fn before_resize(&self) {
        self.state.begin_resize();
    }

    fn after_resize(&self, scheduler: PaintScheduler, inner: &Arc<Inner>) {
        inner.invalidate_view();
        start_invalidate_loop(scheduler, &self.state, inner);
    }

    fn before_close(&self) {
        self.state.stop_invalidate_loop();
    }

    fn refresh_rate_changed(&self, target: FrameRate) -> bool {
        self.state.update_boost_saved_frame_rate(target)
    }

    fn verdict(&self, inner: &Inner) -> Verdict {
        active_verdict(&self.state, inner)
    }

    fn kick_task(&self, scheduler: PaintScheduler, inner: &Arc<Inner>) {
        active_kick_apply(scheduler, &self.state, inner);
    }

    fn tick_task(&self, scheduler: PaintScheduler, inner: &Arc<Inner>) {
        active_invalidate_tick(scheduler, &self.state, inner);
    }
}

fn start_invalidate_loop(scheduler: PaintScheduler, state: &PaintState, inner: &Arc<Inner>) {
    if !state.start_invalidate_loop() {
        return;
    }
    let next = Arc::clone(inner);
    let mut task = KickTask::new(scheduler, next);
    let _ = post_task(ThreadId::UI, Some(&mut task));
}

fn active_kick_apply(scheduler: PaintScheduler, state: &PaintState, inner: &Arc<Inner>) {
    // Boost CEF compositor rate while the loop is live — JS rAF ties to
    // compositor rate, so this speeds up convergence to post-resize dims.
    if let Some(fps) = inner.frame_rate.load()
        && inner.browser_alive()
        && state.saved_frame_rate.load().is_none()
    {
        state.saved_frame_rate.store(Some(fps));
        inner.set_frame_rate(fps.times(BOOST_MULTIPLIER));
    }
    active_invalidate_tick(scheduler, state, inner);
}

/// Ends the invalidate loop: restores the frame rate it boosted and clears the
/// running flag. Both exits — the stop flag and a display that reports no
/// refresh interval — go through here.
fn stop_invalidate(state: &PaintState, inner: &Arc<Inner>) {
    if let Some(saved) = state.saved_frame_rate.swap(None)
        && inner.browser_alive()
    {
        inner.set_frame_rate(saved);
    }
    state.invalidate_running.store(false, Ordering::Release);
}

fn active_invalidate_tick(scheduler: PaintScheduler, state: &PaintState, inner: &Arc<Inner>) {
    if state.invalidate_tick_count.fetch_add(1, Ordering::AcqRel) + 1 > INVALIDATE_TICK_LIMIT {
        state.invalidate_stop.store(true, Ordering::Release);
    }
    if state.invalidate_stop.load(Ordering::Acquire) {
        stop_invalidate(state, inner);
        return;
    }
    if inner.browser_alive() {
        inner.invalidate_view();
        let external_bf = jfn_platform_abi::try_get()
            .and_then(|p| p.cef_host())
            .is_some_and(|h| h.external_begin_frame());
        if external_bf {
            inner.send_external_begin_frame();
        }
    }
    // The loop ticks at the display's own refresh; a display that reports none
    // spaces nothing, so the loop stops rather than run at a rate this process
    // invented.
    let Some(period) = jfn_gpu_paint::refresh_interval() else {
        stop_invalidate(state, inner);
        return;
    };
    let delay_ms = (period.as_millis() as i64).max(1);
    let next = Arc::clone(inner);
    let mut task = TickTask::new(scheduler, next);
    let _ = post_delayed_task(ThreadId::UI, Some(&mut task), delay_ms);
}

/// What the scheduler decided about one produced frame.
pub(crate) enum Verdict {
    Present,
    /// The frame is elided; the producer named here owes the successor.
    Supersede,
}

fn active_verdict(state: &PaintState, inner: &Inner) -> Verdict {
    let cur_gen = state.resize_gen.load(Ordering::Acquire);
    let last_gen = state.last_paint_gen.load(Ordering::Acquire);
    if cur_gen != last_gen {
        state.last_paint_gen.store(cur_gen, Ordering::Release);
        // Rate-clamp the skip-counter reset. Continuous drag bumps gen
        // many times per second; resetting on every bump would keep
        // wiping the counter before any paint clears the skip threshold.
        let now_ns_val = now_ns();
        let period_ns = jfn_gpu_paint::refresh_interval().map_or(i64::MAX, |period| {
            period.as_nanos().min(i64::MAX as u128) as i64
        });
        if now_ns_val - state.last_skip_reset_ns.load(Ordering::Acquire) >= period_ns {
            state
                .last_skip_reset_ns
                .store(now_ns_val, Ordering::Release);
            let pump = inner.frame_rate.load().map_or(0, |fps| 1 + fps.get());
            state.pump_paint_count.store(pump, Ordering::Release);
            state.paints_since_resize.store(0, Ordering::Release);
        }
    }
    let count = state.paints_since_resize.fetch_add(1, Ordering::AcqRel) + 1;
    let pump = state.pump_paint_count.load(Ordering::Acquire);
    // The skip is only ever taken while the invalidate loop is running, so the
    // frame it elides has a successor already on the way.
    let verdict = if count > SKIP_PAINTS_AFTER_RESIZE {
        Verdict::Present
    } else {
        Verdict::Supersede
    };
    if pump > 0 && count == pump {
        // Pumped enough frames — signal stop to host Invalidate loop and
        // renderer's rAF loop. Counter remains past pump so subsequent
        // paints don't re-fire.
        state.invalidate_stop.store(true, Ordering::Release);
        inner.exec_js("window.__cefStopRaf && window.__cefStopRaf();");
    }
    verdict
}

wrap_task! {
    struct KickTask {
        scheduler: PaintScheduler,
        inner: Arc<Inner>,
    }
    impl Task {
        fn execute(&self) {
            self.scheduler.kick_task(&self.inner);
        }
    }
}

wrap_task! {
    struct TickTask {
        scheduler: PaintScheduler,
        inner: Arc<Inner>,
    }
    impl Task {
        fn execute(&self) {
            self.scheduler.tick_task(&self.inner);
        }
    }
}
