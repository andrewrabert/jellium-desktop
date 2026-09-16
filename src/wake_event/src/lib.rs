#[cfg(target_os = "linux")]
#[path = "eventfd.rs"]
mod imp;
#[cfg(all(unix, not(target_os = "linux")))]
#[path = "pipe.rs"]
mod imp;
#[cfg(windows)]
#[path = "event.rs"]
mod imp;

#[cfg(unix)]
mod fd_wait;
#[cfg(target_os = "linux")]
mod source;

pub use imp::WakeEvent;
#[cfg(target_os = "linux")]
pub use source::{Drain, WakeSource};

#[cfg(unix)]
pub fn drain_raw_fd(fd: std::ffi::c_int) {
    let fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
    let mut buf = [0u8; 64];
    loop {
        match nix::unistd::read(fd, &mut buf) {
            Ok(0) => break,
            Ok(_) => continue,
            Err(nix::errno::Errno::EINTR) => continue,
            Err(_) => break,
        }
    }
}

#[cfg(unix)]
pub fn signal_raw_fd(fd: std::ffi::c_int) {
    let val: u64 = 1;
    let fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
    let _ = nix::unistd::write(fd, &val.to_ne_bytes());
}
