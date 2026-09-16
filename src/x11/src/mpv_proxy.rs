//! X11 proxy mpv connects to instead of the real server.
//!
//! It binds a fresh display socket, points mpv's `DISPLAY` at it (via env,
//! before `mpv_create`), and forwards every byte — and every `SCM_RIGHTS` fd —
//! to the real server. Forwarding ancillary fds is mandatory: mpv's gpu VO
//! (DRI3/Present) and MIT-SHM pass dmabuf/shm fds over the socket, so a
//! byte-only splice would break video.
//!
//! Both directions are framed. The app is the sole windowing authority; mpv
//! keeps only rendering, and the proxy enforces that:
//!
//! Client→server ([`ReqParser`]) — enforcement:
//! - capture mpv's embed sub-window from `CreateWindow` parented to the
//!   app's video host, solely to key the rewrites below (the geometry
//!   thread discovers the window independently, via `CreateNotify` on the
//!   video host, and sizes it in the same batch as the host and overlays);
//! - neutralize `ConfigureWindow`/`CirculateWindow` on the embed sub-window
//!   (this includes mpv's raise — stack mode is a `ConfigureWindow` value),
//!   so only the app sizes and stacks it;
//! - strip the `cursor` value from `ChangeWindowAttributes` and neutralize
//!   XFixes `HideCursor`/`ShowCursor`, so the video area inherits the
//!   app-controlled host cursor (a cursor attribute applies whenever the
//!   pointer is over the window, regardless of event selection).
//!
//! Client→server — defensive backstop (mpv runs embedded and skips all WM
//! interaction, so in normal operation none of these fire):
//! - force `override_redirect` on a root-parented `CreateWindow`, so the WM
//!   never manages an mpv top-level (not even a taskbar-entry flash);
//! - neutralize mpv's `SetInputFocus` to `NoOperation`, so mpv can't steal
//!   keyboard focus from the app top-level;
//! - neutralize `ConfigureWindow` on a root-parented mpv window, so only
//!   the app ever moves/resizes toplevel geometry;
//! - neutralize `_NET_WM_STATE` traffic (`ChangeProperty` on an mpv
//!   top-level, fullscreen `SendEvent` client messages), so fullscreen flows
//!   only through the app's toplevel path.
//!
//! Server→client ([`EventFramer`]) — coalescing: drop a video-host
//! `ConfigureNotify` whose size differs from the geometry the app last
//! published ([`publish_host_geometry`]). Every reconcile publishes and then
//! resizes the host, so a matching notify always follows; mpv sees one
//! settled notify per reconcile instead of the full drag burst.

use std::borrow::Cow;
use std::io::{self, IoSlice, IoSliceMut, Write};
use std::net::TcpStream;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::{self, JoinHandle};

use calloop::generic::Generic;
use calloop::{EventLoop, Interest, Mode, PostAction};
use nix::errno::Errno;
use nix::sys::socket::{
    ControlMessage, ControlMessageOwned, MsgFlags, Shutdown, recvmsg, send, sendmsg, shutdown,
};
use parking_lot::Mutex;
use x11rb::connection::Connection as _;
use x11rb::reexports::x11rb_protocol::errors::ParseError;
use x11rb::reexports::x11rb_protocol::parse_display::{
    ConnectAddress, ParsedDisplay, parse_display,
};
use x11rb::reexports::x11rb_protocol::protocol::xfixes::{
    HIDE_CURSOR_REQUEST, SHOW_CURSOR_REQUEST,
};
use x11rb::reexports::x11rb_protocol::protocol::xproto::{
    CHANGE_PROPERTY_REQUEST, CHANGE_WINDOW_ATTRIBUTES_REQUEST, CIRCULATE_WINDOW_REQUEST,
    CLIENT_MESSAGE_EVENT, CONFIGURE_NOTIFY_EVENT, CONFIGURE_WINDOW_REQUEST, CREATE_WINDOW_REQUEST,
    ChangePropertyRequest, ChangeWindowAttributesRequest, ConfigureNotifyEvent,
    ConfigureWindowRequest, CreateWindowRequest, GE_GENERIC_EVENT, NO_OPERATION_REQUEST,
    SEND_EVENT_REQUEST, SET_INPUT_FOCUS_REQUEST, SendEventRequest, SetupRequest,
};
use x11rb::reexports::x11rb_protocol::x11_utils::{
    BigRequests, RequestHeader, TryParse, parse_request_header,
};
use x11rb::reexports::x11rb_protocol::xauth::{Family, get_auth};

/// The kernel caps `SCM_RIGHTS` at this many fds per message; a cmsg buffer
/// sized for it can never truncate fds.
const MAX_FDS_PER_MSG: usize = 253;
const CHUNK: usize = 64 * 1024;
/// `FamilyLocal` in the `.Xauthority` on-wire format (a `u16`, unlike the
/// core-protocol `Family` which is a `u8`).
const FAMILY_LOCAL: u16 = 256;

#[derive(Clone)]
enum UpstreamAddr {
    Abstract(u16),
    Path(String),
    Tcp(String, u16),
}

/// Which server `DISPLAY`/`XAUTHORITY` currently point at. The proxy repoints
/// the environment to itself only for mpv's connect; app connections made in
/// [`DisplayEpoch::Proxy`] must target the real server explicitly (via
/// [`real_display`]) since they'd otherwise route through the proxy.
#[derive(PartialEq, Eq)]
enum DisplayEpoch {
    /// Repointed to the proxy; mpv connects here.
    Proxy,
    /// Restored to the real server after mpv has connected.
    RealRestored,
}

/// Sockets, temp files, and the accept thread are cleaned up on `Drop`, so an
/// unwinding path can't leak them. The `DISPLAY`/`XAUTHORITY` restore is *not*
/// a drop-time concern: it happens mid-life, once mpv has connected, via
/// [`restore_real_display`].
struct ProxyState {
    accept_thread: Option<JoinHandle<()>>,
    stop: calloop::ping::Ping,
    /// Filesystem socket to unlink (dropping the listener does not).
    fs_socket_path: PathBuf,
    xauth_temp: Option<PathBuf>,
    orig_display: Option<String>,
    orig_xauth: Option<String>,
    epoch: DisplayEpoch,
}

impl Drop for ProxyState {
    fn drop(&mut self) {
        self.stop.ping();
        if let Some(h) = self.accept_thread.take() {
            let _ = h.join();
        }
        let _ = std::fs::remove_file(&self.fs_socket_path);
        if let Some(p) = &self.xauth_temp {
            let _ = std::fs::remove_file(p);
        }
    }
}

fn state() -> &'static Mutex<Option<ProxyState>> {
    static S: OnceLock<Mutex<Option<ProxyState>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(None))
}

struct EmbedContext {
    video_host: u32,
    net_wm_state: u32,
    xfixes_opcode: Option<u8>,
    /// The video host size (width, height) the app last committed; the event
    /// framer forwards only `ConfigureNotify` matching it.
    published: (u16, u16),
}

struct SharedEmbed {
    ctx: Mutex<Option<EmbedContext>>,
    /// mpv's embed sub-window XID, captured from `CreateWindow`; 0 = unknown.
    embed_window: AtomicU32,
}

impl SharedEmbed {
    const fn new() -> Self {
        Self {
            ctx: Mutex::new(None),
            embed_window: AtomicU32::new(0),
        }
    }
}

static EMBED: SharedEmbed = SharedEmbed::new();

/// Hand the proxy the app's windowing context. Must run before mpv connects
/// (i.e. before mpv init); `width`/`height` seed the published host size.
pub fn set_embed_context(
    video_host: u32,
    net_wm_state: u32,
    xfixes_opcode: Option<u8>,
    width: u16,
    height: u16,
) {
    *EMBED.ctx.lock() = Some(EmbedContext {
        video_host,
        net_wm_state,
        xfixes_opcode,
        published: (width, height),
    });
}

/// Publish the video host size the app is about to commit. Must be called
/// before the `ConfigureWindow` that applies it reaches the server, so the
/// resulting `ConfigureNotify` is never mistaken for a stale one and dropped.
pub fn publish_host_geometry(width: u16, height: u16) {
    if let Some(ctx) = EMBED.ctx.lock().as_mut() {
        ctx.published = (width, height);
    }
}

/// Start the proxy and repoint `DISPLAY`/`XAUTHORITY` at it. Returns `false`
/// (leaving the environment untouched) if it cannot bind or resolve the
/// upstream server. Idempotent.
pub fn start() -> bool {
    // Repointing DISPLAY here must come *after* the app has created its Vulkan
    // instance on the real server; otherwise NVIDIA's ICD lazy global init
    // routes its internal XOpenDisplay through the proxy and races mpv's VO
    // thread. `crate::paint::resolve_and_store` runs first in `prepare`.
    debug_assert!(
        crate::paint::is_resolved(),
        "paint tier must resolve before the proxy repoints DISPLAY",
    );
    let mut guard = state().lock();
    if guard.is_some() {
        return true;
    }

    let orig_display = std::env::var("DISPLAY").ok();
    let parsed = match parse_display(None) {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(target: "Main", "cannot parse DISPLAY: {e}");
            return false;
        }
    };
    let upstream = upstream_addresses(&parsed);

    // Root window IDs of the real server, one per screen: the request parser
    // classifies a CreateWindow as an mpv top-level by parent ∈ roots (mpv
    // normally embeds into the app's video host and never hits this).
    let roots: Arc<[u32]> = match x11rb::rust_connection::RustConnection::connect(None) {
        Ok((c, _)) => c.setup().roots.iter().map(|s| s.root).collect(),
        Err(e) => {
            tracing::error!(target: "Main", "cannot query X server roots for proxy: {e}");
            return false;
        }
    };

    let Some(bound) = bind_listeners() else {
        tracing::error!(target: "Main", "no free X11 display socket for the proxy");
        return false;
    };

    let (stop, stop_source) = match calloop::ping::make_ping() {
        Ok(pair) => pair,
        Err(e) => {
            tracing::error!(target: "Main", "proxy stop ping failed: {e}");
            let _ = std::fs::remove_file(&bound.fs_path);
            return false;
        }
    };

    let orig_xauth = std::env::var("XAUTHORITY").ok();
    let xauth_temp = provision_auth(parsed.display, bound.number).unwrap_or(None);

    let BoundListeners {
        abstract_l,
        fs_l,
        fs_path,
        number,
    } = bound;
    let accept_thread = thread::Builder::new()
        .name("x11-proxy-accept".into())
        .spawn(move || run_acceptor(abstract_l, fs_l, stop_source, upstream, roots))
        .ok();
    let Some(accept_thread) = accept_thread else {
        tracing::error!(target: "Main", "proxy accept thread spawn failed");
        let _ = std::fs::remove_file(&fs_path);
        return false;
    };

    let new_display = if parsed.screen == 0 {
        format!(":{number}")
    } else {
        format!(":{number}.{}", parsed.screen)
    };
    unsafe { std::env::set_var("DISPLAY", &new_display) };
    if let Some(p) = &xauth_temp {
        unsafe { std::env::set_var("XAUTHORITY", p) };
    }

    tracing::info!(target: "Main", "x11 proxy listening on {new_display}");
    *guard = Some(ProxyState {
        accept_thread: Some(accept_thread),
        stop,
        fs_socket_path: fs_path,
        xauth_temp,
        orig_display,
        orig_xauth,
        epoch: DisplayEpoch::Proxy,
    });
    true
}

/// The display string of the real server while the proxy has `DISPLAY`
/// repointed, or `None` when the proxy isn't active (callers then fall back
/// to the environment). App-side connections that may run before
/// [`restore_real_display`] use this so they never route through the proxy.
pub fn real_display() -> Option<String> {
    let guard = state().lock();
    let st = guard.as_ref()?;
    if st.epoch == DisplayEpoch::RealRestored {
        return None;
    }
    st.orig_display.clone()
}

/// Put `DISPLAY`/`XAUTHORITY` back to the real server so the app's own
/// connections bypass the proxy. Must not run until mpv has connected (i.e.
/// after `mpv_initialize`); idempotent.
pub fn restore_real_display() {
    let mut guard = state().lock();
    let Some(st) = guard.as_mut() else {
        return;
    };
    if st.epoch == DisplayEpoch::RealRestored {
        return;
    }
    st.epoch = DisplayEpoch::RealRestored;
    match &st.orig_display {
        Some(d) => unsafe { std::env::set_var("DISPLAY", d) },
        None => unsafe { std::env::remove_var("DISPLAY") },
    }
    match &st.orig_xauth {
        Some(x) => unsafe { std::env::set_var("XAUTHORITY", x) },
        None if st.xauth_temp.is_some() => unsafe { std::env::remove_var("XAUTHORITY") },
        None => {}
    }
}

/// Stop accepting new connections and clean up the socket + temp auth files
/// (via [`ProxyState`]'s `Drop`). Established relays drain on their own when mpv
/// closes its connection.
pub fn stop() {
    // Take out of the lock first, then drop outside it so the thread join in
    // `Drop` never runs while holding the state mutex.
    let taken = state().lock().take();
    drop(taken);
}

struct BoundListeners {
    abstract_l: UnixListener,
    fs_l: UnixListener,
    fs_path: PathBuf,
    number: u32,
}

/// Find a display number free on both the abstract and filesystem X sockets and
/// bind both, so libxcb (abstract-first on Linux) and legacy path clients agree.
fn bind_listeners() -> Option<BoundListeners> {
    for number in 64u32..1024 {
        let name = format!("/tmp/.X11-unix/X{number}");
        let Ok(addr) = SocketAddr::from_abstract_name(name.as_bytes()) else {
            continue;
        };
        let Ok(abstract_l) = UnixListener::bind_addr(&addr) else {
            continue;
        };
        let fs_path = PathBuf::from(&name);
        let Ok(fs_l) = UnixListener::bind(&fs_path) else {
            continue;
        };
        return Some(BoundListeners {
            abstract_l,
            fs_l,
            fs_path,
            number,
        });
    }
    None
}

fn run_acceptor(
    abstract_l: UnixListener,
    fs_l: UnixListener,
    stop: calloop::ping::PingSource,
    upstream: Vec<UpstreamAddr>,
    roots: Arc<[u32]>,
) {
    let mut event_loop: EventLoop<'_, ()> = match EventLoop::try_new() {
        Ok(el) => el,
        Err(e) => {
            tracing::error!(target: "x11-proxy", "acceptor event loop creation failed: {e}");
            return;
        }
    };
    let signal = event_loop.get_signal();
    let handle = event_loop.handle();

    for listener in [abstract_l, fs_l] {
        let upstream = upstream.clone();
        let roots = roots.clone();
        let res = handle.insert_source(
            Generic::new(listener, Interest::READ, Mode::Level),
            move |_readiness, listener, ()| {
                accept_one(listener, &upstream, &roots);
                Ok(PostAction::Continue)
            },
        );
        if let Err(e) = res {
            tracing::error!(target: "x11-proxy", "listener source registration failed: {e}");
            return;
        }
    }

    if let Err(e) = handle.insert_source(stop, move |(), (), ()| signal.stop()) {
        tracing::error!(target: "x11-proxy", "stop source registration failed: {e}");
        return;
    }

    if let Err(e) = event_loop.run(None, &mut (), |()| {}) {
        tracing::error!(target: "x11-proxy", "acceptor event loop exited: {e}");
    }
}

fn accept_one(listener: &UnixListener, upstream: &[UpstreamAddr], roots: &Arc<[u32]>) {
    match listener.accept() {
        Ok((stream, _)) => {
            let up = upstream.to_vec();
            let roots = roots.clone();
            let _ = thread::Builder::new()
                .name("x11-proxy-conn".into())
                .spawn(move || handle_conn(stream, up, roots));
        }
        Err(e) => tracing::debug!(target: "x11-proxy", "accept failed: {e}"),
    }
}

fn handle_conn(client: UnixStream, upstream: Vec<UpstreamAddr>, roots: Arc<[u32]>) {
    let up = match connect_upstream(&upstream) {
        Ok(fd) => fd,
        Err(e) => {
            tracing::debug!(target: "x11-proxy", "upstream connect failed: {e}");
            return;
        }
    };
    let client: OwnedFd = client.into();
    let cf = client.as_raw_fd();
    let uf = up.as_raw_fd();
    // Shared parse-desync flag: byte order and stream health are properties
    // of the connection, so once either direction goes blind (verbatim relay)
    // the other must too — its length fields would misparse the same way.
    let blind = Arc::new(AtomicBool::new(false));
    let blind2 = blind.clone();
    thread::scope(|s| {
        s.spawn(|| pump_requests(cf, uf, roots, blind));
        s.spawn(|| pump_replies(uf, cf, blind2));
    });
}

/// Build the ordered upstream-address candidates from a parsed `DISPLAY`,
/// reusing x11rb's resolution and prepending the Linux abstract socket (which
/// x11rb does not try) for local servers.
fn upstream_addresses(parsed: &ParsedDisplay) -> Vec<UpstreamAddr> {
    let candidates: Vec<ConnectAddress<'_>> = parsed.connect_instruction().collect();
    let mut addrs = Vec::new();
    if candidates
        .iter()
        .any(|c| matches!(c, ConnectAddress::Socket(_)))
    {
        addrs.push(UpstreamAddr::Abstract(parsed.display));
    }
    for c in candidates {
        match c {
            ConnectAddress::Socket(path) => addrs.push(UpstreamAddr::Path(path)),
            ConnectAddress::Hostname(host, port) => {
                addrs.push(UpstreamAddr::Tcp(host.to_string(), port));
            }
            _ => {}
        }
    }
    addrs
}

fn connect_upstream(upstream: &[UpstreamAddr]) -> io::Result<OwnedFd> {
    let mut last = None;
    for addr in upstream {
        let result = match addr {
            UpstreamAddr::Abstract(number) => {
                let name = format!("/tmp/.X11-unix/X{number}");
                SocketAddr::from_abstract_name(name.as_bytes())
                    .and_then(|a| UnixStream::connect_addr(&a))
                    .map(OwnedFd::from)
            }
            UpstreamAddr::Path(path) => UnixStream::connect(path).map(OwnedFd::from),
            UpstreamAddr::Tcp(host, port) => TcpStream::connect((host.as_str(), *port)).map(|s| {
                let _ = s.set_nodelay(true);
                OwnedFd::from(s)
            }),
        };
        match result {
            Ok(fd) => return Ok(fd),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no upstream address")))
}

/// Client→server pump: frames the X11 request stream so [`ReqParser`] can
/// rewrite requests. Only whole requests are forwarded; a request split
/// across reads (and any fds it carries) is held until complete.
fn pump_requests(from: RawFd, to: RawFd, roots: Arc<[u32]>, blind: Arc<AtomicBool>) {
    let mut buf = vec![0u8; CHUNK];
    let mut cmsg = nix::cmsg_space!([RawFd; MAX_FDS_PER_MSG]);
    let mut parser = ReqParser::new(roots, &EMBED, blind);
    let mut held: Vec<u8> = Vec::new();
    let mut held_fds: Vec<OwnedFd> = Vec::new();
    let mut out: Vec<u8> = Vec::new();
    loop {
        match recv_with_fds(from, &mut buf, &mut cmsg) {
            Ok((0, _)) => break,
            Ok((n, mut fds)) => {
                let mut data = std::mem::take(&mut held);
                data.extend_from_slice(&buf[..n]);
                held_fds.append(&mut fds);
                out.clear();
                let consumed = parser.process(&data, &mut out);
                if consumed == 0 {
                    held = data;
                    continue;
                }
                // Flush all pending fds now: fds must reach the server no later
                // than the request that consumes them, and delivering them with
                // an earlier send is harmless (the server queues them in order).
                let fds_out = std::mem::take(&mut held_fds);
                if let Err(e) = send_with_fds(to, &out, fds_out) {
                    tracing::debug!(target: "x11-proxy", "relay send failed: {e}");
                    break;
                }
                held = data.split_off(consumed);
            }
            Err(e) => {
                tracing::debug!(target: "x11-proxy", "relay recv failed: {e}");
                break;
            }
        }
    }
    let _ = shutdown(from, Shutdown::Both);
    let _ = shutdown(to, Shutdown::Both);
}

/// Server→client pump: frames the reply/event/error stream so [`EventFramer`]
/// can drop stale video-host `ConfigureNotify` events. Only whole units are
/// forwarded; a unit split across reads is held until complete.
fn pump_replies(from: RawFd, to: RawFd, blind: Arc<AtomicBool>) {
    let mut buf = vec![0u8; CHUNK];
    let mut cmsg = nix::cmsg_space!([RawFd; MAX_FDS_PER_MSG]);
    let mut framer = EventFramer::new(&EMBED, blind);
    let mut held: Vec<u8> = Vec::new();
    let mut held_fds: Vec<OwnedFd> = Vec::new();
    let mut out: Vec<u8> = Vec::new();
    loop {
        match recv_with_fds(from, &mut buf, &mut cmsg) {
            Ok((0, _)) => break,
            Ok((n, mut fds)) => {
                let mut data = std::mem::take(&mut held);
                data.extend_from_slice(&buf[..n]);
                held_fds.append(&mut fds);
                out.clear();
                let consumed = framer.process(&data, &mut out);
                held = data.split_off(consumed);
                if out.is_empty() {
                    // Nothing forwarded (partial unit, or every complete unit
                    // was dropped) — keep any fds held; they belong to a reply
                    // that has not been sent yet, never to a dropped event.
                    continue;
                }
                let fds_out = std::mem::take(&mut held_fds);
                if let Err(e) = send_with_fds(to, &out, fds_out) {
                    tracing::debug!(target: "x11-proxy", "relay send failed: {e}");
                    break;
                }
            }
            Err(e) => {
                tracing::debug!(target: "x11-proxy", "relay recv failed: {e}");
                break;
            }
        }
    }
    let _ = shutdown(from, Shutdown::Both);
    let _ = shutdown(to, Shutdown::Both);
}

/// Ceiling on a single request's or reply's byte length; past it we assume a
/// parse desync and fall back to a verbatim relay rather than buffer unbounded.
const MAX_REQUEST_BYTES: usize = 64 * 1024 * 1024;

/// Same-length rewrite to `NoOperation`, which accepts any request length and
/// has no reply, so sequence numbers stay intact.
fn emit_noop(raw: &[u8], out: &mut Vec<u8>) {
    let start = out.len();
    out.extend_from_slice(raw);
    out[start] = NO_OPERATION_REQUEST;
}

struct ReqParser<'a> {
    setup_done: bool,
    /// Root window IDs of the real server, one per screen.
    roots: Arc<[u32]>,
    /// Root-parented windows mpv created (would-be top-levels).
    toplevels: std::collections::HashSet<u32>,
    embed: &'a SharedEmbed,
    video_host: Option<u32>,
    net_wm_state: Option<u32>,
    xfixes_opcode: Option<u8>,
    blind: Arc<AtomicBool>,
}

impl<'a> ReqParser<'a> {
    fn new(roots: Arc<[u32]>, embed: &'a SharedEmbed, blind: Arc<AtomicBool>) -> Self {
        let ctx = embed.ctx.lock();
        Self {
            setup_done: false,
            roots,
            toplevels: std::collections::HashSet::new(),
            embed,
            video_host: ctx.as_ref().map(|c| c.video_host),
            net_wm_state: ctx.as_ref().map(|c| c.net_wm_state),
            xfixes_opcode: ctx.as_ref().and_then(|c| c.xfixes_opcode),
            blind,
        }
    }

    fn go_blind(&self) {
        self.blind.store(true, Ordering::Relaxed);
    }

    /// Append the leading complete requests of `input` to `out` (rewritten
    /// where needed) and return how many `input` bytes were consumed. A
    /// trailing partial request is left for the next call.
    fn process(&mut self, input: &[u8], out: &mut Vec<u8>) -> usize {
        if self.blind.load(Ordering::Relaxed) {
            out.extend_from_slice(input);
            return input.len();
        }

        let mut off = 0;
        if !self.setup_done {
            // x11rb-protocol silently misparses non-native byte order.
            let native = if cfg!(target_endian = "little") {
                b'l'
            } else {
                b'B'
            };
            if input.first() != Some(&native) {
                self.go_blind();
                out.extend_from_slice(input);
                return input.len();
            }
            match SetupRequest::try_parse(input) {
                Ok((_, remaining)) => {
                    let total = input.len() - remaining.len();
                    out.extend_from_slice(&input[..total]);
                    off = total;
                    self.setup_done = true;
                }
                Err(ParseError::InsufficientData) => return 0,
                Err(_) => {
                    self.go_blind();
                    out.extend_from_slice(input);
                    return input.len();
                }
            }
        }

        while off < input.len() {
            let avail = &input[off..];
            // A zero length field can only be a BIG-REQUESTS length, so
            // `Enabled` is correct without tracking the extension handshake.
            let (header, body) = match parse_request_header(avail, BigRequests::Enabled) {
                Ok(v) => v,
                Err(ParseError::InsufficientData) => break,
                Err(_) => {
                    self.go_blind();
                    out.extend_from_slice(avail);
                    return input.len();
                }
            };
            let header_len = avail.len() - body.len();
            let Some(total) = (header.remaining_length as usize)
                .checked_mul(4)
                .map(|b| b + header_len)
                .filter(|&t| t <= MAX_REQUEST_BYTES)
            else {
                self.go_blind();
                out.extend_from_slice(avail);
                return input.len();
            };
            if avail.len() < total {
                break;
            }
            let raw = &avail[..total];
            let body = &raw[header_len..];
            match header.major_opcode {
                CREATE_WINDOW_REQUEST => self.emit_create_window(header, body, raw, out),
                SET_INPUT_FOCUS_REQUEST => emit_noop(raw, out),
                CONFIGURE_WINDOW_REQUEST if self.targets_owned(header, body) => {
                    emit_noop(raw, out);
                }
                CIRCULATE_WINDOW_REQUEST => emit_noop(raw, out),
                CHANGE_PROPERTY_REQUEST if self.is_wm_state_property(header, body) => {
                    emit_noop(raw, out);
                }
                SEND_EVENT_REQUEST if self.is_wm_state_message(header, body) => {
                    emit_noop(raw, out);
                }
                CHANGE_WINDOW_ATTRIBUTES_REQUEST => {
                    self.emit_change_attributes(header, body, raw, out);
                }
                op if self.is_xfixes_cursor(op, header.minor_opcode) => emit_noop(raw, out),
                _ => out.extend_from_slice(raw),
            }
            off += total;
        }
        off
    }

    fn emit_create_window(
        &mut self,
        header: RequestHeader,
        body: &[u8],
        raw: &[u8],
        out: &mut Vec<u8>,
    ) {
        let Ok(mut req) = CreateWindowRequest::try_parse_request(header, body) else {
            out.extend_from_slice(raw);
            return;
        };
        if Some(req.parent) == self.video_host {
            // mpv's embed sub-window, captured only to key this parser's
            // rewrites. The lock-free store needs no synchronization: mpv sends
            // this CreateWindow before any request naming the wid, so the
            // capture always precedes the requests it neutralizes.
            self.embed.embed_window.store(req.wid, Ordering::Relaxed);
            out.extend_from_slice(raw);
            return;
        }
        if !self.roots.contains(&req.parent) {
            out.extend_from_slice(raw);
            return;
        }
        self.toplevels.insert(req.wid);
        req.value_list = Cow::Owned(req.value_list.into_owned().override_redirect(1));
        let (bufs, _) = req.serialize();
        for buf in &bufs {
            out.extend_from_slice(buf);
        }
    }

    /// The app owns the cursor: strip the `cursor` value so the video area
    /// inherits the host cursor (a cursor attribute applies whenever the
    /// pointer is over the window, regardless of event selection).
    fn emit_change_attributes(
        &self,
        header: RequestHeader,
        body: &[u8],
        raw: &[u8],
        out: &mut Vec<u8>,
    ) {
        let Ok(mut req) = ChangeWindowAttributesRequest::try_parse_request(header, body) else {
            out.extend_from_slice(raw);
            return;
        };
        if req.value_list.cursor.is_none() {
            out.extend_from_slice(raw);
            return;
        }
        let mut aux = req.value_list.into_owned();
        aux.cursor = None;
        req.value_list = Cow::Owned(aux);
        let (bufs, _) = req.serialize();
        for buf in &bufs {
            out.extend_from_slice(buf);
        }
    }

    fn targets_owned(&self, header: RequestHeader, body: &[u8]) -> bool {
        ConfigureWindowRequest::try_parse_request(header, body).is_ok_and(|req| {
            let embed = self.embed.embed_window.load(Ordering::Relaxed);
            (embed != 0 && req.window == embed) || self.toplevels.contains(&req.window)
        })
    }

    fn is_wm_state_property(&self, header: RequestHeader, body: &[u8]) -> bool {
        let Some(net_wm_state) = self.net_wm_state else {
            return false;
        };
        ChangePropertyRequest::try_parse_request(header, body)
            .is_ok_and(|req| req.property == net_wm_state && self.toplevels.contains(&req.window))
    }

    fn is_wm_state_message(&self, header: RequestHeader, body: &[u8]) -> bool {
        let Some(net_wm_state) = self.net_wm_state else {
            return false;
        };
        SendEventRequest::try_parse_request(header, body).is_ok_and(|req| {
            req.event[0] & 0x7f == CLIENT_MESSAGE_EVENT
                && req.event[8..12] == net_wm_state.to_ne_bytes()
        })
    }

    fn is_xfixes_cursor(&self, major: u8, minor: u8) -> bool {
        self.xfixes_opcode == Some(major)
            && matches!(minor, HIDE_CURSOR_REQUEST | SHOW_CURSOR_REQUEST)
    }
}

/// Server→client framer: walks the setup reply, then reply/error/event units,
/// dropping a video-host `ConfigureNotify` whose size differs from the
/// app-published geometry. Every reconcile publishes and then resizes the
/// host, so a matching notify always follows a dropped stale one; mpv reads
/// live parent geometry on notify (not the event fields), so intermediates
/// carry nothing it needs.
struct EventFramer<'a> {
    setup_done: bool,
    embed: &'a SharedEmbed,
    blind: Arc<AtomicBool>,
}

impl<'a> EventFramer<'a> {
    fn new(embed: &'a SharedEmbed, blind: Arc<AtomicBool>) -> Self {
        Self {
            setup_done: false,
            embed,
            blind,
        }
    }

    fn go_blind(&self) {
        self.blind.store(true, Ordering::Relaxed);
    }

    /// Append the leading complete units of `input` to `out` (minus dropped
    /// events) and return how many `input` bytes were consumed. A trailing
    /// partial unit is left for the next call.
    fn process(&mut self, input: &[u8], out: &mut Vec<u8>) -> usize {
        if self.blind.load(Ordering::Relaxed) {
            out.extend_from_slice(input);
            return input.len();
        }

        let mut off = 0;
        if !self.setup_done {
            if input.len() < 8 {
                return 0;
            }
            // Setup replies (failed=0, success=1, authenticate=2) all carry an
            // additional-data length in 4-byte units at offset 6.
            if input[0] > 2 {
                self.go_blind();
                out.extend_from_slice(input);
                return input.len();
            }
            let words = u16::from_ne_bytes([input[6], input[7]]) as usize;
            let total = 8 + 4 * words;
            if input.len() < total {
                return 0;
            }
            out.extend_from_slice(&input[..total]);
            off = total;
            self.setup_done = true;
        }

        while off < input.len() {
            let avail = &input[off..];
            if avail.len() < 32 {
                break;
            }
            let code = avail[0] & 0x7f;
            // Replies (byte 0 == 1) and GenericEvents carry extra length in
            // 4-byte units at offset 4; errors and core events are 32 bytes.
            let total = if avail[0] == 1 || code == GE_GENERIC_EVENT {
                let words = u32::from_ne_bytes([avail[4], avail[5], avail[6], avail[7]]) as usize;
                let Some(total) = words
                    .checked_mul(4)
                    .map(|b| b + 32)
                    .filter(|&t| t <= MAX_REQUEST_BYTES)
                else {
                    self.go_blind();
                    out.extend_from_slice(avail);
                    return input.len();
                };
                total
            } else {
                32
            };
            if avail.len() < total {
                break;
            }
            let stale = avail[0] != 1
                && code == CONFIGURE_NOTIFY_EVENT
                && self.stale_configure(&avail[..32]);
            if !stale {
                out.extend_from_slice(&avail[..total]);
            }
            off += total;
        }
        off
    }

    fn stale_configure(&self, unit: &[u8]) -> bool {
        let guard = self.embed.ctx.lock();
        let Some(ctx) = guard.as_ref() else {
            return false;
        };
        ConfigureNotifyEvent::try_parse(unit)
            .is_ok_and(|(e, _)| e.window == ctx.video_host && (e.width, e.height) != ctx.published)
    }
}

fn recv_with_fds(fd: RawFd, buf: &mut [u8], cmsg: &mut [u8]) -> io::Result<(usize, Vec<OwnedFd>)> {
    let mut iov = [IoSliceMut::new(buf)];
    let msg = loop {
        match recvmsg::<()>(fd, &mut iov, Some(&mut *cmsg), MsgFlags::empty()) {
            Err(Errno::EINTR) => continue,
            Err(e) => return Err(e.into()),
            Ok(m) => break m,
        }
    };
    let mut fds = Vec::new();
    for c in msg.cmsgs()? {
        if let ControlMessageOwned::ScmRights(list) = c {
            fds.extend(list.into_iter().map(|f| unsafe { OwnedFd::from_raw_fd(f) }));
        }
    }
    Ok((msg.bytes, fds))
}

/// The fds attach to `buf`'s first byte: delivered whole on the first
/// `sendmsg`, so a short send finishes with plain `send`s.
fn send_with_fds(fd: RawFd, buf: &[u8], fds: Vec<OwnedFd>) -> io::Result<()> {
    if buf.is_empty() {
        return Ok(());
    }

    let raw: Vec<RawFd> = fds.iter().map(AsRawFd::as_raw_fd).collect();
    let iov = [IoSlice::new(buf)];
    let scm = [ControlMessage::ScmRights(&raw)];
    let cmsgs = if raw.is_empty() { &[][..] } else { &scm[..] };
    let first = loop {
        match sendmsg::<()>(fd, &iov, cmsgs, MsgFlags::MSG_NOSIGNAL, None) {
            Err(Errno::EINTR) => continue,
            Err(e) => return Err(e.into()),
            Ok(n) => break n,
        }
    };
    drop(fds);

    let mut off = first;
    while off < buf.len() {
        match send(fd, &buf[off..], MsgFlags::MSG_NOSIGNAL) {
            Err(Errno::EINTR) => continue,
            Err(e) => return Err(e.into()),
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => off += n,
        }
    }
    Ok(())
}

/// Provision an auth cookie for the proxy's display number so mpv's connection
/// is accepted. Reads the real display's cookie via x11rb and writes a single
/// re-keyed entry into a private, jellium-owned runtime dir. Returns `None` when
/// the server has no cookie (e.g. an unauthenticated `xhost +local:` session).
fn provision_auth(display: u16, proxy_number: u32) -> io::Result<Option<PathBuf>> {
    let host = gethostname::gethostname().into_encoded_bytes();
    let (name, data) = match get_auth(Family::LOCAL, &host, display) {
        Ok(Some(auth)) => auth,
        Ok(None) => return Ok(None),
        Err(e) => {
            tracing::debug!(target: "x11-proxy", "xauth lookup failed: {e}");
            return Ok(None);
        }
    };

    // mpv reaches the proxy over a local socket, so the entry it looks up is
    // keyed by FamilyLocal + this host, under the proxy's display number. A
    // second entry re-keys the same cookie under the real display number so
    // app connections made while `XAUTHORITY` is repointed (via
    // [`real_display`]) still authenticate against the real server.
    let mut out = Vec::new();
    write_xauth_entry(
        &mut out,
        FAMILY_LOCAL,
        &host,
        proxy_number.to_string().as_bytes(),
        &name,
        &data,
    );
    write_xauth_entry(
        &mut out,
        FAMILY_LOCAL,
        &host,
        display.to_string().as_bytes(),
        &name,
        &data,
    );

    let dir = jfn_paths::runtime_dir()?;
    let mut file = tempfile::Builder::new()
        .prefix("xauth-")
        .tempfile_in(&dir)?;
    file.write_all(&out)?;
    file.flush()?;
    let (_file, path) = file.keep().map_err(|e| e.error)?;
    Ok(Some(path))
}

fn write_xauth_entry(
    out: &mut Vec<u8>,
    family: u16,
    address: &[u8],
    number: &[u8],
    name: &[u8],
    data: &[u8],
) {
    fn block(out: &mut Vec<u8>, b: &[u8]) {
        out.extend_from_slice(&(b.len() as u16).to_be_bytes());
        out.extend_from_slice(b);
    }
    out.extend_from_slice(&family.to_be_bytes());
    block(out, address);
    block(out, number);
    block(out, name);
    block(out, data);
}

#[cfg(test)]
mod tests {
    use super::*;
    use x11rb::reexports::x11rb_protocol::protocol::xproto::{
        ChangeWindowAttributesAux, Circulate, CirculateWindowRequest, ConfigureWindowAux,
        CreateWindowAux, EventMask, PropMode, WindowClass,
    };
    use x11rb::reexports::x11rb_protocol::x11_utils::Serialize;

    const ROOT: u32 = 1;
    const VIDEO_HOST: u32 = 100;
    const NET_WM_STATE: u32 = 555;

    fn shared(xfixes: Option<u8>) -> SharedEmbed {
        let s = SharedEmbed::new();
        *s.ctx.lock() = Some(EmbedContext {
            video_host: VIDEO_HOST,
            net_wm_state: NET_WM_STATE,
            xfixes_opcode: xfixes,
            published: (800, 600),
        });
        s
    }

    fn parser(embed: &SharedEmbed) -> ReqParser<'_> {
        ReqParser::new(
            Arc::from([ROOT].as_slice()),
            embed,
            Arc::new(AtomicBool::new(false)),
        )
    }

    fn setup_bytes() -> Vec<u8> {
        SetupRequest {
            byte_order: if cfg!(target_endian = "little") {
                b'l'
            } else {
                b'B'
            },
            protocol_major_version: 11,
            protocol_minor_version: 0,
            authorization_protocol_name: Vec::new(),
            authorization_protocol_data: Vec::new(),
        }
        .serialize()
    }

    /// Feed a whole buffer and assert it is fully consumed.
    fn feed(parser: &mut ReqParser<'_>, bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        assert_eq!(parser.process(bytes, &mut out), bytes.len());
        out
    }

    fn started(embed: &SharedEmbed) -> ReqParser<'_> {
        let mut p = parser(embed);
        let setup = setup_bytes();
        assert_eq!(feed(&mut p, &setup), setup);
        p
    }

    fn create_window(parent: u32, wid: u32) -> Vec<u8> {
        let (bufs, _) = CreateWindowRequest {
            depth: 0,
            wid,
            parent,
            x: 0,
            y: 0,
            width: 10,
            height: 10,
            border_width: 0,
            class: WindowClass::INPUT_OUTPUT,
            visual: 0,
            value_list: Cow::Owned(CreateWindowAux::new()),
        }
        .serialize();
        bufs.concat()
    }

    fn configure_window(window: u32) -> Vec<u8> {
        let (bufs, _) = ConfigureWindowRequest {
            window,
            value_list: Cow::Owned(ConfigureWindowAux::new().width(64)),
        }
        .serialize();
        bufs.concat()
    }

    fn change_property(window: u32, property: u32) -> Vec<u8> {
        let (bufs, _) = ChangePropertyRequest {
            mode: PropMode::REPLACE,
            window,
            property,
            type_: 4,
            format: 32,
            data_len: 1,
            data: Cow::Owned(vec![0u8; 4]),
        }
        .serialize();
        bufs.concat()
    }

    fn send_event(msg_type: u32) -> Vec<u8> {
        let mut event = [0u8; 32];
        event[0] = CLIENT_MESSAGE_EVENT;
        event[1] = 32;
        event[8..12].copy_from_slice(&msg_type.to_ne_bytes());
        let (bufs, _) = SendEventRequest {
            propagate: false,
            destination: ROOT,
            event_mask: EventMask::SUBSTRUCTURE_NOTIFY,
            event: Cow::Owned(event),
        }
        .serialize();
        bufs.concat()
    }

    fn change_attributes(window: u32, aux: ChangeWindowAttributesAux) -> Vec<u8> {
        let (bufs, _) = ChangeWindowAttributesRequest {
            window,
            value_list: Cow::Owned(aux),
        }
        .serialize();
        bufs.concat()
    }

    #[test]
    fn embed_subwindow_captured_and_configure_neutralized() {
        let embed = shared(None);
        let mut p = started(&embed);

        let cw = create_window(VIDEO_HOST, 200);
        assert_eq!(feed(&mut p, &cw), cw);
        assert_eq!(embed.embed_window.load(Ordering::Relaxed), 200);

        let cfg = configure_window(200);
        let out = feed(&mut p, &cfg);
        assert_eq!(out[0], NO_OPERATION_REQUEST);
        assert_eq!(out.len(), cfg.len());

        let other = configure_window(999);
        assert_eq!(feed(&mut p, &other), other);
    }

    #[test]
    fn toplevel_backstop_rewrites() {
        let embed = shared(None);
        let mut p = started(&embed);

        let cw = create_window(ROOT, 300);
        let out = feed(&mut p, &cw);
        let (header, body) = parse_request_header(&out, BigRequests::Enabled).unwrap();
        let req = CreateWindowRequest::try_parse_request(header, body).unwrap();
        assert_eq!(req.value_list.override_redirect, Some(1));

        let cfg = configure_window(300);
        assert_eq!(feed(&mut p, &cfg)[0], NO_OPERATION_REQUEST);

        let prop = change_property(300, NET_WM_STATE);
        assert_eq!(feed(&mut p, &prop)[0], NO_OPERATION_REQUEST);

        let benign = change_property(300, 42);
        assert_eq!(feed(&mut p, &benign), benign);

        let untracked = change_property(999, NET_WM_STATE);
        assert_eq!(feed(&mut p, &untracked), untracked);
    }

    #[test]
    fn wm_state_send_event_neutralized() {
        let embed = shared(None);
        let mut p = started(&embed);

        let ev = send_event(NET_WM_STATE);
        assert_eq!(feed(&mut p, &ev)[0], NO_OPERATION_REQUEST);

        let benign = send_event(42);
        assert_eq!(feed(&mut p, &benign), benign);
    }

    #[test]
    fn circulate_neutralized() {
        let embed = shared(None);
        let mut p = started(&embed);
        let (bufs, _) = CirculateWindowRequest {
            direction: Circulate::RAISE_LOWEST,
            window: 5,
        }
        .serialize();
        let raw = bufs.concat();
        assert_eq!(feed(&mut p, &raw)[0], NO_OPERATION_REQUEST);
    }

    #[test]
    fn cursor_value_stripped() {
        let embed = shared(None);
        let mut p = started(&embed);

        let raw = change_attributes(
            5,
            ChangeWindowAttributesAux::new()
                .event_mask(EventMask::STRUCTURE_NOTIFY)
                .cursor(77),
        );
        let out = feed(&mut p, &raw);
        let (header, body) = parse_request_header(&out, BigRequests::Enabled).unwrap();
        let req = ChangeWindowAttributesRequest::try_parse_request(header, body).unwrap();
        assert_eq!(req.value_list.cursor, None);
        assert_eq!(req.value_list.event_mask, Some(EventMask::STRUCTURE_NOTIFY));

        let no_cursor = change_attributes(5, ChangeWindowAttributesAux::new().background_pixel(0));
        assert_eq!(feed(&mut p, &no_cursor), no_cursor);
    }

    #[test]
    fn xfixes_cursor_neutralized() {
        let embed = shared(Some(140));
        let mut p = started(&embed);

        for minor in [HIDE_CURSOR_REQUEST, SHOW_CURSOR_REQUEST] {
            let mut raw = vec![140, minor];
            raw.extend_from_slice(&2u16.to_ne_bytes());
            raw.extend_from_slice(&5u32.to_ne_bytes());
            assert_eq!(feed(&mut p, &raw)[0], NO_OPERATION_REQUEST);
        }

        // Unknown opcode without a provisioned XFixes opcode stays verbatim.
        let embed = shared(None);
        let mut p = started(&embed);
        let mut raw = vec![140, HIDE_CURSOR_REQUEST];
        raw.extend_from_slice(&2u16.to_ne_bytes());
        raw.extend_from_slice(&5u32.to_ne_bytes());
        assert_eq!(feed(&mut p, &raw), raw);
    }

    #[test]
    fn split_request_held_until_complete() {
        let embed = shared(None);
        let mut p = started(&embed);
        let cfg = configure_window(999);
        let mut out = Vec::new();
        assert_eq!(p.process(&cfg[..5], &mut out), 0);
        assert!(out.is_empty());
        assert_eq!(feed(&mut p, &cfg), cfg);
    }

    fn setup_reply() -> Vec<u8> {
        let mut b = vec![1u8, 0];
        b.extend_from_slice(&11u16.to_ne_bytes());
        b.extend_from_slice(&0u16.to_ne_bytes());
        b.extend_from_slice(&2u16.to_ne_bytes());
        b.extend_from_slice(&[0u8; 8]);
        b
    }

    fn started_framer(embed: &SharedEmbed) -> EventFramer<'_> {
        let mut f = EventFramer::new(embed, Arc::new(AtomicBool::new(false)));
        let setup = setup_reply();
        let mut out = Vec::new();
        assert_eq!(f.process(&setup, &mut out), setup.len());
        assert_eq!(out, setup);
        f
    }

    fn configure_notify(window: u32, width: u16, height: u16) -> [u8; 32] {
        (&ConfigureNotifyEvent {
            response_type: CONFIGURE_NOTIFY_EVENT,
            sequence: 7,
            event: window,
            window,
            above_sibling: 0,
            x: 0,
            y: 0,
            width,
            height,
            border_width: 0,
            override_redirect: false,
        })
            .into()
    }

    #[test]
    fn framer_reply_and_generic_event_framing() {
        let embed = shared(None);
        let mut f = started_framer(&embed);

        let mut reply = vec![0u8; 36];
        reply[0] = 1;
        reply[4..8].copy_from_slice(&1u32.to_ne_bytes());
        let mut out = Vec::new();
        assert_eq!(f.process(&reply[..35], &mut out), 0);
        assert!(out.is_empty());
        assert_eq!(f.process(&reply, &mut out), reply.len());
        assert_eq!(out, reply);

        let mut generic = vec![0u8; 40];
        generic[0] = GE_GENERIC_EVENT;
        generic[4..8].copy_from_slice(&2u32.to_ne_bytes());
        let mut out = Vec::new();
        assert_eq!(f.process(&generic, &mut out), generic.len());
        assert_eq!(out, generic);
    }

    #[test]
    fn framer_drops_stale_host_configure_notify() {
        let embed = shared(None);
        let mut f = started_framer(&embed);

        let stale = configure_notify(VIDEO_HOST, 500, 400);
        let mut out = Vec::new();
        assert_eq!(f.process(&stale, &mut out), stale.len());
        assert!(out.is_empty());

        let settled = configure_notify(VIDEO_HOST, 800, 600);
        let mut out = Vec::new();
        assert_eq!(f.process(&settled, &mut out), settled.len());
        assert_eq!(out, settled);

        // Another window's notify is never the host's — always forwarded.
        let other = configure_notify(999, 500, 400);
        let mut out = Vec::new();
        assert_eq!(f.process(&other, &mut out), other.len());
        assert_eq!(out, other);
    }

    #[test]
    fn framer_without_context_forwards_everything() {
        let embed = SharedEmbed::new();
        let mut f = started_framer(&embed);
        let ev = configure_notify(VIDEO_HOST, 500, 400);
        let mut out = Vec::new();
        assert_eq!(f.process(&ev, &mut out), ev.len());
        assert_eq!(out, ev);
    }
}
