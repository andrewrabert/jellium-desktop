#![allow(non_snake_case)]

use std::ffi::{c_int, c_void};
use std::sync::atomic::Ordering;

use objc2::MainThreadMarker;
use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString, NSScreen, NSWorkspace};
use objc2_core_foundation::{CFRunLoop, CFString, kCFRunLoopDefaultMode};
use objc2_foundation::{NSDefaultRunLoopMode, NSString, NSURL};
use objc2_io_kit::{
    IOPMAssertionCreateWithName, IOPMAssertionID, IOPMAssertionRelease, kIOPMAssertionLevelOn,
    kIOReturnSuccess,
};

use jfn_platform_abi::geometry::{Bounds, clamp_to_bounds};
pub use jfn_platform_abi::{
    Content, DisplayBackend, JfnRect, PaintFrame, Platform, Presented, Visibility,
    VisibilityCommit, WindowDecorations,
};

use crate::dispatch::{post_to_main, run_on_main_async, wake_main_queue};
use crate::init::{jfn_macos_apply_theme_color_on_main, jfn_macos_get_window};

pub fn macos_set_theme_color(rgb: u32) {
    run_on_main_async(move || jfn_macos_apply_theme_color_on_main(rgb));
}

const K_IOPM_NULL_ASSERTION_ID: IOPMAssertionID = 0;

static G_IDLE_ASSERTION: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(K_IOPM_NULL_ASSERTION_ID);

pub fn macos_set_idle_inhibit(level: c_int) {
    let prev = G_IDLE_ASSERTION.swap(K_IOPM_NULL_ASSERTION_ID, Ordering::SeqCst);
    if prev != K_IOPM_NULL_ASSERTION_ID {
        let _ = IOPMAssertionRelease(prev);
    }

    let assertion_type = match level {
        2 => CFString::from_str("PreventUserIdleDisplaySleep"),
        1 => CFString::from_str("PreventUserIdleSystemSleep"),
        _ => return,
    };
    let name = CFString::from_str("Jellium Desktop media playback");

    let mut id: IOPMAssertionID = K_IOPM_NULL_ASSERTION_ID;
    let rc = unsafe {
        IOPMAssertionCreateWithName(
            Some(&assertion_type),
            kIOPMAssertionLevelOn,
            Some(&name),
            &mut id,
        )
    };
    if rc == kIOReturnSuccess && id != K_IOPM_NULL_ASSERTION_ID {
        G_IDLE_ASSERTION.store(id, Ordering::SeqCst);
    }
}

pub fn macos_scale() -> Scale {
    unsafe {
        let win = jfn_macos_get_window();
        if !win.is_null() {
            return crate::scale::report_backing(
                "window",
                Some(objc2::msg_send![win, backingScaleFactor]),
            );
        }
    }
    macos_display_scale()
}

pub fn macos_query_window_position(x: &mut c_int, y: &mut c_int) -> bool {
    unsafe {
        let win = jfn_macos_get_window();
        if win.is_null() {
            return false;
        }
        let screen: *mut objc2::runtime::AnyObject = objc2::msg_send![win, screen];
        if screen.is_null() {
            return false;
        }
        let frame: objc2_foundation::NSRect = objc2::msg_send![win, frame];
        let visible: objc2_foundation::NSRect = objc2::msg_send![screen, visibleFrame];
        let scale = crate::scale::report_backing(
            "window screen",
            Some(objc2::msg_send![screen, backingScaleFactor]),
        );
        let lx = frame.origin.x - visible.origin.x;
        let ly = (visible.origin.y + visible.size.height) - (frame.origin.y + frame.size.height);
        let (Some(px), Some(py)) = (
            crate::scale::to_backing(scale, lx),
            crate::scale::to_backing(scale, ly),
        ) else {
            return false;
        };
        *x = px;
        *y = py;
        true
    }
}

pub fn macos_display_scale() -> Scale {
    let mtm = unsafe { MainThreadMarker::new_unchecked() };
    crate::scale::report_backing(
        "main screen",
        NSScreen::mainScreen(mtm).map(|s| s.backingScaleFactor()),
    )
}

pub fn macos_clamp_window_geometry(w: &mut c_int, h: &mut c_int, x: &mut c_int, y: &mut c_int) {
    let mtm = unsafe { MainThreadMarker::new_unchecked() };
    let Some(screen) = NSScreen::mainScreen(mtm) else {
        return;
    };
    let visible = screen.visibleFrame();
    let scale = crate::scale::report_backing("main screen", Some(screen.backingScaleFactor()));
    let (Some(vw), Some(vh)) = (
        crate::scale::to_backing(scale, visible.size.width),
        crate::scale::to_backing(scale, visible.size.height),
    ) else {
        tracing::error!(target: "Main", "visible frame {}x{} is unrepresentable at scale {scale}", visible.size.width, visible.size.height);
        return;
    };
    let mut g = WindowGeometry::from_raw(*w, *h, *x, *y);
    clamp_to_bounds(&mut g, Bounds { w: vw, h: vh });
    *w = g.w;
    *h = g.h;
    let (nx, ny) = g.raw_position();
    *x = nx;
    *y = ny;
}

use crate::init::{macos_cleanup, macos_early_init, macos_init};

use crate::input::jfn_input_macos_set_cursor;

use jfn_mpv::api::{jfn_mpv_set_fullscreen, jfn_mpv_toggle_fullscreen};
use jfn_mpv::boot::jfn_mpv_handle_get;

pub fn macos_set_fullscreen(fullscreen: bool) {
    if jfn_mpv_handle_get().is_null() {
        return;
    }
    jfn_mpv_set_fullscreen(fullscreen);
}

pub fn macos_toggle_fullscreen() {
    if jfn_mpv_handle_get().is_null() {
        return;
    }
    jfn_mpv_toggle_fullscreen();
}

const NS_EVENT_MASK_ANY: u64 = u64::MAX;

pub fn macos_pump() {
    unsafe {
        let pool: *mut objc2::runtime::AnyObject =
            objc2::msg_send![objc2::class!(NSAutoreleasePool), new];
        let app: *mut objc2::runtime::AnyObject =
            objc2::msg_send![objc2::class!(NSApplication), sharedApplication];
        let distant_past: *mut objc2::runtime::AnyObject =
            objc2::msg_send![objc2::class!(NSDate), distantPast];
        loop {
            let event: *mut objc2::runtime::AnyObject = objc2::msg_send![
                app,
                nextEventMatchingMask: NS_EVENT_MASK_ANY,
                untilDate: distant_past,
                inMode: NSDefaultRunLoopMode,
                dequeue: true,
            ];
            if event.is_null() {
                break;
            }
            let _: () = objc2::msg_send![app, sendEvent: event];
        }
        let _ = CFRunLoop::run_in_mode(kCFRunLoopDefaultMode, 0.0, false);
        let _: () = objc2::msg_send![pool, drain];
    }
}

pub fn macos_run_main_loop() {
    unsafe {
        let app: *mut objc2::runtime::AnyObject =
            objc2::msg_send![objc2::class!(NSApplication), sharedApplication];
        let _: () = objc2::msg_send![app, run];
    }
}

pub unsafe extern "C" fn macos_mpv_wakeup_cb(_data: *mut c_void) {
    wake_main_queue();
}

pub fn macos_pump_block(seconds: f64) {
    macos_pump();
    let _ = CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, seconds, true);
}

pub(crate) fn macos_wait_for_source() {
    let _ = CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, f64::MAX, true);
}

unsafe fn stop_app_with_sentinel() {
    unsafe {
        let pool: *mut objc2::runtime::AnyObject =
            objc2::msg_send![objc2::class!(NSAutoreleasePool), new];
        let app: *mut objc2::runtime::AnyObject =
            objc2::msg_send![objc2::class!(NSApplication), sharedApplication];
        let _: () = objc2::msg_send![app, stop: std::ptr::null_mut::<objc2::runtime::AnyObject>()];
        const NS_EVENT_TYPE_APPLICATION_DEFINED: u64 = 15;
        let zero_point = objc2_foundation::NSPoint { x: 0.0, y: 0.0 };
        let sentinel: *mut objc2::runtime::AnyObject = objc2::msg_send![
            objc2::class!(NSEvent),
            otherEventWithType: NS_EVENT_TYPE_APPLICATION_DEFINED,
            location: zero_point,
            modifierFlags: 0u64,
            timestamp: 0.0f64,
            windowNumber: 0isize,
            context: std::ptr::null_mut::<objc2::runtime::AnyObject>(),
            subtype: 0i16,
            data1: 0isize,
            data2: 0isize,
        ];
        if !sentinel.is_null() {
            let _: () = objc2::msg_send![app, postEvent: sentinel, atStart: true];
        }
        let _: () = objc2::msg_send![pool, drain];
    }
}

pub fn macos_wake_main_loop() {
    post_to_main(|| unsafe { stop_app_with_sentinel() });
    if let Some(rl) = CFRunLoop::main() {
        rl.wake_up();
    }
}

pub fn macos_run_blocking(
    f: Box<dyn FnOnce() + Send>,
) -> Result<(), jfn_platform_abi::BlockingError> {
    extern "C" fn sigalrm_noop(_: std::ffi::c_int) {}
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    struct Completion(std::sync::Arc<std::sync::atomic::AtomicBool>);
    impl Drop for Completion {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
            wake_main_queue();
        }
    }
    let completion = Completion(done.clone());
    let t = jfn_platform_abi::blocking::spawn_preserving(f, |work| {
        std::thread::Builder::new()
            .name("jfn-blocking".into())
            .spawn(move || {
                let _completion = completion;
                use nix::sys::signal::{SigHandler, Signal, signal};
                let _ = unsafe { signal(Signal::SIGALRM, SigHandler::Handler(sigalrm_noop)) };
                work();
            })
    })?;
    while !done.load(Ordering::Acquire) {
        let _ = CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, f64::MAX, true);
    }
    if let Err(panic) = t.join() {
        std::panic::resume_unwind(panic);
    }
    Ok(())
}

pub fn macos_clipboard_read_text_async(on_done: OnText) {
    let pb = NSPasteboard::generalPasteboard();
    let text = pb
        .stringForType(unsafe { NSPasteboardTypeString })
        .map(|s| s.to_string());
    on_done(text.as_deref());
}

pub fn macos_clipboard_write_text(text: &str) {
    let pb = NSPasteboard::generalPasteboard();
    unsafe {
        pb.clearContents();
        let _ = pb.setString_forType(&NSString::from_str(text), NSPasteboardTypeString);
    }
}

pub fn macos_open_external_url(url: &str) {
    if url.is_empty() {
        return;
    }
    let Some(nsurl) = NSURL::URLWithString(&NSString::from_str(url)) else {
        return;
    };
    let _ = NSWorkspace::sharedWorkspace().openURL(&nsurl);
}

use crate::compositor::{
    macos_alloc_surface, macos_apply_stack, macos_free_surface, macos_set_surface_visibility,
    macos_surface_present, macos_surface_present_software, macos_surface_resize,
    macos_surface_window_target,
};

use jfn_platform_abi::{
    IdleInhibitLevel, MenuDelivery, MenuKind, OnText, Scale, SurfaceHandle, SurfaceSize,
    WindowGeometry, WindowPos,
};

struct NowPlayingSink;

impl jfn_platform_abi::MediaSink for NowPlayingSink {
    fn start(&self, _instance: &jfn_platform_abi::Instance) {
        jfn_macos_sink::jfn_macos_sink_start();
    }

    fn stop(&self) {
        jfn_macos_sink::jfn_macos_sink_stop();
    }
}

pub struct MacosPlatform;

impl Platform for MacosPlatform {
    fn display(&self) -> DisplayBackend {
        DisplayBackend::MacOS
    }

    fn default_window_decorations(&self) -> WindowDecorations {
        WindowDecorations::ServerThemed
    }

    fn early_init(&self) {
        macos_early_init();
    }

    fn init(
        &self,
        _access: &jfn_platform_abi::LifecycleAccess,
        mpv: *mut c_void,
    ) -> Result<(), jfn_platform_abi::PlatformInitError> {
        macos_init(mpv)
    }

    fn cleanup(&self, _access: &jfn_platform_abi::LifecycleAccess) {
        macos_cleanup();
    }

    fn post_window_cleanup(&self, _access: &jfn_platform_abi::LifecycleAccess) {}

    fn window_decoration_options(&self) -> jfn_platform_abi::DecorationOptions {
        jfn_platform_abi::DecorationOptions::all()
    }

    fn window_decorations_supported(&self) -> bool {
        false
    }

    fn effective_decorations(&self) -> jfn_platform_abi::EffectiveDecorations {
        jfn_platform_abi::EffectiveDecorations::ServerSide
    }

    fn shared_texture_supported(&self) -> bool {
        true
    }

    fn set_shared_texture_unsupported(&self) {}

    fn cef_init_precedes_mpv_window(&self) -> bool {
        false
    }

    fn web_paste_reads_clipboard(&self) -> bool {
        true
    }

    fn alloc_surface(&self, initial: Visibility) -> SurfaceHandle {
        SurfaceHandle::from_ptr(macos_alloc_surface(initial))
    }

    fn free_surface(&self, s: SurfaceHandle) {
        macos_free_surface(s.as_ptr());
    }

    fn surface_present<'a>(
        &self,
        s: SurfaceHandle,
        frame: PaintFrame<'a>,
    ) -> Result<Presented, PaintFrame<'a>> {
        let presented = match frame.content() {
            Content::Accelerated(tex) => macos_surface_present(s.as_ptr(), tex),
            Content::Software {
                size,
                pixels,
                dirty,
            } => macos_surface_present_software(s.as_ptr(), pixels, *size, dirty),
        };
        presented.ok_or(frame)
    }

    fn surface_resize(&self, s: SurfaceHandle, size: SurfaceSize) {
        macos_surface_resize(s.as_ptr(), size);
    }

    fn surface_window_target(&self, s: SurfaceHandle) -> Option<jfn_platform_abi::WindowTarget> {
        macos_surface_window_target(s.as_ptr())
    }

    fn set_surface_visibility(&self, s: SurfaceHandle, visibility: Visibility) -> VisibilityCommit {
        macos_set_surface_visibility(s.as_ptr(), visibility)
    }

    fn apply_stack(&self, ordered: &[SurfaceHandle]) {
        macos_apply_stack(ordered.as_ptr() as *const *mut c_void, ordered.len());
    }

    fn menu_delivery(&self, _kind: MenuKind) -> MenuDelivery<'_> {
        MenuDelivery::Host(&crate::menu::NsMenuHost)
    }

    fn mpv_host(&self) -> &dyn jfn_platform_abi::MpvHost {
        &crate::mpv_host::MacosMpvHost
    }

    fn cef_host(&self) -> Option<&dyn jfn_platform_abi::CefHost> {
        Some(&crate::cef_host::MacosCefHost)
    }

    fn media_session(&self) -> &dyn jfn_platform_abi::MediaSink {
        &NowPlayingSink
    }

    fn cef_paths(&self) -> jfn_platform_abi::CefPaths {
        let exe = std::env::current_exe()
            .and_then(std::fs::canonicalize)
            .unwrap_or_default();
        let app_contents = exe.parent().and_then(|p| p.parent()).unwrap_or(&exe);
        let framework = app_contents
            .join("Frameworks")
            .join("Chromium Embedded Framework.framework");
        jfn_platform_abi::CefPaths {
            framework_dir_path: Some(framework),
            browser_subprocess_path: Some(exe),
            ..Default::default()
        }
    }

    fn set_fullscreen(&self, v: bool) {
        macos_set_fullscreen(v);
    }

    fn toggle_fullscreen(&self) {
        macos_toggle_fullscreen();
    }

    fn resize_gate(&self) -> Option<&dyn jfn_platform_abi::ResizeGate> {
        Some(&crate::compositor::MACOS_RESIZE_GATE)
    }

    fn titlebar_controls(&self) -> Option<&dyn jfn_platform_abi::TitlebarControls> {
        None
    }

    fn scale(&self) -> Scale {
        macos_scale()
    }

    fn display_scale(&self, at: Option<WindowPos>) -> Scale {
        crate::scale::display_scale(at)
    }

    fn window_owner(&self) -> jfn_platform_abi::WindowOwner<'_> {
        jfn_platform_abi::WindowOwner::Mpv(&jfn_playback::window_source::MPV_WINDOW_SOURCE)
    }

    fn query_window_position(&self) -> Option<WindowPos> {
        let (mut x, mut y) = (0, 0);
        if macos_query_window_position(&mut x, &mut y) {
            Some(WindowPos { x, y })
        } else {
            None
        }
    }

    fn clamp_window_geometry(&self, g: WindowGeometry) -> WindowGeometry {
        let (mut w, mut h) = (g.w, g.h);
        let (mut x, mut y) = g.raw_position();
        macos_clamp_window_geometry(&mut w, &mut h, &mut x, &mut y);
        WindowGeometry::from_raw(w, h, x, y)
    }

    fn pump(&self) {
        macos_pump();
    }

    fn run_main_loop(&self) {
        macos_run_main_loop();
    }

    fn wake_main_loop(&self) {
        macos_wake_main_loop();
    }

    fn set_cursor(&self, shape: jfn_platform_abi::cursor::CursorShape) {
        jfn_input_macos_set_cursor(shape.as_raw());
    }

    fn set_idle_inhibit(&self, level: IdleInhibitLevel) {
        macos_set_idle_inhibit(level as c_int);
    }

    fn set_theme_color(&self, rgb: u32) {
        macos_set_theme_color(rgb);
    }

    fn clipboard_read_text_async(&self, on_done: OnText) {
        macos_clipboard_read_text_async(on_done);
    }

    fn clipboard_write_text(&self, text: &str) {
        macos_clipboard_write_text(text);
    }

    fn open_external_url(&self, url: &str) {
        macos_open_external_url(url);
    }

    fn open_path(&self, path: &std::path::Path) {
        let _ = std::process::Command::new("open").arg(path).spawn();
    }

    fn run_blocking(
        &self,
        f: Box<dyn FnOnce() + Send>,
    ) -> Result<(), jfn_platform_abi::BlockingError> {
        macos_run_blocking(f)
    }
}

pub fn make_macos_platform() -> Box<dyn Platform> {
    Box::new(MacosPlatform)
}
