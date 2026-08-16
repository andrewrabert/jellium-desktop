//! The selection that exists on two backends out of four.

/// Takes a selection read's text, `None` when the read fetched none.
pub type OnText = Box<dyn FnOnce(Option<&str>) + Send>;

/// The seat's primary selection: a distinct selection with its own protocol
/// object, not a second mode of the clipboard.
pub trait PrimarySelection: Send + Sync {
    /// `None` when the selection holds no text, or the read failed.
    fn read_text_async(&self, on_done: OnText);

    /// A backend that cannot take the selection leaves the previous contents.
    fn write_text(&self, text: &str);
}
