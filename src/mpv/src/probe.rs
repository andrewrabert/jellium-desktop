use crate::handle::Handle;
use std::io::{self, Write};

const PROPS: &[&str] = &["mpv-version", "ffmpeg-version"];

pub fn version_info() -> Vec<(String, String)> {
    let Ok(handle) = Handle::create() else {
        return Vec::new();
    };
    if handle.initialize().is_err() {
        return Vec::new();
    }
    PROPS
        .iter()
        .filter_map(|name| {
            handle
                .get_property_string(name)
                .ok()
                .map(|v| ((*name).to_string(), v))
        })
        .collect()
}

pub fn jfn_mpv_print_version_info() {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for (name, value) in version_info() {
        let _ = writeln!(out, "{} {}", name, value);
    }
    let _ = out.flush();
}
