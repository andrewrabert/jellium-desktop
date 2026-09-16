use std::ffi::c_void;

use dispatch2::DispatchQueue;

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

pub(crate) fn run_on_main_sync<F>(f: F)
where
    F: FnOnce(),
{
    if is_main_thread() {
        f();
        return;
    }
    extern "C" fn sync_trampoline<F: FnOnce()>(ctx: *mut c_void) {
        let slot = unsafe { &mut *(ctx as *mut Option<F>) };
        if let Some(f) = slot.take() {
            f();
        }
    }
    let mut slot = Some(f);
    let ctx = std::ptr::from_mut(&mut slot) as *mut c_void;
    unsafe { DispatchQueue::main().exec_sync_f(ctx, sync_trampoline::<F>) };
}

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

pub(crate) fn post_to_main<F>(f: F)
where
    F: FnOnce() + 'static,
{
    unsafe { DispatchQueue::main().exec_async_f(into_ctx(f), trampoline) };
}

pub(crate) fn wake_main_queue() {
    post_to_main(|| {});
}
