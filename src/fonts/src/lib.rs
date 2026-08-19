//! The process's one font system.
//!
//! `cosmic_text::FontSystem` enumerates the system's fonts when it is built.
//! Two of them means two scans, and a scan running while Chromium manipulates
//! process file descriptors aborts the process — so there is exactly one, it is
//! iced's global, and every text consumer shapes through it.

pub use iced_graphics::text::cosmic_text;

/// The workspace aborts on panic, so a poisoned lock cannot outlive the panic
/// that poisoned it; the guard is taken either way rather than losing the
/// process's only font system to it.
fn lock() -> std::sync::RwLockWriteGuard<'static, iced_graphics::text::FontSystem> {
    match iced_graphics::text::font_system().write() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Construct the process's one font system — the only filesystem font
/// enumeration in the process — and load `bundled` as an in-memory face.
pub fn warm(bundled: &'static [u8]) {
    lock().load_font(std::borrow::Cow::Borrowed(bundled));
}

/// Borrow the process's one font system for the duration of `f`.
pub fn with_font_system<R>(f: impl FnOnce(&mut cosmic_text::FontSystem) -> R) -> R {
    f(lock().raw())
}
