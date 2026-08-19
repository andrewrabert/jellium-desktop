//! CEF-context readiness signal.
//!
//! Readiness means the CEF UI thread is running, not that `CefInitialize`
//! returned: a `CefURLRequest` built before that thread exists is null.
//! The mark is therefore posted onto TID_UI and lands when that thread runs it.
//!
//! Anything that must not touch CEF before then parks a callback here; the
//! shell overlay's first probe is the reason it exists.

use cef::rc::Rc;
use cef::{ImplTask, Task, ThreadId, WrapTask, post_task, wrap_task};
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

static READY: AtomicBool = AtomicBool::new(false);
static WAITING: Mutex<Vec<Box<dyn FnOnce() + Send>>> = Mutex::new(Vec::new());

pub fn cef_ready() -> bool {
    READY.load(Ordering::Acquire)
}

/// Runs `f` once the CEF context is initialized, inline if it already is.
pub fn on_cef_ready(f: Box<dyn FnOnce() + Send>) {
    if cef_ready() {
        f();
        return;
    }
    let mut waiting = WAITING.lock();
    if READY.load(Ordering::Acquire) {
        drop(waiting);
        f();
        return;
    }
    waiting.push(f);
}

/// Posts the readiness mark onto TID_UI. Called once, right after
/// `CefInitialize` returns; the mark itself lands on the UI thread.
pub fn post_cef_ready() {
    let mut task = MarkReadyTask::new();
    let _ = post_task(ThreadId::UI, Some(&mut task));
}

wrap_task! {
    struct MarkReadyTask {
    }
    impl Task {
        fn execute(&self) {
            mark_cef_ready();
        }
    }
}

fn mark_cef_ready() {
    let waiting = {
        let mut waiting = WAITING.lock();
        READY.store(true, Ordering::Release);
        std::mem::take(&mut *waiting)
    };
    for f in waiting {
        f();
    }
}
