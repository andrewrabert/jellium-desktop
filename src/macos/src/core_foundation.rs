//! The crate's only CoreFoundation declarations.

use std::ffi::{c_char, c_void};

#[allow(non_camel_case_types)]
pub(crate) type CFIndex = isize;
#[allow(non_camel_case_types)]
pub(crate) type CFAbsoluteTime = f64;
#[allow(non_camel_case_types)]
pub(crate) type CFTimeInterval = f64;
#[allow(non_camel_case_types)]
pub(crate) type CFOptionFlags = usize;
#[allow(non_camel_case_types)]
pub(crate) type CFHashCode = usize;
#[allow(non_camel_case_types)]
pub(crate) type Boolean = u8;

pub(crate) type CFAllocatorRef = *const c_void;
pub(crate) type CFRunLoopRef = *mut c_void;
pub(crate) type CFRunLoopSourceRef = *mut c_void;
pub(crate) type CFRunLoopTimerRef = *mut c_void;
pub(crate) type CFStringRef = *const c_void;
pub(crate) type CFTypeRef = *const c_void;

pub(crate) const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

#[repr(C)]
pub(crate) struct CFRunLoopSourceContext {
    pub(crate) version: CFIndex,
    pub(crate) info: *mut c_void,
    pub(crate) retain: Option<unsafe extern "C" fn(*const c_void) -> *const c_void>,
    pub(crate) release: Option<unsafe extern "C" fn(*const c_void)>,
    pub(crate) copy_description: Option<unsafe extern "C" fn(*const c_void) -> CFStringRef>,
    pub(crate) equal: Option<unsafe extern "C" fn(*const c_void, *const c_void) -> Boolean>,
    pub(crate) hash: Option<unsafe extern "C" fn(*const c_void) -> CFHashCode>,
    pub(crate) schedule: Option<unsafe extern "C" fn(*mut c_void, CFRunLoopRef, CFStringRef)>,
    pub(crate) cancel: Option<unsafe extern "C" fn(*mut c_void, CFRunLoopRef, CFStringRef)>,
    pub(crate) perform: Option<unsafe extern "C" fn(*mut c_void)>,
}

#[repr(C)]
pub(crate) struct CFRunLoopTimerContext {
    pub(crate) version: CFIndex,
    pub(crate) info: *mut c_void,
    pub(crate) retain: Option<unsafe extern "C" fn(*const c_void) -> *const c_void>,
    pub(crate) release: Option<unsafe extern "C" fn(*const c_void)>,
    pub(crate) copy_description: Option<unsafe extern "C" fn(*const c_void) -> CFStringRef>,
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    pub(crate) static kCFRunLoopCommonModes: CFStringRef;
    pub(crate) static kCFRunLoopDefaultMode: CFStringRef;
    // kCFAllocatorNull as contents_deallocator: CF won't free our static byte
    // buffers.
    pub(crate) static kCFAllocatorNull: CFAllocatorRef;

    pub(crate) fn CFRunLoopGetMain() -> CFRunLoopRef;
    pub(crate) fn CFRunLoopWakeUp(rl: CFRunLoopRef);
    pub(crate) fn CFRunLoopRunInMode(
        mode: CFStringRef,
        seconds: f64,
        return_after_source_handled: i32,
    ) -> i32;
    pub(crate) fn CFRunLoopAddSource(
        rl: CFRunLoopRef,
        source: CFRunLoopSourceRef,
        mode: CFStringRef,
    );
    pub(crate) fn CFRunLoopAddTimer(rl: CFRunLoopRef, timer: CFRunLoopTimerRef, mode: CFStringRef);
    pub(crate) fn CFRunLoopSourceCreate(
        allocator: CFAllocatorRef,
        order: CFIndex,
        context: *mut CFRunLoopSourceContext,
    ) -> CFRunLoopSourceRef;
    pub(crate) fn CFRunLoopSourceSignal(source: CFRunLoopSourceRef);
    pub(crate) fn CFRunLoopSourceInvalidate(source: CFRunLoopSourceRef);
    pub(crate) fn CFRunLoopTimerCreate(
        allocator: CFAllocatorRef,
        fire_date: CFAbsoluteTime,
        interval: CFTimeInterval,
        flags: CFOptionFlags,
        order: CFIndex,
        callout: Option<unsafe extern "C" fn(CFRunLoopTimerRef, *mut c_void)>,
        context: *mut CFRunLoopTimerContext,
    ) -> CFRunLoopTimerRef;
    pub(crate) fn CFRunLoopTimerSetNextFireDate(
        timer: CFRunLoopTimerRef,
        fire_date: CFAbsoluteTime,
    );
    pub(crate) fn CFRunLoopTimerInvalidate(timer: CFRunLoopTimerRef);
    pub(crate) fn CFAbsoluteTimeGetCurrent() -> CFAbsoluteTime;
    pub(crate) fn CFStringCreateWithCStringNoCopy(
        alloc: CFAllocatorRef,
        c_str: *const c_char,
        encoding: u32,
        contents_deallocator: CFAllocatorRef,
    ) -> CFStringRef;
    pub(crate) fn CFRelease(cf: CFTypeRef);
}
