//! macOS external message pump.
//!
//! Mirrors what `MessagePumpCFRunLoopBase` does internally (which CEF's
//! `MessagePumpExternal` declines to do). A `CFRunLoopSource` services
//! immediate work; a `CFRunLoopTimer` services delayed work; both are
//! installed in the main runloop's common modes.
//!
//! The wedge-recovery heuristic is preserved verbatim because it's tied to
//! a specific CEF version's `WorkDeduplicator` internals.

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};
use std::time::Instant;

use objc2_core_foundation::{
    CFAbsoluteTimeGetCurrent, CFRetained, CFRunLoop, CFRunLoopSource, CFRunLoopSourceContext,
    CFRunLoopTimer, kCFRunLoopCommonModes,
};

// ----- State ----------------------------------------------------------------

static WORK_SOURCE: AtomicPtr<CFRunLoopSource> = AtomicPtr::new(std::ptr::null_mut());
static DELAYED_TIMER: AtomicPtr<CFRunLoopTimer> = AtomicPtr::new(std::ptr::null_mut());
static PUMP_SHUTDOWN: AtomicBool = AtomicBool::new(false);
// True between on_schedule(imm) signalling the source and the source
// callback actually running. CFRunLoop has no public API to read the
// signaled bit, so we shadow it ourselves. Diagnostic only.
static WORK_SOURCE_PENDING: AtomicBool = AtomicBool::new(false);

static SCHED_IMM_CALLS: AtomicU64 = AtomicU64::new(0);
static SCHED_DELAYED_CALLS: AtomicU64 = AtomicU64::new(0);
static SOURCE_FIRED: AtomicU64 = AtomicU64::new(0);
static TIMER_FIRED: AtomicU64 = AtomicU64::new(0);
static DMLW_CALLS: AtomicU64 = AtomicU64::new(0);

// CEF's MessagePumpExternal::Run caps each Run() at 0.01f (10ms). If DoWork
// is still returning is_immediate at that point, Run breaks with the
// WorkDeduplicator state stuck at kDoWorkPending. In that state,
// WorkDeduplicator::OnWorkRequested silently drops subsequent cross-thread
// ScheduleWork calls, so OnScheduleMessagePumpWork stops firing and the
// pump wedges.
//
// The way out: re-enter cef::do_message_loop_work. ThreadController::OnWorkStarted
// unconditionally transitions state to kInDoWork. We detect the wedge by
// measuring wall-clock time. CEF's break condition is strict inequality on
// 10.0ms — anything > 10.0ms means Run was cut short.
const CEF_MAX_TIME_SLICE_MS: f64 = 10.0;

/// Mark the work source signalled and wake the main run loop.
fn signal_work_source() {
    let src = WORK_SOURCE.load(Ordering::Acquire);
    // SAFETY: the pointer is null or the source `init` stored.
    let Some(src) = (unsafe { src.as_ref() }) else {
        return;
    };
    WORK_SOURCE_PENDING.store(true, Ordering::Release);
    src.signal();
    if let Some(rl) = CFRunLoop::main() {
        rl.wake_up();
    }
}

fn pump_drain(trigger: &str) {
    if PUMP_SHUTDOWN.load(Ordering::Acquire) {
        if jfn_logging::log_enabled(jfn_logging::Category::Cef, jfn_logging::Level::Debug) {
            jfn_logging::log(
                jfn_logging::Category::Cef,
                jfn_logging::Level::Debug,
                &format!("[PUMP] drain({trigger}) skipped (shutdown)"),
            );
        }
        return;
    }

    WORK_SOURCE_PENDING.store(false, Ordering::Release);
    DMLW_CALLS.fetch_add(1, Ordering::Relaxed);
    let t0 = Instant::now();
    cef::do_message_loop_work();
    let ms = t0.elapsed().as_secs_f64() * 1e3;
    let pending = WORK_SOURCE_PENDING.load(Ordering::Acquire);

    let wedged = ms > CEF_MAX_TIME_SLICE_MS;
    if wedged && !pending {
        signal_work_source();
    }
}

unsafe extern "C-unwind" fn work_source_perform(_info: *mut c_void) {
    SOURCE_FIRED.fetch_add(1, Ordering::Relaxed);
    pump_drain("source");
}

unsafe extern "C-unwind" fn delayed_timer_fire(_timer: *mut CFRunLoopTimer, _info: *mut c_void) {
    TIMER_FIRED.fetch_add(1, Ordering::Relaxed);
    pump_drain("timer");
}

// ----- Public API -----------------------------------------------------------

pub(crate) fn init() {
    jfn_logging::log(
        jfn_logging::Category::Cef,
        jfn_logging::Level::Info,
        "[PUMP] init: installing CFRunLoopSource + CFRunLoopTimer",
    );

    let Some(main) = CFRunLoop::main() else {
        jfn_logging::log(
            jfn_logging::Category::Cef,
            jfn_logging::Level::Info,
            "[PUMP] init: no main run loop",
        );
        return;
    };

    let mut src_ctx = CFRunLoopSourceContext {
        version: 0,
        info: std::ptr::null_mut(),
        retain: None,
        release: None,
        copyDescription: None,
        equal: None,
        hash: None,
        schedule: None,
        cancel: None,
        perform: Some(work_source_perform),
    };
    // SAFETY: the context is a valid pointer for the duration of the call,
    // and CoreFoundation copies it.
    if let Some(source) = unsafe { CFRunLoopSource::new(None, 1, &mut src_ctx) } {
        main.add_source(Some(&source), unsafe { kCFRunLoopCommonModes });
        WORK_SOURCE.store(CFRetained::into_raw(source).as_ptr(), Ordering::Release);
    }

    // SAFETY: the callout matches the timer signature; the context is null.
    let timer = unsafe {
        CFRunLoopTimer::new(
            None,
            CFAbsoluteTimeGetCurrent() + 1e10,
            0.0,
            0,
            0,
            Some(delayed_timer_fire),
            std::ptr::null_mut(),
        )
    };
    if let Some(timer) = timer {
        main.add_timer(Some(&timer), unsafe { kCFRunLoopCommonModes });
        DELAYED_TIMER.store(CFRetained::into_raw(timer).as_ptr(), Ordering::Release);
    }
}

pub(crate) fn on_schedule(delay_ms: i64) {
    if PUMP_SHUTDOWN.load(Ordering::Acquire) {
        if jfn_logging::log_enabled(jfn_logging::Category::Cef, jfn_logging::Level::Debug) {
            jfn_logging::log(
                jfn_logging::Category::Cef,
                jfn_logging::Level::Debug,
                &format!("[PUMP] on_schedule({delay_ms}) SKIP(shutdown)"),
            );
        }
        return;
    }
    if delay_ms <= 0 {
        SCHED_IMM_CALLS.fetch_add(1, Ordering::Relaxed);
        signal_work_source();
    } else {
        SCHED_DELAYED_CALLS.fetch_add(1, Ordering::Relaxed);
        let timer = DELAYED_TIMER.load(Ordering::Acquire);
        // SAFETY: the pointer is null or the timer `init` stored.
        if let Some(timer) = unsafe { timer.as_ref() } {
            timer.set_next_fire_date(CFAbsoluteTimeGetCurrent() + delay_ms as f64 / 1000.0);
        }
    }
}

pub(crate) fn shutdown() {
    jfn_logging::log(
        jfn_logging::Category::Cef,
        jfn_logging::Level::Info,
        &format!(
            "[PUMP] shutdown: sched_imm={} sched_delayed={} source_fired={} timer_fired={} dmlw_calls={}",
            SCHED_IMM_CALLS.load(Ordering::Relaxed),
            SCHED_DELAYED_CALLS.load(Ordering::Relaxed),
            SOURCE_FIRED.load(Ordering::Relaxed),
            TIMER_FIRED.load(Ordering::Relaxed),
            DMLW_CALLS.load(Ordering::Relaxed),
        ),
    );
    PUMP_SHUTDOWN.store(true, Ordering::Release);

    let timer = DELAYED_TIMER.swap(std::ptr::null_mut(), Ordering::AcqRel);
    if let Some(timer) = NonNull::new(timer) {
        // SAFETY: reclaims the +1 `init` stored.
        let timer = unsafe { CFRetained::from_raw(timer) };
        timer.invalidate();
    }
    let source = WORK_SOURCE.swap(std::ptr::null_mut(), Ordering::AcqRel);
    if let Some(source) = NonNull::new(source) {
        // SAFETY: reclaims the +1 `init` stored.
        let source = unsafe { CFRetained::from_raw(source) };
        source.invalidate();
    }
}
