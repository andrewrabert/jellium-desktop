//! One calloop `EventSource` over an X connection, shared by x11rb and xcb.

use std::collections::VecDeque;
use std::os::fd::{AsRawFd, BorrowedFd, RawFd};
use std::sync::Arc;

use calloop::{EventSource, Interest, Mode, Poll, PostAction, Readiness, Token, TokenFactory};
use x11rb::connection::Connection as _;
use x11rb::errors::ConnectionError;
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

/// An X connection that owns one socket and a userspace event queue.
pub(crate) trait PollConn: 'static {
    type Event;
    type Error: std::error::Error + Send + Sync + 'static;

    fn socket_fd(&self) -> RawFd;

    /// Reads the socket, then yields the next parsed event; never blocks.
    fn next_event(&self) -> Result<Option<Self::Event>, Self::Error>;

    /// Yields only events already parsed into userspace; errors read as `None`
    /// and are reported by the next `next_event`.
    fn next_queued_event(&self) -> Option<Self::Event>;
}

pub(crate) struct ConnSource<C: PollConn> {
    conn: Arc<C>,
    fd: BorrowedFd<'static>,
    pending: VecDeque<C::Event>,
    token: Option<Token>,
}

impl<C: PollConn> ConnSource<C> {
    pub(crate) fn new(conn: Arc<C>) -> ConnSource<C> {
        // SAFETY: the fd is the connection's socket, and `conn` (held alongside
        // it) keeps that socket open for as long as this borrow is live.
        let fd = unsafe { BorrowedFd::borrow_raw(conn.socket_fd()) };
        ConnSource {
            conn,
            fd,
            pending: VecDeque::new(),
            token: None,
        }
    }
}

impl<C: PollConn> EventSource for ConnSource<C> {
    type Event = C::Event;
    type Metadata = ();
    type Ret = ();
    type Error = C::Error;

    const NEEDS_EXTRA_LIFECYCLE_EVENTS: bool = true;

    fn process_events<F: FnMut(C::Event, &mut ())>(
        &mut self,
        _readiness: Readiness,
        token: Token,
        mut callback: F,
    ) -> Result<PostAction, C::Error> {
        if self.token != Some(token) {
            return Ok(PostAction::Continue);
        }
        while let Some(ev) = self.conn.next_event()? {
            self.pending.push_back(ev);
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

    /// Every X round trip drains the socket and parses whatever events it finds
    /// into userspace, so the fd can look idle to `poll(2)` while events sit
    /// unhandled. Returning synthetic readiness here is what gets those events
    /// dispatched instead of blocking on socket traffic that may never come.
    fn before_sleep(&mut self) -> calloop::Result<Option<(Readiness, Token)>> {
        while let Some(ev) = self.conn.next_queued_event() {
            self.pending.push_back(ev);
        }
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

pub(crate) type X11Source = ConnSource<RustConnection>;

pub(crate) type XcbSource = ConnSource<xcb::Connection>;

#[derive(Debug, thiserror::Error)]
pub(crate) enum X11SourceError {
    #[error("x11 connection error: {0}")]
    Connection(#[source] ConnectionError),
    #[error("x11 socket i/o error: {0}")]
    Io(#[source] std::io::Error),
}

impl PollConn for RustConnection {
    type Event = Event;
    type Error = X11SourceError;

    fn socket_fd(&self) -> RawFd {
        self.stream().as_raw_fd()
    }

    fn next_event(&self) -> Result<Option<Event>, X11SourceError> {
        match self.poll_for_event() {
            Ok(ev) => Ok(ev),
            Err(ConnectionError::IoError(e)) => Err(X11SourceError::Io(e)),
            Err(e) => Err(X11SourceError::Connection(e)),
        }
    }

    fn next_queued_event(&self) -> Option<Event> {
        self.poll_for_event().ok().flatten()
    }
}

/// `xcb::Error`'s own `Display` names only the category, so the cause is
/// formatted with `Debug` here and carried as `source()`.
#[derive(Debug, thiserror::Error)]
#[error("xcb connection error: {0:?}")]
pub(crate) struct XcbSourceError(#[source] xcb::Error);

impl PollConn for xcb::Connection {
    type Event = xcb::Event;
    type Error = XcbSourceError;

    fn socket_fd(&self) -> RawFd {
        self.as_raw_fd()
    }

    fn next_event(&self) -> Result<Option<xcb::Event>, XcbSourceError> {
        self.poll_for_event().map_err(XcbSourceError)
    }

    fn next_queued_event(&self) -> Option<xcb::Event> {
        self.poll_for_queued_event().ok().flatten()
    }
}
