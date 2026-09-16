pub type OnText = Box<dyn FnOnce(Option<&str>) + Send>;

pub trait PrimarySelection: Send + Sync {
    fn read_text_async(&self, on_done: OnText);

    fn write_text(&self, text: &str);
}
