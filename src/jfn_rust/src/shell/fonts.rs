fn lock() -> std::sync::RwLockWriteGuard<'static, iced_graphics::text::FontSystem> {
    match iced_graphics::text::font_system().write() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

pub(crate) fn warm(bundled: &'static [u8]) {
    lock().load_font(std::borrow::Cow::Borrowed(bundled));
}
