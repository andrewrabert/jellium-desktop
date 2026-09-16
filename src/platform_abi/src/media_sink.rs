pub trait MediaSink: Send + Sync {
    fn start(&self, instance: &crate::Instance);
    fn stop(&self);
}
