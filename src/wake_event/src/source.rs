use std::ffi::c_int;
use std::os::fd::{AsRawFd as _, BorrowedFd};

use calloop::{EventSource, Interest, Mode, Poll, PostAction, Readiness, Token, TokenFactory};

pub enum Drain {
    Never,
    BeforeCallback,
}

pub struct WakeSource {
    fd: BorrowedFd<'static>,
    drain: Drain,
    token: Option<Token>,
}

impl WakeSource {
    pub fn new(fd: c_int, drain: Drain) -> WakeSource {
        let fd = unsafe { BorrowedFd::borrow_raw(fd) };
        WakeSource {
            fd,
            drain,
            token: None,
        }
    }
}

impl EventSource for WakeSource {
    type Event = ();
    type Metadata = ();
    type Ret = ();
    type Error = std::io::Error;

    fn process_events<F: FnMut((), &mut ())>(
        &mut self,
        _readiness: Readiness,
        token: Token,
        mut callback: F,
    ) -> std::io::Result<PostAction> {
        if self.token != Some(token) {
            return Ok(PostAction::Continue);
        }
        if matches!(self.drain, Drain::BeforeCallback) {
            crate::drain_raw_fd(self.fd.as_raw_fd());
        }
        callback((), &mut ());
        Ok(PostAction::Continue)
    }

    fn register(&mut self, poll: &mut Poll, factory: &mut TokenFactory) -> calloop::Result<()> {
        let token = factory.token();
        self.token = Some(token);
        unsafe { poll.register(self.fd, Interest::READ, Mode::Level, token) }
    }

    fn reregister(&mut self, poll: &mut Poll, factory: &mut TokenFactory) -> calloop::Result<()> {
        let token = factory.token();
        self.token = Some(token);
        poll.reregister(self.fd, Interest::READ, Mode::Level, token)
    }

    fn unregister(&mut self, poll: &mut Poll) -> calloop::Result<()> {
        self.token = None;
        poll.unregister(self.fd)
    }
}
