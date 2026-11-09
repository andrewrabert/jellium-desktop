use std::ffi::c_int;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};

use nix::fcntl::{FcntlArg, OFlag, fcntl};
use nix::unistd::write;

pub struct WakeEvent {
    read_fd: OwnedFd,
    write_fd: OwnedFd,
}

impl WakeEvent {
    pub fn new() -> Option<Self> {
        let (read, write) = std::io::pipe().ok()?;
        let read_fd = OwnedFd::from(read);
        let write_fd = OwnedFd::from(write);
        for fd in [&read_fd, &write_fd] {
            fcntl(fd, FcntlArg::F_SETFL(OFlag::O_NONBLOCK)).ok()?;
        }
        Some(WakeEvent { read_fd, write_fd })
    }

    pub fn fd(&self) -> c_int {
        self.read_fd.as_raw_fd()
    }

    pub fn signal(&self) {
        let _ = write(&self.write_fd, &[1u8]);
    }

    pub fn drain(&self) {
        crate::drain_raw_fd(self.read_fd.as_raw_fd());
    }

    /// Block until signaled. Level-triggered, so a `signal()` that lands
    /// before the call returns immediately.
    pub fn wait(&self) {
        crate::fd_wait::wait(self.read_fd.as_raw_fd());
    }
}

impl AsFd for WakeEvent {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.read_fd.as_fd()
    }
}
