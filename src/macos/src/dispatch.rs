//! The crate's only GCD declarations, plus the main-queue hop helpers every
//! module goes through.

use std::ffi::c_void;

unsafe extern "C" {
    // dispatch_get_main_queue() is an inline C function that returns
    // &_dispatch_main_q, so the exported symbol is the queue object itself.
    static _dispatch_main_q: c_void;

    fn dispatch_async_f(
        queue: *mut c_void,
        ctx: *mut c_void,
        work: unsafe extern "C" fn(*mut c_void),
    );

    fn dispatch_sync_f(
        queue: *mut c_void,
        ctx: *mut c_void,
        work: unsafe extern "C" fn(*mut c_void),
    );
}

#[inline]
fn main_queue() -> *mut c_void {
    std::ptr::addr_of!(_dispatch_main_q) as *mut c_void
}

/// Returns true if the current thread is the AppKit main thread. Avoids
/// pulling in `objc2-foundation` `MainThreadMarker` infrastructure for a
/// single check.
pub(crate) fn is_main_thread() -> bool {
    unsafe {
        let cls = objc2::class!(NSThread);
        let b: bool = objc2::msg_send![cls, isMainThread];
        b
    }
}

unsafe extern "C" fn trampoline(ctx: *mut c_void) {
    unsafe {
        let dbl_box: Box<Box<dyn FnOnce()>> = Box::from_raw(ctx as *mut _);
        (*dbl_box)();
    }
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
    unsafe extern "C" fn sync_trampoline<F: FnOnce()>(ctx: *mut c_void) {
        let slot = unsafe { &mut *(ctx as *mut Option<F>) };
        if let Some(f) = slot.take() {
            f();
        }
    }
    let mut slot = Some(f);
    let ctx = std::ptr::from_mut(&mut slot) as *mut c_void;
    unsafe { dispatch_sync_f(main_queue(), ctx, sync_trampoline::<F>) };
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
    unsafe { dispatch_async_f(main_queue(), into_ctx(f), trampoline) };
}

/// Post an empty work item; the side effect is the run-loop wake.
pub(crate) fn wake_main_queue() {
    post_to_main(|| {});
}
