//! Fault injection at the lifecycle boundary, independent of a window server.
#![allow(unused_variables, clippy::panic, clippy::unreachable)]
use super::*;

#[derive(Clone, Copy)]
enum Outcome {
    Success,
    Failure,
    Panic,
}

struct Backend {
    outcome: Outcome,
    events: Mutex<Vec<&'static str>>,
}

impl Platform for Backend {
    fn init(&self, _access: &LifecycleAccess, _mpv: *mut c_void) -> Result<(), PlatformInitError> {
        self.events.lock().push("acquired host");
        match self.outcome {
            Outcome::Success => Ok(()),
            Outcome::Failure => Err(PlatformInitError::backend(
                "after host acquisition",
                std::io::Error::other("injected"),
            )),
            Outcome::Panic => panic!("injected panic after host acquisition"),
        }
    }
    fn cleanup(&self, _access: &LifecycleAccess) {
        self.events.lock().push("detach backend");
    }
    fn post_window_cleanup(&self, _access: &LifecycleAccess) {
        self.events.lock().push("release host");
    }
    fn display(&self) -> DisplayBackend {
        unreachable!("lifecycle test never calls this platform operation")
    }
    fn default_window_decorations(&self) -> WindowDecorations {
        unreachable!("lifecycle test never calls this platform operation")
    }
    fn window_decoration_options(&self) -> DecorationOptions {
        unreachable!("lifecycle test never calls this platform operation")
    }
    fn early_init(&self) {
        unreachable!("lifecycle test never calls this platform operation")
    }
    fn alloc_surface(&self, initial: Visibility) -> SurfaceHandle {
        unreachable!("lifecycle test never calls this platform operation")
    }
    fn free_surface(&self, s: SurfaceHandle) {
        unreachable!("lifecycle test never calls this platform operation")
    }
    fn surface_present<'a>(
        &self,
        s: SurfaceHandle,
        frame: PaintFrame<'a>,
    ) -> Result<Presented, PaintFrame<'a>> {
        unreachable!("lifecycle test never calls this platform operation")
    }
    fn surface_resize(&self, s: SurfaceHandle, size: SurfaceSize) {
        unreachable!("lifecycle test never calls this platform operation")
    }
    fn surface_window_target(&self, s: SurfaceHandle) -> Option<WindowTarget> {
        unreachable!("lifecycle test never calls this platform operation")
    }
    fn set_surface_visibility(&self, s: SurfaceHandle, visibility: Visibility) -> VisibilityCommit {
        unreachable!("lifecycle test never calls this platform operation")
    }
    fn apply_stack(&self, ordered: &[SurfaceHandle]) {
        unreachable!("lifecycle test never calls this platform operation")
    }
    fn menu_delivery(&self, kind: MenuKind) -> MenuDelivery<'_> {
        unreachable!("lifecycle test never calls this platform operation")
    }
    fn mpv_host(&self) -> &dyn MpvHost {
        unreachable!("lifecycle test never calls this platform operation")
    }
    fn media_session(&self) -> &dyn MediaSink {
        unreachable!("lifecycle test never calls this platform operation")
    }
    fn cef_paths(&self) -> CefPaths {
        unreachable!("lifecycle test never calls this platform operation")
    }
    fn set_fullscreen(&self, v: bool) {
        unreachable!("lifecycle test never calls this platform operation")
    }
    fn toggle_fullscreen(&self) {
        unreachable!("lifecycle test never calls this platform operation")
    }
    fn titlebar_controls(&self) -> Option<&dyn TitlebarControls> {
        unreachable!("lifecycle test never calls this platform operation")
    }
    fn resize_gate(&self) -> Option<&dyn ResizeGate> {
        unreachable!("lifecycle test never calls this platform operation")
    }
    fn scale(&self) -> Scale {
        unreachable!("lifecycle test never calls this platform operation")
    }
    fn display_scale(&self, at: Option<WindowPos>) -> Scale {
        unreachable!("lifecycle test never calls this platform operation")
    }
    fn query_window_position(&self) -> Option<WindowPos> {
        unreachable!("lifecycle test never calls this platform operation")
    }
    fn window_owner(&self) -> WindowOwner<'_> {
        unreachable!("lifecycle test never calls this platform operation")
    }
    fn clamp_window_geometry(&self, g: WindowGeometry) -> WindowGeometry {
        unreachable!("lifecycle test never calls this platform operation")
    }
    fn pump(&self) {
        unreachable!("lifecycle test never calls this platform operation")
    }
    fn set_cursor(&self, shape: cursor::CursorShape) {
        unreachable!("lifecycle test never calls this platform operation")
    }
    fn set_idle_inhibit(&self, level: IdleInhibitLevel) {
        unreachable!("lifecycle test never calls this platform operation")
    }
    fn set_theme_color(&self, rgb: u32) {
        unreachable!("lifecycle test never calls this platform operation")
    }
    fn window_decorations_supported(&self) -> bool {
        unreachable!("lifecycle test never calls this platform operation")
    }
    fn effective_decorations(&self) -> EffectiveDecorations {
        unreachable!("lifecycle test never calls this platform operation")
    }
    fn shared_texture_supported(&self) -> bool {
        unreachable!("lifecycle test never calls this platform operation")
    }
    fn cef_init_precedes_mpv_window(&self) -> bool {
        unreachable!("lifecycle test never calls this platform operation")
    }
    fn set_shared_texture_unsupported(&self) {
        unreachable!("lifecycle test never calls this platform operation")
    }
    fn clipboard_read_text_async(&self, on_done: OnText) {
        unreachable!("lifecycle test never calls this platform operation")
    }
    fn clipboard_write_text(&self, text: &str) {
        unreachable!("lifecycle test never calls this platform operation")
    }
    fn web_paste_reads_clipboard(&self) -> bool {
        unreachable!("lifecycle test never calls this platform operation")
    }
    fn open_external_url(&self, url: &str) {
        unreachable!("lifecycle test never calls this platform operation")
    }
    fn open_path(&self, path: &Path) {
        unreachable!("lifecycle test never calls this platform operation")
    }
}

fn prepared(outcome: Outcome) -> (&'static Backend, PreparedPlatform) {
    let backend = Box::leak(Box::new(Backend {
        outcome,
        events: Mutex::new(Vec::new()),
    }));
    (
        backend,
        PreparedPlatform {
            platform: backend,
            _thread: std::marker::PhantomData,
        },
    )
}

#[test]
fn partial_acquisition_failure_and_panic_preserve_ordered_rollback() {
    let _serial = TEST_PLATFORM.lock();
    for outcome in [Outcome::Failure, Outcome::Panic] {
        let (backend, prepared) = prepared(outcome);
        let (error, prepared) = match prepared.initialize(std::ptr::null_mut()) {
            Ok(_) => panic!("fault injection unexpectedly succeeded"),
            Err(failure) => failure,
        };
        assert!(matches!(
            (outcome, error),
            (Outcome::Failure, PlatformInitError::Backend { .. })
                | (Outcome::Panic, PlatformInitError::Panicked)
        ));
        assert!(
            prepared
                .cleanup(|| {
                    backend.events.lock().push("terminate mpv");
                    Ok::<(), ()>(())
                })
                .is_ok()
        );
        assert_eq!(
            *backend.events.lock(),
            [
                "acquired host",
                "detach backend",
                "terminate mpv",
                "release host"
            ]
        );
    }
}

#[test]
fn busy_cleanup_returns_backend_and_window_owner_for_retry() {
    let _serial = TEST_PLATFORM.lock();
    let (backend, prepared) = prepared(Outcome::Success);
    let runtime = match prepared.initialize(std::ptr::null_mut()) {
        Ok(runtime) => runtime,
        Err(_) => panic!("successful backend initialization failed"),
    };
    let dependent = match try_lease() {
        Some(lease) => lease,
        None => panic!("initialized backend was not published"),
    };
    struct WindowOwner(&'static Backend);
    impl Drop for WindowOwner {
        fn drop(&mut self) {
            self.0.events.lock().push("terminate mpv");
        }
    }
    let window = WindowOwner(backend);
    let (runtime, terminate) = match runtime.cleanup_with_timeout(
        move || {
            drop(window);
            Ok::<(), ()>(())
        },
        std::time::Duration::ZERO,
    ) {
        Ok(()) => panic!("cleanup ran with a dependent alive"),
        Err(PlatformCleanupError::Busy { runtime, terminate }) => (runtime, terminate),
        Err(PlatformCleanupError::Termination { .. }) => panic!("termination attempted while busy"),
    };
    assert_eq!(*backend.events.lock(), ["acquired host"]);
    assert!(
        try_lease().is_none(),
        "busy cleanup must leave admission retired"
    );
    drop(dependent);
    assert!(
        runtime
            .cleanup(|| {
                assert!(
                    try_lease().is_none(),
                    "cleanup must revoke new native work before window termination"
                );
                terminate()
            })
            .is_ok()
    );
    assert_eq!(
        *backend.events.lock(),
        [
            "acquired host",
            "detach backend",
            "terminate mpv",
            "release host"
        ]
    );
}

#[test]
fn cleanup_retires_admission_and_waits_for_an_inflight_callback() {
    let _serial = TEST_PLATFORM.lock();
    let (backend, prepared) = prepared(Outcome::Success);
    let runtime = match prepared.initialize(std::ptr::null_mut()) {
        Ok(runtime) => runtime,
        Err(_) => panic!("successful backend initialization failed"),
    };
    let (entered, started) = std::sync::mpsc::sync_channel(1);
    let callback = std::thread::spawn(move || {
        let lease = match try_lease() {
            Some(lease) => lease,
            None => panic!("callback cannot acquire initialized platform"),
        };
        assert!(entered.send(()).is_ok());
        // Wait for shutdown to retire admission while retaining the callback's
        // original authority, modelling an already-entered native input event.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while try_lease().is_some() {
            assert!(
                std::time::Instant::now() < deadline,
                "cleanup never retired admission"
            );
            std::thread::yield_now();
        }
        backend.events.lock().push("callback finished");
        drop(lease);
    });
    assert!(
        started
            .recv_timeout(std::time::Duration::from_secs(1))
            .is_ok()
    );
    assert!(
        runtime
            .cleanup(|| {
                backend.events.lock().push("terminate mpv");
                Ok::<(), ()>(())
            })
            .is_ok()
    );
    assert!(callback.join().is_ok());
    assert_eq!(
        *backend.events.lock(),
        [
            "acquired host",
            "callback finished",
            "detach backend",
            "terminate mpv",
            "release host"
        ]
    );
}

#[test]
fn failed_window_termination_retains_host_and_retry_skips_backend_detachment() {
    let _serial = TEST_PLATFORM.lock();
    let (backend, prepared) = prepared(Outcome::Success);
    let runtime = match prepared.initialize(std::ptr::null_mut()) {
        Ok(runtime) => runtime,
        Err(_) => panic!("successful backend initialization failed"),
    };
    let captured_owner = std::sync::Arc::new(());
    let observer = std::sync::Arc::downgrade(&captured_owner);
    let pending: crate::blocking::Work = Box::new(move || {
        backend.events.lock().push("terminate mpv");
        drop(captured_owner);
    });
    let (pending, post_window) = match runtime.cleanup(|| Err(pending)) {
        Err(PlatformCleanupError::Termination { error, post_window }) => (error, post_window),
        _ => panic!("termination failure lost its remaining cleanup phase"),
    };
    assert_eq!(*backend.events.lock(), ["acquired host", "detach backend"]);
    assert!(
        observer.upgrade().is_some(),
        "failed termination must retain its captured owner"
    );
    assert!(
        post_window
            .retry(|| {
                pending();
                Ok::<(), ()>(())
            })
            .is_ok()
    );
    assert!(observer.upgrade().is_none());
    assert_eq!(
        *backend.events.lock(),
        [
            "acquired host",
            "detach backend",
            "terminate mpv",
            "release host"
        ]
    );
}
