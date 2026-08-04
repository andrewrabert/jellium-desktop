//! The crate's single conversion from a CEF userfree UTF-16 string to a Rust
//! `String`.

use cef::{CefStringUserfreeUtf16, sys};

/// Empty string for a null or zero-length CEF string.
pub(crate) fn userfree_to_string(s: &CefStringUserfreeUtf16) -> String {
    let raw: Option<&sys::_cef_string_utf16_t> = s.into();
    raw.map(|r| {
        if r.str_.is_null() || r.length == 0 {
            String::new()
        } else {
            let slice = unsafe { std::slice::from_raw_parts(r.str_, r.length) };
            String::from_utf16_lossy(slice)
        }
    })
    .unwrap_or_default()
}
