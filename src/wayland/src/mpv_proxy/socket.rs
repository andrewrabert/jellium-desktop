//! The proxy owns a private namespace, never a compositor's `wayland-N` entry.

use std::io;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};

pub(super) struct ProxySocket {
    pub(super) listener: UnixListener,
    // Declared after listener so the fd closes before the directory is removed.
    directory: tempfile::TempDir,
}

impl ProxySocket {
    pub(super) fn new() -> io::Result<Self> {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "XDG_RUNTIME_DIR is not set"))?;
        Self::bind_in(Path::new(&runtime))
    }

    fn bind_in(runtime: &Path) -> io::Result<Self> {
        if !runtime.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "XDG_RUNTIME_DIR must be absolute",
            ));
        }
        // Exclusive creation, mode 0700. A previous crash's directory is never
        // reused or removed. TempDir also rolls back failed initialization.
        let directory = tempfile::Builder::new()
            .prefix("jellium-proxy-")
            .tempdir_in(runtime)?;
        let listener = UnixListener::bind(directory.path().join("mpv"))?;
        listener.set_nonblocking(true)?;
        Ok(Self { listener, directory })
    }

    pub(super) fn path(&self) -> PathBuf {
        self.directory.path().join("mpv")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::os::unix::net::UnixStream;

    #[test]
    fn preserves_flatpak_mapping_and_other_listeners() -> io::Result<()> {
        let runtime = tempfile::tempdir()?;
        let upstream = runtime.path().join("upstream");
        let _compositor = UnixListener::bind(&upstream)?;
        for name in ["wayland-0", "wayland-1", "another-display"] {
            symlink("upstream", runtime.path().join(name))?;
        }
        let proxy = ProxySocket::bind_in(runtime.path())?;
        let path = proxy.path();
        assert!(path.is_absolute());
        assert_eq!(
            std::fs::metadata(proxy.directory.path())?.permissions().mode() & 0o777,
            0o700
        );
        let _client = UnixStream::connect(&path)?;
        let _accepted = proxy.listener.accept()?;
        drop(proxy);
        assert!(!path.exists());
        assert!(!path.parent().is_some_and(Path::exists));
        for name in ["wayland-0", "wayland-1", "another-display"] {
            assert_eq!(
                std::fs::read_link(runtime.path().join(name))?,
                Path::new("upstream")
            );
            let _client = UnixStream::connect(runtime.path().join(name))?;
        }
        Ok(())
    }

    #[test]
    fn repeated_initialization_does_not_reuse_an_existing_proxy() -> io::Result<()> {
        let runtime = tempfile::tempdir()?;
        // An existing namespace (including a crash's leftovers) cannot collide.
        let existing = ProxySocket::bind_in(runtime.path())?;
        let existing_path = existing.path();
        let next = ProxySocket::bind_in(runtime.path())?;
        let next_path = next.path();
        assert_ne!(existing_path, next_path);
        drop(next); // Also models failure after binding but before publication.
        assert!(!next_path.exists());
        assert!(existing_path.exists());
        let again = ProxySocket::bind_in(runtime.path())?;
        assert_ne!(existing_path, again.path());
        Ok(())
    }

    #[test]
    fn failed_bind_removes_its_private_directory() -> io::Result<()> {
        let runtime = tempfile::tempdir()?;
        // Derive an overlong pathname from this platform's Unix socket limit.
        let long = runtime
            .path()
            .join("x".repeat(std::mem::size_of::<nix::libc::sockaddr_un>()));
        std::fs::create_dir(&long)?;
        assert!(ProxySocket::bind_in(&long).is_err());
        assert_eq!(std::fs::read_dir(&long)?.count(), 0);
        Ok(())
    }
}
