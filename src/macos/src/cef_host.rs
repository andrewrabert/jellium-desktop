//! macOS [`CefHost`]: external message pump on the
//! main CFRunLoop + CADisplayLink-driven BeginFrame.

use jfn_platform_abi::CefHost;

pub struct MacosCefHost;

impl CefHost for MacosCefHost {
    fn pump_init(&self) {
        crate::cef_pump::init();
    }

    fn pump_schedule(&self, delay_ms: i64) {
        crate::cef_pump::on_schedule(delay_ms);
    }

    fn pump_shutdown(&self) {
        crate::cef_pump::shutdown();
    }

    fn external_begin_frame(&self) -> bool {
        true
    }

    fn stop_frame_driver(&self) {
        crate::init::stop_frame_driver();
    }

    fn start_frame_driver(&self, driver: std::sync::Arc<dyn Fn() + Send + Sync>) {
        if !crate::init::start_frame_driver(driver) {
            tracing::error!(target: "Platform", "[INIT] failed to start CADisplayLink");
        }
    }
}
