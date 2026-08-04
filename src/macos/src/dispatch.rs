//! The crate's only GCD declarations, plus the main-queue hop helpers every
//! module goes through.

use std::ffi::c_void;

use dispatch2::DispatchQueue;

/// Returns true if the current thread is the AppKit main thread.
pub(crate) fn is_main_thread() -> bool {
    objc2::MainThreadMarker::new().is_some()
}

extern "C" fn trampoline(ctx: *mut c_void) {
    let dbl_box: Box<Box<dyn FnOnce()>> = unsafe { Box::from_raw(ctx as *mut _) };
    (*dbl_box)();
}

fn into_ctx<F: FnOnce() + 'static>(f: F) -> *mut c_void {
    let boxed: Box<dyn FnOnce()> = Box::new(f);
    Box::into_raw(Box::new(boxed)) as *mut c_void
}

/// Run `f` on the main queue, blocking until it returns. Used for layer-tree
/// mutations the caller needs applied before it continues. Runs inline when
/// already on the main thread.
///
/// The closure runs strictly on the main thread; raw pointers it captures
/// don't actually cross threads, so there is no `Send` bound.
pub(crate) fn run_on_main_sync<F>(f: F)
where
    F: FnOnce(),
{
    if is_main_thread() {
        f();
        return;
    }
    // dispatch_sync_f blocks until the work item returns, so a stack slot
    // outlives the call and the closure needs no `'static` bound.
    extern "C" fn sync_trampoline<F: FnOnce()>(ctx: *mut c_void) {
        let slot = unsafe { &mut *(ctx as *mut Option<F>) };
        if let Some(f) = slot.take() {
            f();
        }
    }
    let mut slot = Some(f);
    let ctx = std::ptr::from_mut(&mut slot) as *mut c_void;
    // SAFETY: the trampoline reads exactly the slot type it is instantiated
    // with, and the slot outlives the blocking call.
    unsafe { DispatchQueue::main().exec_sync_f(ctx, sync_trampoline::<F>) };
}

/// Post `f` to the main queue. Fire-and-forget, for callers that don't need
/// ordering. Runs inline when already on the main thread.
pub(crate) fn run_on_main_async<F>(f: F)
where
    F: FnOnce() + 'static,
{
    if is_main_thread() {
        f();
        return;
    }
    post_to_main(f);
}

/// Post `f` to the main queue, never inline, so the caller's frame unwinds
/// before `f` runs.
pub(crate) fn post_to_main<F>(f: F)
where
    F: FnOnce() + 'static,
{
    // SAFETY: the trampoline reclaims the box `into_ctx` leaked.
    unsafe { DispatchQueue::main().exec_async_f(into_ctx(f), trampoline) };
}

/// Post an empty work item; the side effect is the run-loop wake.
pub(crate) fn wake_main_queue() {
    post_to_main(|| {});
}
