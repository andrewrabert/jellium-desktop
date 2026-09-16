pub trait CefHost: Send + Sync {
    fn pump_init(&self);

    fn pump_schedule(&self, delay_ms: i64);

    fn pump_shutdown(&self);

    fn external_begin_frame(&self) -> bool;

    fn stop_frame_driver(&self);

    fn start_frame_driver(&self, driver: std::sync::Arc<dyn Fn() + Send + Sync>);
}
