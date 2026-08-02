//! Spawn `xdg-open <url>` detached. Caller ensures the URL is non-empty and
//! doesn't start with '-'. Also used to open local paths (xdg-open handles
//! both URLs and filesystem paths).
//!
//! The Wayland backend points the process-wide `WAYLAND_DISPLAY` at the mpv
//! proxy so in-process libmpv reaches us instead of the compositor. Children
//! inherit that env, so the handler `xdg-open` picks would connect to the mpv
//! proxy too, where the first `get_xdg_surface` is demoted to a subsurface on
//! the assumption it's mpv. A browser starting cold then exports that surface
//! via `zxdg_exporter_v2`, the compositor rejects it ("exported surface had an
//! invalid role"), and the fatal `wl_display` error takes down the connection
//! we share with mpv. [`set_host_wayland_display`] records the compositor's
//! socket so spawned children go there instead.

use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::thread;

/// Compositor `WAYLAND_DISPLAY` as it was before the mpv proxy overrode the
/// process env. `Some(None)` means there was none to begin with, so children
/// must not inherit the proxy's.
static HOST_WAYLAND_DISPLAY: OnceLock<Option<String>> = OnceLock::new();

/// Record the compositor's `WAYLAND_DISPLAY`. Call once, before the mpv proxy
/// overrides the process env; later calls are ignored.
pub fn set_host_wayland_display(display: Option<String>) {
    let _ = HOST_WAYLAND_DISPLAY.set(display);
}

/// `None` (never recorded) leaves the child's env alone: X11, or the proxy
/// never started, so what we inherited is already the compositor's.
fn apply_host_wayland_display(cmd: &mut Command, host: Option<Option<&str>>) {
    match host {
        Some(Some(display)) => {
            cmd.env("WAYLAND_DISPLAY", display);
        }
        Some(None) => {
            cmd.env_remove("WAYLAND_DISPLAY");
        }
        None => {}
    }
}

pub fn open(url: &str) {
    let mut cmd = Command::new("xdg-open");
    cmd.arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    apply_host_wayland_display(&mut cmd, HOST_WAYLAND_DISPLAY.get().map(Option::as_deref));

    match cmd.spawn() {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    /// `Command::get_envs` reports only explicit overrides, so an absent key
    /// means "inherit" and `Some(None)` means "remove before exec".
    fn override_for(cmd: &Command, key: &str) -> Option<Option<String>> {
        cmd.get_envs()
            .find(|(k, _)| *k == OsStr::new(key))
            .map(|(_, v)| v.map(|v| v.to_string_lossy().into_owned()))
    }

    #[test]
    fn unrecorded_host_leaves_child_env_untouched() {
        let mut cmd = Command::new("xdg-open");
        apply_host_wayland_display(&mut cmd, None);
        assert_eq!(override_for(&cmd, "WAYLAND_DISPLAY"), None);
    }

    #[test]
    fn recorded_host_overrides_the_proxy_display() {
        let mut cmd = Command::new("xdg-open");
        apply_host_wayland_display(&mut cmd, Some(Some("wayland-1")));
        assert_eq!(
            override_for(&cmd, "WAYLAND_DISPLAY"),
            Some(Some("wayland-1".to_string()))
        );
    }

    #[test]
    fn host_without_a_display_clears_the_proxy_display() {
        let mut cmd = Command::new("xdg-open");
        apply_host_wayland_display(&mut cmd, Some(None));
        assert_eq!(override_for(&cmd, "WAYLAND_DISPLAY"), Some(None));
    }
}
