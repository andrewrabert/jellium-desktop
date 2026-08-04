use std::collections::VecDeque;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::sync::Arc;

use calloop::{EventSource, Interest, Mode, Poll, PostAction, Readiness, Token, TokenFactory};
use x11rb::connection::Connection as _;
use x11rb::errors::ConnectionError;
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

pub struct X11Source {
    conn: Arc<RustConnection>,
    fd: BorrowedFd<'static>,
    pending: VecDeque<Event>,
    token: Option<Token>,
}

impl X11Source {
    pub fn new(conn: Arc<RustConnection>) -> X11Source {
        // SAFETY: the fd is the connection's socket, and `conn` (held alongside
        // it) keeps that socket open for as long as this borrow is live.
        let fd = unsafe { BorrowedFd::borrow_raw(conn.stream().as_raw_fd()) };
        X11Source {
            conn,
            fd,
            pending: VecDeque::new(),
            token: None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum X11SourceError {
    #[error("x11 connection error: {0}")]
    Connection(#[source] ConnectionError),
    #[error("x11 socket i/o error: {0}")]
    Io(#[source] std::io::Error),
}

impl EventSource for X11Source {
    type Event = Event;
    type Metadata = ();
    type Ret = ();
    type Error = X11SourceError;

    const NEEDS_EXTRA_LIFECYCLE_EVENTS: bool = true;

    fn process_events<F: FnMut(Event, &mut ())>(
        &mut self,
        _readiness: Readiness,
        token: Token,
        mut callback: F,
    ) -> Result<PostAction, X11SourceError> {
        if self.token != Some(token) {
            return Ok(PostAction::Continue);
        }
        loop {
            match self.conn.poll_for_event() {
                Ok(Some(ev)) => self.pending.push_back(ev),
                Ok(None) => break,
                Err(ConnectionError::IoError(e)) => return Err(X11SourceError::Io(e)),
                Err(e) => return Err(X11SourceError::Connection(e)),
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

    /// Every x11rb round trip (`reply()`) drains the socket and parses whatever
    /// events it finds into userspace, so the fd can look idle to `poll(2)`
    /// while events sit unhandled. Returning synthetic readiness here is what
    /// gets those events dispatched instead of blocking on socket traffic that
    /// may never come.
    fn before_sleep(&mut self) -> calloop::Result<Option<(Readiness, Token)>> {
        loop {
            match self.conn.poll_for_event() {
                Ok(Some(ev)) => self.pending.push_back(ev),
                Ok(None) => break,
                Err(_) => break,
            }
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
