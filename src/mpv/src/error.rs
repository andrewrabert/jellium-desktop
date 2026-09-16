use crate::sys;
use std::ffi::CStr;
use std::fmt;

#[derive(Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{}", self.message())]
pub struct Error {
    pub code: i32,
}

impl Error {
    pub fn new(code: i32) -> Self {
        Self { code }
    }

    pub fn message(&self) -> &'static str {
        let ptr = unsafe { sys::mpv_error_string(self.code) };
        if ptr.is_null() {
            return "unknown mpv error";
        }
        unsafe { CStr::from_ptr(ptr) }
            .to_str()
            .unwrap_or("invalid utf-8")
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "mpv::Error({}: {})", self.code, self.message())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn check(code: i32) -> Result<()> {
    if code >= 0 {
        Ok(())
    } else {
        Err(Error::new(code))
    }
}
