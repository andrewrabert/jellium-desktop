use std::ffi::c_int;
use std::os::fd::BorrowedFd;

use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};

pub fn wait(fd: c_int) {
    let fd = unsafe { BorrowedFd::borrow_raw(fd) };
    let mut fds = [PollFd::new(fd, PollFlags::POLLIN)];
    loop {
        match poll(&mut fds, PollTimeout::NONE) {
            Err(Errno::EINTR) => continue,
            Err(_) => return,
            Ok(_) => {
                if fds[0].revents().is_some_and(|r| !r.is_empty()) {
                    return;
                }
            }
        }
    }
}
