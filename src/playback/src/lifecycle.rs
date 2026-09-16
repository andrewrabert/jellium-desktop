use std::sync::atomic::{AtomicPtr, Ordering};

static SET_VISIBLE: AtomicPtr<()> = AtomicPtr::new(std::ptr::null_mut());
static SUSPEND: AtomicPtr<()> = AtomicPtr::new(std::ptr::null_mut());
static RESUME: AtomicPtr<()> = AtomicPtr::new(std::ptr::null_mut());

pub fn jfn_lifecycle_set_handlers(visible: fn(bool), suspend: fn(), resume: fn()) {
    SET_VISIBLE.store(visible as *mut (), Ordering::Release);
    SUSPEND.store(suspend as *mut (), Ordering::Release);
    RESUME.store(resume as *mut (), Ordering::Release);
}

pub fn jfn_lifecycle_set_visible(visible: bool) {
    let p = SET_VISIBLE.load(Ordering::Acquire);
    if !p.is_null() {
        let f: fn(bool) = unsafe { std::mem::transmute(p) };
        f(visible);
    }
}

pub fn jfn_lifecycle_suspend() {
    let p = SUSPEND.load(Ordering::Acquire);
    if !p.is_null() {
        let f: fn() = unsafe { std::mem::transmute(p) };
        f();
    }
}

pub fn jfn_lifecycle_resume() {
    let p = RESUME.load(Ordering::Acquire);
    if !p.is_null() {
        let f: fn() = unsafe { std::mem::transmute(p) };
        f();
    }
}
