//! How the platform drives CEF's message loop.
//!
//! Present (`Platform::cef_host` returns `Some`) only on backends where
//! the platform must pump CEF itself (macOS: external message pump on the
//! main CFRunLoop, CADisplayLink-driven BeginFrame).
//! Backends returning `None` run CEF's own multi-threaded message loop.

pub trait CefHost: Send + Sync {
    /// Install the pump's run-loop hooks. Runs before `CefInitialize` so
    /// the first `OnScheduleMessagePumpWork` (fired synchronously during
    /// init) finds them ready.
    fn pump_init(&self);

    /// CEF's `OnScheduleMessagePumpWork` — schedule a pump after
    /// `delay_ms` (immediately when <= 0). May fire from any thread.
    fn pump_schedule(&self, delay_ms: i64);

    /// Gate further pump dispatches before CEF state is torn down.
    fn pump_shutdown(&self);

    /// Whether browsers are created with external BeginFrame enabled —
    /// the platform drives frame production (e.g. via CADisplayLink).
    fn external_begin_frame(&self) -> bool;

    /// Stores `driver` and starts the platform's frame source. A tick never
    /// runs before the driver is stored.
    /// Stop frame callbacks and release the stored driver before native shutdown.
    fn stop_frame_driver(&self);

    fn start_frame_driver(&self, driver: std::sync::Arc<dyn Fn() + Send + Sync>);
}
