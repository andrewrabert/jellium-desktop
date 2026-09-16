#![allow(clippy::not_unsafe_ptr_arg_deref)]

use parking_lot::Mutex;
use std::cell::Cell;
use std::ffi::{c_int, c_void};
use std::ptr;

use objc2::ffi::{class_addProtocol, imp_implementationWithBlock};
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, AnyProtocol, Bool, Sel};
use objc2::{ClassType, DefinedClass, class, define_class, extern_class, msg_send, sel};
use objc2_foundation::{NSObject, NSObjectProtocol, NSRect};

use crate::input::jfn_input_macos_create_view;

use jfn_playback::shutdown::{jfn_shutdown_initiate, jfn_shutting_down};

use jfn_mpv::api::jfn_mpv_set_force_window_position;

const LOG_TARGET: &str = "Platform";

struct InitState {
    window: *mut AnyObject,
    input_view: *mut AnyObject,
    display_link_target: *mut AnyObject,
    display_link: *mut AnyObject,
    app_menu_target: *mut AnyObject,
    wake_target: *mut AnyObject,
    lifecycle_observer: *mut AnyObject,
    frame_driver: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
}

unsafe impl Send for InitState {}

static INIT_STATE: Mutex<InitState> = Mutex::new(InitState {
    window: ptr::null_mut(),
    input_view: ptr::null_mut(),
    display_link_target: ptr::null_mut(),
    display_link: ptr::null_mut(),
    app_menu_target: ptr::null_mut(),
    wake_target: ptr::null_mut(),
    lifecycle_observer: ptr::null_mut(),
    frame_driver: None,
});

pub fn jfn_macos_get_window() -> *mut AnyObject {
    INIT_STATE.lock().window
}

pub fn jfn_macos_get_input_view() -> *mut AnyObject {
    INIT_STATE.lock().input_view
}

pub fn jfn_macos_query_logical_content_size(w: *mut c_int, h: *mut c_int) -> bool {
    unsafe {
        let win = INIT_STATE.lock().window;
        if win.is_null() {
            return false;
        }
        let content_view: *mut AnyObject = msg_send![win, contentView];
        if content_view.is_null() {
            return false;
        }
        let bounds: NSRect = msg_send![content_view, bounds];
        *w = bounds.size.width as c_int;
        *h = bounds.size.height as c_int;
        *w > 0 && *h > 0
    }
}

pub fn jfn_macos_apply_theme_color_on_main(rgb: u32) {
    let win = INIT_STATE.lock().window;
    unsafe { apply_theme_color_to_window(win, rgb) };
}

unsafe fn apply_theme_color_to_window(win: *mut AnyObject, rgb: u32) {
    if win.is_null() {
        return;
    }
    unsafe {
        let r = ((rgb >> 16) & 0xff) as f64 / 255.0;
        let g = ((rgb >> 8) & 0xff) as f64 / 255.0;
        let b = (rgb & 0xff) as f64 / 255.0;
        let nscolor_cls = class!(NSColor);
        let ns: *mut AnyObject = msg_send![
            nscolor_cls,
            colorWithSRGBRed: r,
            green: g,
            blue: b,
            alpha: 1.0f64
        ];
        let _: () = msg_send![win, setBackgroundColor: ns];
        let cv: *mut AnyObject = msg_send![win, contentView];
        if !cv.is_null() {
            let layer: *mut AnyObject = msg_send![cv, layer];
            if !layer.is_null() {
                let cg: *mut c_void = msg_send![ns, CGColor];
                let _: () = msg_send![layer, setBackgroundColor: cg];
            }
        }
    }
}

#[derive(Default)]
struct JellyfinAppIvars {
    handling_send_event: Cell<bool>,
}

unsafe impl Send for JellyfinAppIvars {}
unsafe impl Sync for JellyfinAppIvars {}

extern_class!(
    #[unsafe(super(NSObject))]
    #[name = "NSApplication"]
    pub struct NSApplication;
);

define_class!(
    #[unsafe(super(NSApplication))]
    #[name = "JellyfinApplication"]
    #[ivars = JellyfinAppIvars]
    struct JellyfinApplication;

    impl JellyfinApplication {
        #[unsafe(method(isHandlingSendEvent))]
        fn is_handling_send_event(&self) -> bool {
            self.ivars().handling_send_event.get()
        }

        #[unsafe(method(setHandlingSendEvent:))]
        fn set_handling_send_event(&self, v: bool) {
            self.ivars().handling_send_event.set(v);
        }

        #[unsafe(method(sendEvent:))]
        unsafe fn send_event(&self, event: *mut AnyObject) {
            self.ivars().handling_send_event.set(true);
            unsafe {
                let _: () = msg_send![super(self), sendEvent: event];
            }
            self.ivars().handling_send_event.set(false);
        }

        #[unsafe(method(terminate:))]
        unsafe fn terminate(&self, _sender: *mut AnyObject) {
            jfn_shutdown_initiate();
        }

        #[unsafe(method(handleReopenEvent:withReplyEvent:))]
        unsafe fn handle_reopen(&self, _event: *mut AnyObject, _reply: *mut AnyObject) {
            unsafe {
                let ns_app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
                let windows: *mut AnyObject = msg_send![ns_app, windows];
                if windows.is_null() {
                    return;
                }
                let count: usize = msg_send![windows, count];
                for i in 0..count {
                    let w: *mut AnyObject = msg_send![windows, objectAtIndex: i];
                    if w.is_null() {
                        continue;
                    }
                    let mini: bool = msg_send![w, isMiniaturized];
                    if mini {
                        let _: () = msg_send![w, deminiaturize: ptr::null_mut::<AnyObject>()];
                        break;
                    }
                }
            }
        }
    }
);

fn attach_cef_app_protocol(cls: &AnyClass) {
    let Some(proto) = AnyProtocol::get(c"CefAppProtocol") else {
        return;
    };
    let _ = unsafe { class_addProtocol(std::ptr::from_ref(cls).cast_mut(), proto) };
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "JellyfinAppMenuTarget"]
    pub struct JellyfinAppMenuTarget;

    impl JellyfinAppMenuTarget {
        #[unsafe(method(showAbout:))]
        unsafe fn show_about(&self, _sender: *mut AnyObject) {
            jfn_platform_abi::request_about();
        }
    }

    unsafe impl NSObjectProtocol for JellyfinAppMenuTarget {}
);

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "JellyfinLifecycleObserver"]
    pub struct JellyfinLifecycleObserver;

    impl JellyfinLifecycleObserver {
        #[unsafe(method(appDidHide:))]
        unsafe fn app_did_hide(&self, _n: *mut AnyObject) {
            jfn_playback::lifecycle::jfn_lifecycle_set_visible(false);
        }
        #[unsafe(method(appDidUnhide:))]
        unsafe fn app_did_unhide(&self, _n: *mut AnyObject) {
            jfn_playback::lifecycle::jfn_lifecycle_set_visible(true);
        }
        #[unsafe(method(workspaceWillSleep:))]
        unsafe fn workspace_will_sleep(&self, _n: *mut AnyObject) {
            jfn_playback::lifecycle::jfn_lifecycle_suspend();
        }
        #[unsafe(method(workspaceDidWake:))]
        unsafe fn workspace_did_wake(&self, _n: *mut AnyObject) {
            jfn_playback::lifecycle::jfn_lifecycle_resume();
        }
        #[unsafe(method(windowDidBecomeKey:))]
        unsafe fn window_did_become_key(&self, _n: *mut AnyObject) {
            jfn_input::jfn_input_dispatch_keyboard_focus(1);
        }
        #[unsafe(method(windowDidResignKey:))]
        unsafe fn window_did_resign_key(&self, _n: *mut AnyObject) {
            jfn_input::jfn_input_dispatch_keyboard_focus(0);
        }
                                #[unsafe(method(windowDidChangeBackingProperties:))]
        unsafe fn window_did_change_backing_properties(&self, _n: *mut AnyObject) {
            jfn_playback::ingest_driver::jfn_playback_rescale_window_extent();
        }
    }

    unsafe impl NSObjectProtocol for JellyfinLifecycleObserver {}
);

unsafe fn install_lifecycle_observer() {
    unsafe {
        let observer: Retained<JellyfinLifecycleObserver> =
            msg_send![JellyfinLifecycleObserver::class(), new];
        let observer_obj: *mut AnyObject = Retained::into_raw(observer) as *mut AnyObject;

        let nc: *mut AnyObject = msg_send![class!(NSNotificationCenter), defaultCenter];
        for (sel, name) in [
            (sel!(appDidHide:), c"NSApplicationDidHideNotification"),
            (sel!(appDidUnhide:), c"NSApplicationDidUnhideNotification"),
            (
                sel!(windowDidBecomeKey:),
                c"NSWindowDidBecomeKeyNotification",
            ),
            (
                sel!(windowDidResignKey:),
                c"NSWindowDidResignKeyNotification",
            ),
            (
                sel!(windowDidChangeBackingProperties:),
                c"NSWindowDidChangeBackingPropertiesNotification",
            ),
        ] {
            let name_ns: *mut AnyObject =
                msg_send![class!(NSString), stringWithUTF8String: name.as_ptr()];
            let _: () = msg_send![
                nc,
                addObserver: observer_obj,
                selector: sel,
                name: name_ns,
                object: std::ptr::null_mut::<AnyObject>(),
            ];
        }

        let ws: *mut AnyObject = msg_send![class!(NSWorkspace), sharedWorkspace];
        let ws_nc: *mut AnyObject = msg_send![ws, notificationCenter];
        for (sel, name) in [
            (
                sel!(workspaceWillSleep:),
                c"NSWorkspaceWillSleepNotification",
            ),
            (sel!(workspaceDidWake:), c"NSWorkspaceDidWakeNotification"),
        ] {
            let name_ns: *mut AnyObject =
                msg_send![class!(NSString), stringWithUTF8String: name.as_ptr()];
            let _: () = msg_send![
                ws_nc,
                addObserver: observer_obj,
                selector: sel,
                name: name_ns,
                object: std::ptr::null_mut::<AnyObject>(),
            ];
        }

        INIT_STATE.lock().lifecycle_observer = observer_obj;
    }
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "JellyfinDisplayLinkTarget"]
    pub struct JellyfinDisplayLinkTarget;

    impl JellyfinDisplayLinkTarget {
        #[unsafe(method(tick:))]
        unsafe fn tick(&self, _link: *mut AnyObject) {
            if jfn_shutting_down() {
                return;
            }
            let driver = INIT_STATE.lock().frame_driver.clone();
            if let Some(driver) = driver {
                driver();
            }
        }
    }

    unsafe impl NSObjectProtocol for JellyfinDisplayLinkTarget {}
);

unsafe fn build_display_link(window: *mut AnyObject) -> Option<(*mut AnyObject, *mut AnyObject)> {
    unsafe {
        let target: Retained<JellyfinDisplayLinkTarget> =
            msg_send![JellyfinDisplayLinkTarget::class(), new];
        let target_obj: *mut AnyObject = Retained::into_raw(target) as *mut AnyObject;

        let screen: *mut AnyObject = msg_send![window, screen];
        if screen.is_null() {
            tracing::error!(target: LOG_TARGET, "[CVDL] window has no screen");
            let _: () = msg_send![target_obj, release];
            return None;
        }
        let sel_tick = sel!(tick:);
        let link: *mut AnyObject = msg_send![
            screen,
            displayLinkWithTarget: target_obj,
            selector: sel_tick,
        ];
        if link.is_null() {
            tracing::error!(target: LOG_TARGET, "[CVDL] displayLinkWithTarget failed");
            let _: () = msg_send![target_obj, release];
            return None;
        }
        let _: () = msg_send![link, retain];

        let _: () = msg_send![link, setPaused: false];

        let main_runloop: *mut AnyObject = msg_send![class!(NSRunLoop), mainRunLoop];
        let common_modes_name = c"kCFRunLoopCommonModes";
        let common_ns: *mut AnyObject = msg_send![
            class!(NSString),
            stringWithUTF8String: common_modes_name.as_ptr()
        ];
        let _: () = msg_send![link, addToRunLoop: main_runloop, forMode: common_ns];
        let default_modes_name = c"kCFRunLoopDefaultMode";
        let default_ns: *mut AnyObject = msg_send![
            class!(NSString),
            stringWithUTF8String: default_modes_name.as_ptr()
        ];
        let _: () = msg_send![link, addToRunLoop: main_runloop, forMode: default_ns];

        Some((target_obj, link))
    }
}

unsafe fn start_display_link(state: &mut InitState) -> bool {
    unsafe {
        match build_display_link(state.window) {
            Some((target_obj, link)) => {
                state.display_link_target = target_obj;
                state.display_link = link;
                tracing::info!(target: LOG_TARGET, "[CVDL] started");
                true
            }
            None => false,
        }
    }
}

unsafe fn restart_display_link(state: &mut InitState) {
    unsafe {
        let Some((target_obj, link)) = build_display_link(state.window) else {
            tracing::error!(
                target: LOG_TARGET,
                "[CVDL] restart failed to build link; keeping existing link"
            );
            return;
        };
        let old_target = state.display_link_target;
        let old_link = state.display_link;
        state.display_link_target = target_obj;
        state.display_link = link;
        if !old_link.is_null() {
            let _: () = msg_send![old_link, invalidate];
            let _: () = msg_send![old_link, release];
        }
        if !old_target.is_null() {
            let _: () = msg_send![old_target, release];
        }
        tracing::info!(target: LOG_TARGET, "[CVDL] restarted");
    }
}

unsafe fn stop_display_link(state: &mut InitState) {
    unsafe {
        if state.display_link.is_null() {
            return;
        }
        let _: () = msg_send![state.display_link, invalidate];
        let _: () = msg_send![state.display_link, release];
        state.display_link = ptr::null_mut();
        if !state.display_link_target.is_null() {
            let _: () = msg_send![state.display_link_target, release];
            state.display_link_target = ptr::null_mut();
        }
        tracing::info!(target: LOG_TARGET, "[CVDL] stopped");
    }
}

pub fn stop_frame_driver() {
    let mut state = INIT_STATE.lock();
    unsafe {
        stop_display_link(&mut state);
    }
    state.frame_driver.take();
}

pub fn start_frame_driver(driver: std::sync::Arc<dyn Fn() + Send + Sync>) -> bool {
    let mut state = INIT_STATE.lock();
    state.frame_driver = Some(driver);
    if !state.display_link.is_null() {
        return true;
    }
    unsafe { start_display_link(&mut state) }
}

fn restart_display_link_locked() {
    if jfn_shutting_down() {
        return;
    }
    let mut state = INIT_STATE.lock();
    if state.window.is_null() {
        return;
    }
    unsafe { restart_display_link(&mut state) };
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "JellyfinWakeTarget"]
    pub struct JellyfinWakeTarget;

    impl JellyfinWakeTarget {
        #[unsafe(method(displayLinkNeedsRestart:))]
        unsafe fn display_link_needs_restart(&self, _note: *mut AnyObject) {
            tracing::info!(
                target: LOG_TARGET,
                "[WAKE] wake/screen-change; restarting display link"
            );
            restart_display_link_locked();
        }
    }

    unsafe impl NSObjectProtocol for JellyfinWakeTarget {}
);

unsafe fn start_wake_observer(state: &mut InitState) {
    unsafe {
        let target: Retained<JellyfinWakeTarget> = msg_send![JellyfinWakeTarget::class(), new];
        let target_obj: *mut AnyObject = Retained::into_raw(target) as *mut AnyObject;
        state.wake_target = target_obj;

        let sel_restart = sel!(displayLinkNeedsRestart:);

        let ws: *mut AnyObject = msg_send![class!(NSWorkspace), sharedWorkspace];
        let ws_nc: *mut AnyObject = msg_send![ws, notificationCenter];
        for name in [
            c"NSWorkspaceDidWakeNotification",
            c"NSWorkspaceScreensDidWakeNotification",
        ] {
            let name_ns: *mut AnyObject =
                msg_send![class!(NSString), stringWithUTF8String: name.as_ptr()];
            let _: () = msg_send![
                ws_nc,
                addObserver: target_obj,
                selector: sel_restart,
                name: name_ns,
                object: ptr::null_mut::<AnyObject>(),
            ];
        }

        let nc: *mut AnyObject = msg_send![class!(NSNotificationCenter), defaultCenter];
        let screen_name: *mut AnyObject = msg_send![
            class!(NSString),
            stringWithUTF8String: c"NSApplicationDidChangeScreenParametersNotification".as_ptr()
        ];
        let _: () = msg_send![
            nc,
            addObserver: target_obj,
            selector: sel_restart,
            name: screen_name,
            object: ptr::null_mut::<AnyObject>(),
        ];

        tracing::info!(
            target: LOG_TARGET,
            "[WAKE] subscribed to wake + screen-change notifications"
        );
    }
}

use crate::macos_pump_block;
use std::time::Instant;

pub fn macos_init(_mpv: *mut c_void) -> Result<(), jfn_platform_abi::PlatformInitError> {
    tracing::info!(target: LOG_TARGET, "[INIT] macos_init: waiting for mpv window");

    let mut state = INIT_STATE.lock();
    unsafe {
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let ns_app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
            let windows: *mut AnyObject = msg_send![ns_app, windows];
            if !windows.is_null() {
                let count: usize = msg_send![windows, count];
                for i in 0..count {
                    let w: *mut AnyObject = msg_send![windows, objectAtIndex: i];
                    if w.is_null() {
                        continue;
                    }
                    let visible: bool = msg_send![w, isVisible];
                    if visible {
                        let _: () = msg_send![w, retain];
                        state.window = w;
                        break;
                    }
                }
            }
            if !state.window.is_null() {
                break;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            macos_pump_block(remaining.as_secs_f64());
        }
        if state.window.is_null() {
            return Err(jfn_platform_abi::PlatformInitError::backend(
                "macOS window acquisition",
                "mpv did not create a visible window before deadline",
            ));
        }
        tracing::info!(target: LOG_TARGET, "[INIT] macos_init: got window={:?}", state.window);

        let cls: *const AnyClass = msg_send![state.window, class];
        if let Some(method) = cls
            .as_ref()
            .and_then(|c| c.instance_method(sel!(windowShouldClose:)))
        {
            unsafe extern "C" fn close_block_invoke(
                _block: *mut c_void,
                _self_: *mut AnyObject,
                _win: *mut AnyObject,
            ) -> Bool {
                jfn_shutdown_initiate();
                Bool::NO
            }
            #[repr(C)]
            struct GlobalBlock {
                isa: *const c_void,
                flags: i32,
                reserved: i32,
                invoke: unsafe extern "C" fn(*mut c_void, *mut AnyObject, *mut AnyObject) -> Bool,
                descriptor: *const BlockDescriptor,
            }
            #[repr(C)]
            struct BlockDescriptor {
                reserved: usize,
                size: usize,
            }
            unsafe impl Sync for GlobalBlock {}
            unsafe extern "C" {
                static _NSConcreteGlobalBlock: c_void;
            }
            static BLOCK_DESC: BlockDescriptor = BlockDescriptor {
                reserved: 0,
                size: std::mem::size_of::<GlobalBlock>(),
            };
            static BLOCK: GlobalBlock = GlobalBlock {
                isa: unsafe { &_NSConcreteGlobalBlock as *const c_void },
                flags: 1 << 28,
                reserved: 0,
                invoke: close_block_invoke,
                descriptor: &BLOCK_DESC,
            };
            let imp = imp_implementationWithBlock(
                std::ptr::from_ref(&BLOCK).cast_mut().cast::<AnyObject>(),
            );
            let _ = method.set_implementation(imp);
        }

        jfn_mpv_set_force_window_position(false);

        let bundle: *mut AnyObject = msg_send![class!(NSBundle), mainBundle];
        if !bundle.is_null() {
            let res_path: *mut AnyObject = msg_send![bundle, resourcePath];
            if !res_path.is_null() {
                let icon_name = c"AppIcon.icns";
                let icon_ns: *mut AnyObject = msg_send![
                    class!(NSString),
                    stringWithUTF8String: icon_name.as_ptr()
                ];
                let icon_path: *mut AnyObject = msg_send![
                    res_path,
                    stringByAppendingPathComponent: icon_ns
                ];
                if !icon_path.is_null() {
                    let icon: *mut AnyObject = msg_send![class!(NSImage), alloc];
                    let icon: *mut AnyObject = msg_send![
                        icon,
                        initWithContentsOfFile: icon_path
                    ];
                    if !icon.is_null() {
                        let ns_app: *mut AnyObject =
                            msg_send![class!(NSApplication), sharedApplication];
                        let _: () = msg_send![ns_app, setApplicationIconImage: icon];
                        let _: () = msg_send![icon, release];
                    }
                }
            }
        }

        let _: () = msg_send![state.window, setTitlebarAppearsTransparent: true];
        let _: () = msg_send![state.window, setTitleVisibility: 1isize];
        let mask: u64 = msg_send![state.window, styleMask];
        let _: () = msg_send![state.window, setStyleMask: (mask | (1u64 << 15))];

        let content_view: *mut AnyObject = msg_send![state.window, contentView];
        if !content_view.is_null() {
            let layer: *mut AnyObject = msg_send![content_view, layer];
            if layer.is_null() {
                let _: () = msg_send![content_view, setWantsLayer: true];
            }
        }

        const K_BG_COLOR: u32 = 0x101010;
        apply_theme_color_to_window(state.window, K_BG_COLOR);

        state.input_view = jfn_input_macos_create_view() as *mut AnyObject;
        if !state.input_view.is_null() && !content_view.is_null() {
            let bounds: NSRect = msg_send![content_view, bounds];
            let _: () = msg_send![state.input_view, setFrame: bounds];
            let mask: u64 = (1u64 << 1) | (1u64 << 4);
            let _: () = msg_send![state.input_view, setAutoresizingMask: mask];
            let _: () = msg_send![content_view, addSubview: state.input_view];
        }

        let _: () = msg_send![state.window, setAcceptsMouseMovedEvents: true];
        let _: () = msg_send![state.window, makeFirstResponder: state.input_view];

        start_wake_observer(&mut state);

        tracing::info!(
            target: LOG_TARGET,
            "[INIT] Metal compositor initialized input_view={:?}",
            state.input_view
        );
        Ok(())
    }
}

use crate::compositor::jfn_macos_compositor_cleanup;

pub fn macos_cleanup() {
    let mut state = INIT_STATE.lock();
    unsafe {
        stop_display_link(&mut state);

        if !state.input_view.is_null() {
            let _: () = msg_send![state.input_view, removeFromSuperview];
            let _: () = msg_send![state.input_view, release];
            state.input_view = ptr::null_mut();
        }

        jfn_macos_compositor_cleanup();

        if !state.window.is_null() {
            let _: () = msg_send![state.window, release];
            state.window = ptr::null_mut();
        }
    }
}

pub fn macos_early_init() {
    unsafe {
        attach_cef_app_protocol(JellyfinApplication::class());

        let app_obj: *mut AnyObject = msg_send![JellyfinApplication::class(), sharedApplication];

        let ae_mgr: *mut AnyObject =
            msg_send![class!(NSAppleEventManager), sharedAppleEventManager];
        const K_CORE_EVENT_CLASS: u32 = 0x61_65_76_74;
        const K_AE_REOPEN_APPLICATION: u32 = 0x72_61_70_70;
        let sel = sel!(handleReopenEvent:withReplyEvent:);
        let _: () = msg_send![
            ae_mgr,
            setEventHandler: app_obj,
            andSelector: sel,
            forEventClass: K_CORE_EVENT_CLASS,
            andEventID: K_AE_REOPEN_APPLICATION,
        ];

        let subproc = std::env::var_os("JELLYFIN_CEF_SUBPROCESS").is_some();
        if subproc {
            let _: () = msg_send![app_obj, setActivationPolicy: 2isize];
            return;
        }

        let _: () = msg_send![app_obj, setActivationPolicy: 0isize];

        let defaults: *mut AnyObject = msg_send![class!(NSUserDefaults), standardUserDefaults];
        for k in [
            c"NSDisabledDictationMenuItem",
            c"NSDisabledCharacterPaletteMenuItem",
        ] {
            let ns: *mut AnyObject = msg_send![class!(NSString), stringWithUTF8String: k.as_ptr()];
            let _: () = msg_send![defaults, setBool: true, forKey: ns];
        }

        let mt: Retained<JellyfinAppMenuTarget> = msg_send![JellyfinAppMenuTarget::class(), new];
        let mt_obj: *mut AnyObject = Retained::into_raw(mt) as *mut AnyObject;
        INIT_STATE.lock().app_menu_target = mt_obj;

        install_lifecycle_observer();

        let menubar: *mut AnyObject = msg_send![class!(NSMenu), alloc];
        let menubar: *mut AnyObject = msg_send![menubar, init];

        let app_item: *mut AnyObject = msg_send![class!(NSMenuItem), alloc];
        let app_item: *mut AnyObject = msg_send![app_item, init];
        let _: () = msg_send![menubar, addItem: app_item];

        let app_menu: *mut AnyObject = msg_send![class!(NSMenu), alloc];
        let app_menu: *mut AnyObject = msg_send![app_menu, init];

        add_menu_item(
            app_menu,
            "About Jellium Desktop",
            sel!(showAbout:),
            "",
            Some(mt_obj),
            0,
        );
        add_separator(app_menu);
        add_menu_item(app_menu, "Hide Jellium Desktop", sel!(hide:), "h", None, 0);
        let opt_cmd_mask: u64 = (1u64 << 19) | (1u64 << 20);
        add_menu_item(
            app_menu,
            "Hide Others",
            sel!(hideOtherApplications:),
            "h",
            None,
            opt_cmd_mask,
        );
        add_menu_item(
            app_menu,
            "Show All",
            sel!(unhideAllApplications:),
            "",
            None,
            0,
        );
        add_separator(app_menu);
        add_menu_item(app_menu, "Quit", sel!(terminate:), "q", None, 0);

        let _: () = msg_send![app_item, setSubmenu: app_menu];
        let _: () = msg_send![app_menu, release];
        let _: () = msg_send![app_item, release];

        let edit_item: *mut AnyObject = msg_send![class!(NSMenuItem), alloc];
        let edit_item: *mut AnyObject = msg_send![edit_item, init];
        let _: () = msg_send![menubar, addItem: edit_item];

        let edit_menu: *mut AnyObject = msg_send![class!(NSMenu), alloc];
        let title_ns: *mut AnyObject =
            msg_send![class!(NSString), stringWithUTF8String: c"Edit".as_ptr()];
        let edit_menu: *mut AnyObject = msg_send![edit_menu, initWithTitle: title_ns];

        add_menu_item(edit_menu, "Undo", sel!(undo:), "z", None, 0);
        add_menu_item(edit_menu, "Redo", sel!(redo:), "Z", None, 0);
        add_separator(edit_menu);
        add_menu_item(edit_menu, "Cut", sel!(cut:), "x", None, 0);
        add_menu_item(edit_menu, "Copy", sel!(copy:), "c", None, 0);
        add_menu_item(edit_menu, "Paste", sel!(paste:), "v", None, 0);
        add_separator(edit_menu);
        add_menu_item(edit_menu, "Select All", sel!(selectAll:), "a", None, 0);

        let _: () = msg_send![edit_item, setSubmenu: edit_menu];
        let _: () = msg_send![edit_menu, release];
        let _: () = msg_send![edit_item, release];

        let _: () = msg_send![app_obj, setMainMenu: menubar];
        let _: () = msg_send![menubar, release];

        let _: () = msg_send![app_obj, activateIgnoringOtherApps: true];
    }
}

unsafe fn add_separator(menu: *mut AnyObject) {
    unsafe {
        let sep: *mut AnyObject = msg_send![class!(NSMenuItem), separatorItem];
        let _: () = msg_send![menu, addItem: sep];
    }
}

unsafe fn add_menu_item(
    menu: *mut AnyObject,
    title: &str,
    action: Sel,
    key_equiv: &str,
    target: Option<*mut AnyObject>,
    modifier_mask: u64,
) {
    unsafe {
        let title_c = std::ffi::CString::new(title).unwrap_or_default();
        let title_ns: *mut AnyObject = msg_send![
            class!(NSString),
            stringWithUTF8String: title_c.as_ptr()
        ];
        let ke_c = std::ffi::CString::new(key_equiv).unwrap_or_default();
        let ke_ns: *mut AnyObject = msg_send![
            class!(NSString),
            stringWithUTF8String: ke_c.as_ptr()
        ];
        let item: *mut AnyObject = msg_send![class!(NSMenuItem), alloc];
        let item: *mut AnyObject = msg_send![
            item,
            initWithTitle: title_ns,
            action: action,
            keyEquivalent: ke_ns,
        ];
        if let Some(t) = target {
            let _: () = msg_send![item, setTarget: t];
        }
        if modifier_mask != 0 {
            let _: () = msg_send![item, setKeyEquivalentModifierMask: modifier_mask];
        }
        let _: () = msg_send![menu, addItem: item];
        let _: () = msg_send![item, release];
    }
}
