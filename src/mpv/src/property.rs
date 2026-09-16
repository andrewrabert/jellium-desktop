use crate::sys;

pub trait Format: Sized {
    const MPV_FORMAT: sys::mpv_format;
}

impl Format for i64 {
    const MPV_FORMAT: sys::mpv_format = sys::mpv_format::MPV_FORMAT_INT64;
}

impl Format for f64 {
    const MPV_FORMAT: sys::mpv_format = sys::mpv_format::MPV_FORMAT_DOUBLE;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Flag(pub bool);

impl From<bool> for Flag {
    fn from(b: bool) -> Self {
        Self(b)
    }
}

impl From<Flag> for bool {
    fn from(f: Flag) -> Self {
        f.0
    }
}

impl Format for Flag {
    const MPV_FORMAT: sys::mpv_format = sys::mpv_format::MPV_FORMAT_FLAG;
}
