//! Spawn `xdg-open <url>` detached. Caller ensures the URL is non-empty and
//! doesn't start with '-'. Also used to open local paths (xdg-open handles
//! both URLs and filesystem paths).

use std::process::{Command, Stdio};
use std::thread;

/// Before launching `xdg-open`, determine how to handle the `WAYLAND_DISPLAY` environment variable.
enum WaylandDisplay<'a> {
    /// Use the current value of `WAYLAND_DISPLAY` from the environment.
    Inherit,
    /// Override the current value of `WAYLAND_DISPLAY` with the given string.
    Set(&'a str),
    /// Remove `WAYLAND_DISPLAY` from the environment.
    Unset,
}

pub fn open(url: &str) {
    spawn(url, WaylandDisplay::Inherit);
}

/// Open the given URL using `xdg-open`, with optional control over the `WAYLAND_DISPLAY` environment variable.
pub fn open_with_wayland_display(url: &str, wayland_display: Option<&str>) {
    let display = match wayland_display {
        Some(display) => WaylandDisplay::Set(display),
        None => WaylandDisplay::Unset,
    };
    spawn(url, display);
}

fn spawn(url: &str, wayland_display: WaylandDisplay<'_>) {
    let mut command = Command::new("xdg-open");
    command
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    // apply the WAYLAND_DISPLAY handling as specified by the caller.
    match wayland_display {
        WaylandDisplay::Inherit => {}
        WaylandDisplay::Set(display) => {
            command.env("WAYLAND_DISPLAY", display);
        }
        WaylandDisplay::Unset => {
            command.env_remove("WAYLAND_DISPLAY");
        }
    }

    // spawn the process
    let child = command.spawn();
    match child {
        Ok(mut child) => {
            // xdg-open exits quickly after daemonizing the real handler; reap it.
            thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(e) => {
            tracing::error!("spawn(xdg-open) failed: {}", e);
        }
    }
}
