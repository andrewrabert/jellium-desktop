//! The filesystem socket mpv connects to.
//!
//! It lives in a directory of its own rather than under a `wayland-N` name in
//! `XDG_RUNTIME_DIR`. wl-proxy's acceptor unlinks whatever already holds the
//! name it picks and never removes its own socket; inside Flatpak that name can
//! be the sandbox's link to the real compositor, which then stays broken for
//! every later launch.

use std::io;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

const SOCKET_NAME: &str = "wayland";

pub(super) struct ProxySocket {
    pub(super) listener: UnixListener,
    /// Removing this directory removes the socket's name; the listener and
    /// established connections are unaffected.
    pub(super) dir: TempDir,
}

impl ProxySocket {
    pub(super) fn bind() -> io::Result<Self> {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "XDG_RUNTIME_DIR is not set"))?;
        Self::bind_in(Path::new(&runtime))
    }

    fn bind_in(runtime: &Path) -> io::Result<Self> {
        // Created exclusively and mode 0700, so nothing that already exists in
        // `runtime` is ever reused, replaced or removed.
        let dir = tempfile::Builder::new()
            .prefix("jellium-desktop-mpv-")
            .tempdir_in(runtime)?;
        let listener = UnixListener::bind(dir.path().join(SOCKET_NAME))?;
        listener.set_nonblocking(true)?;
        Ok(Self { listener, dir })
    }

    /// The absolute path clients pass as `WAYLAND_DISPLAY`.
    pub(super) fn path(&self) -> PathBuf {
        self.dir.path().join(SOCKET_NAME)
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;
    use std::os::unix::net::UnixStream;

    use super::*;

    #[test]
    fn leaves_existing_display_entries_alone() -> io::Result<()> {
        let runtime = tempfile::tempdir()?;
        let _compositor = UnixListener::bind(runtime.path().join("compositor"))?;
        let displays = ["wayland-0", "wayland-1"];
        for name in displays {
            symlink("compositor", runtime.path().join(name))?;
        }

        let socket = ProxySocket::bind_in(runtime.path())?;
        let path = socket.path();
        assert!(path.is_absolute());
        let _client = UnixStream::connect(&path)?;
        socket.listener.accept()?;

        socket.dir.close()?;
        assert!(!path.exists());
        for name in displays {
            let entry = runtime.path().join(name);
            assert_eq!(std::fs::read_link(&entry)?, Path::new("compositor"));
            UnixStream::connect(&entry)?;
        }
        Ok(())
    }

    #[test]
    fn concurrent_sockets_do_not_collide() -> io::Result<()> {
        let runtime = tempfile::tempdir()?;
        let first = ProxySocket::bind_in(runtime.path())?;
        let second = ProxySocket::bind_in(runtime.path())?;
        assert_ne!(first.path(), second.path());
        drop(second);
        UnixStream::connect(first.path())?;
        Ok(())
    }

    #[test]
    fn failed_bind_leaves_nothing_behind() -> io::Result<()> {
        let runtime = tempfile::tempdir()?;
        // A parent longer than `sun_path` makes every socket path under it
        // unbindable.
        let parent = runtime
            .path()
            .join("x".repeat(size_of::<nix::libc::sockaddr_un>()));
        std::fs::create_dir(&parent)?;
        assert!(ProxySocket::bind_in(&parent).is_err());
        assert_eq!(std::fs::read_dir(&parent)?.count(), 0);
        Ok(())
    }
}
