use std::collections::VecDeque;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::sync::Arc;

use calloop::{EventSource, Interest, Mode, Poll, PostAction, Readiness, Token, TokenFactory};

pub(crate) struct XcbSource {
    conn: Arc<xcb::Connection>,
    fd: BorrowedFd<'static>,
    pending: VecDeque<xcb::Event>,
    token: Option<Token>,
}

impl XcbSource {
    pub(crate) fn new(conn: Arc<xcb::Connection>) -> XcbSource {
        // SAFETY: the fd is the connection's socket, and `conn` (held alongside
        // it) keeps that socket open for as long as this borrow is live.
        let fd = unsafe { BorrowedFd::borrow_raw(conn.as_raw_fd()) };
        XcbSource {
            conn,
            fd,
            pending: VecDeque::new(),
            token: None,
        }
    }

    fn drain_queued(&mut self) {
        while let Ok(Some(ev)) = self.conn.poll_for_queued_event() {
            self.pending.push_back(ev);
        }
    }
}

#[derive(Debug)]
pub(crate) struct XcbSourceError(xcb::Error);

impl std::fmt::Display for XcbSourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "xcb connection error: {:?}", self.0)
    }
}

impl std::error::Error for XcbSourceError {}

impl EventSource for XcbSource {
    type Event = xcb::Event;
    type Metadata = ();
    type Ret = ();
    type Error = XcbSourceError;

    const NEEDS_EXTRA_LIFECYCLE_EVENTS: bool = true;

    fn process_events<F: FnMut(xcb::Event, &mut ())>(
        &mut self,
        _readiness: Readiness,
        token: Token,
        mut callback: F,
    ) -> Result<PostAction, XcbSourceError> {
        if self.token != Some(token) {
            return Ok(PostAction::Continue);
        }
        loop {
            match self.conn.poll_for_event() {
                Ok(Some(ev)) => self.pending.push_back(ev),
                Ok(None) => break,
                Err(e) => return Err(XcbSourceError(e)),
            }
        }
        while let Some(ev) = self.pending.pop_front() {
            callback(ev, &mut ());
        }
        Ok(PostAction::Continue)
    }

    fn register(&mut self, poll: &mut Poll, factory: &mut TokenFactory) -> calloop::Result<()> {
        let token = factory.token();
        self.token = Some(token);
        // SAFETY: `self.fd` stays valid for as long as `self.conn` is alive,
        // and unregistration always happens before this source is dropped.
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

    /// Any xcb round trip drains the socket and parses whatever events it finds
    /// into the connection's queue, so the fd can look idle to `poll(2)` while
    /// events sit unhandled. Returning synthetic readiness here is what gets
    /// those events dispatched instead of blocking on socket traffic that may
    /// never come.
    fn before_sleep(&mut self) -> calloop::Result<Option<(Readiness, Token)>> {
        self.drain_queued();
        if self.pending.is_empty() {
            return Ok(None);
        }
        let Some(token) = self.token else {
            return Ok(None);
        };
        Ok(Some((
            Readiness {
                readable: true,
                writable: false,
                error: false,
            },
            token,
        )))
    }
}
