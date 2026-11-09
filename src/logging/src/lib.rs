//! Logging backend.
//!
//! Two writers, both wrapped with `tracing_appender::non_blocking`:
//! - stderr (always)
//! - size-rotated file (optional, when `path` is non-empty)
//!
//! Every emitted line is filtered through the `redact` module so auth tokens
//! are 'x'-ed out. Anything other code writes to the real stderr (CEF
//! subprocesses, ffmpeg) is captured by a pipe-and-poll thread and
//! re-emitted as `[CEF]` debug records.

mod redact;

use parking_lot::Mutex;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use time::OffsetDateTime;
use time::format_description::FormatItem;
use time::macros::format_description;

use tracing_appender::non_blocking::{NonBlockingBuilder, WorkerGuard};
use tracing_subscriber::{EnvFilter, Registry, filter::LevelFilter, fmt, layer::SubscriberExt};

/// Subsystem a record is attributed to; each maps to one tracing target so
/// filter directives such as `CEF=off` or `mpv=trace` address it by name.
#[derive(Clone, Copy, Eq, PartialEq, Debug)]
pub enum Category {
    Main,
    Mpv,
    Cef,
    Media,
    Platform,
    Js,
    Resource,
}

#[derive(Clone, Copy, Eq, PartialEq, Debug)]
pub enum Level {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    fn label(self) -> &'static str {
        match self {
            Level::Trace => "TRACE",
            Level::Debug => "DEBUG",
            Level::Info => "INFO",
            Level::Warn => "WARN",
            Level::Error => "ERROR",
        }
    }
}

// =====================================================================
// Rotating file writer
// =====================================================================

const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;
const MAX_BACKUPS: usize = 3;

struct RotatingFile {
    path: PathBuf,
    file: File,
    max_bytes: u64,
    max_backups: usize,
    bytes_written: u64,
}

impl RotatingFile {
    fn open(path: PathBuf, max_bytes: u64, max_backups: usize) -> io::Result<Self> {
        // Start each run with a fresh file; prior run's contents shift into
        // the backup chain.
        shift_backups(&path, max_backups);
        let file = File::create(&path)?;
        Ok(Self {
            path,
            file,
            max_bytes,
            max_backups,
            bytes_written: 0,
        })
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.file.flush()?;
        shift_backups(&self.path, self.max_backups);
        self.file = File::create(&self.path)?;
        self.bytes_written = 0;
        Ok(())
    }
}

fn shift_backups(path: &Path, max_backups: usize) {
    let _ = std::fs::remove_file(backup_path(path, max_backups));
    for i in (1..max_backups).rev() {
        let _ = std::fs::rename(backup_path(path, i), backup_path(path, i + 1));
    }
    let _ = std::fs::rename(path, backup_path(path, 1));
}

fn backup_path(path: &Path, n: usize) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{n}"));
    PathBuf::from(name)
}

impl Write for RotatingFile {
    // Rotates before writing when the buffer would cross `max_bytes`, so a
    // record never spans two files.
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.bytes_written.saturating_add(buf.len() as u64) > self.max_bytes {
            self.rotate()?;
        }
        let n = self.file.write(buf)?;
        self.bytes_written += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

// =====================================================================
// Per-OS console writer + stderr capture
// =====================================================================

#[cfg(unix)]
mod imp {
    use super::{Category, Level, emit};
    use nix::errno::Errno;
    use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
    use nix::unistd::{dup, dup2_stderr, isatty, pipe, read, write};
    use std::io::{self, Write};
    use std::os::fd::{AsFd, OwnedFd};
    use std::sync::Arc;
    use std::thread::{self, JoinHandle};

    use jfn_wake_event::WakeEvent;

    // Holds a dup of the original stderr taken before StderrCapture's
    // dup2 redirect; writing via io::stderr() here would feed each log
    // line back into the capture pipe.
    pub(super) struct StderrWriter {
        fd: Option<OwnedFd>,
    }

    impl Write for StderrWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            match &self.fd {
                Some(fd) => Ok(write(fd, buf)?),
                None => Err(Errno::EBADF.into()),
            }
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    pub(super) fn make_console_writer() -> (StderrWriter, bool) {
        let fd = dup(io::stderr()).ok();
        let is_tty = fd.as_ref().is_some_and(|fd| isatty(fd).unwrap_or(false));
        (StderrWriter { fd }, is_tty)
    }

    pub(super) struct StderrCapture {
        original_fd: Option<OwnedFd>,
        wake: Arc<WakeEvent>,
        join: Option<JoinHandle<()>>,
    }

    impl StderrCapture {
        pub(super) fn start() -> Option<Self> {
            let original = dup(io::stderr()).ok()?;
            let (pipe_read, pipe_write) = pipe().ok()?;
            let wake = Arc::new(WakeEvent::new()?);

            dup2_stderr(&pipe_write).ok()?;
            drop(pipe_write);

            let thread_wake = Arc::clone(&wake);
            let join = thread::spawn(move || capture_loop(&pipe_read, &thread_wake));

            Some(StderrCapture {
                original_fd: Some(original),
                wake,
                join: Some(join),
            })
        }

        pub(super) fn stop(&mut self) {
            // Order: restore STDERR FIRST (so any concurrent writer drains to
            // the real fd from now on), THEN wake the capture thread, THEN
            // join. The wake outlives the join: it is dropped with the
            // StderrCapture, after the thread is gone.
            if let Some(original) = self.original_fd.take() {
                let _ = dup2_stderr(&original);
            }
            self.wake.signal();
            if let Some(h) = self.join.take()
                && let Err(e) = h.join()
            {
                eprintln!("[logging] stderr capture thread panicked: {e:?}");
            }
        }
    }

    fn capture_loop(pipe_read: &OwnedFd, wake: &WakeEvent) {
        let mut buf = [0u8; 4096];
        let mut partial = Vec::<u8>::new();
        loop {
            let mut pfds = [
                PollFd::new(pipe_read.as_fd(), PollFlags::POLLIN),
                PollFd::new(wake.as_fd(), PollFlags::POLLIN),
            ];
            if poll(&mut pfds, PollTimeout::NONE).is_err() {
                break;
            }
            let readable =
                |pfd: &PollFd| pfd.revents().is_some_and(|r| r.contains(PollFlags::POLLIN));
            if readable(&pfds[1]) {
                break;
            }
            if readable(&pfds[0]) {
                let Ok(n) = read(pipe_read, &mut buf) else {
                    break;
                };
                if n == 0 {
                    break;
                }
                partial.extend_from_slice(&buf[..n]);
                while let Some(pos) = partial.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = partial.drain(..=pos).take(pos).collect();
                    if !line.is_empty() {
                        let msg = String::from_utf8_lossy(&line).into_owned();
                        emit(Category::Cef, Level::Debug, &msg);
                    }
                }
            }
        }
    }
}

#[cfg(windows)]
mod imp {
    use std::io::{self, Write};

    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Console::{
        CONSOLE_MODE, ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, GetStdHandle,
        STD_ERROR_HANDLE, SetConsoleMode,
    };

    fn enable_vt_mode() {
        // Best-effort: tell conhost to honor ANSI SGR escapes on stderr.
        // Win10+ supports ENABLE_VIRTUAL_TERMINAL_PROCESSING; older builds
        // silently fail and we render with no color.
        unsafe {
            let h = GetStdHandle(STD_ERROR_HANDLE);
            if h.is_null() || h == INVALID_HANDLE_VALUE {
                return;
            }
            let mut mode: CONSOLE_MODE = 0;
            if GetConsoleMode(h, &mut mode) == 0 {
                return;
            }
            SetConsoleMode(h, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
        }
    }

    pub(super) struct StderrWriter;

    impl Write for StderrWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            io::stderr().write(buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            io::stderr().flush()
        }
    }

    pub(super) fn make_console_writer() -> (StderrWriter, bool) {
        use std::io::IsTerminal;
        let is_tty = io::stderr().is_terminal();
        if is_tty {
            enable_vt_mode();
        }
        (StderrWriter, is_tty)
    }

    pub(super) struct StderrCapture;

    impl StderrCapture {
        pub(super) fn start() -> Option<Self> {
            None
        }
        pub(super) fn stop(&mut self) {}
    }
}

// =====================================================================
// State
// =====================================================================

struct State {
    active_log_path: String,
    _console_guard: WorkerGuard,
    _file_guard: Option<WorkerGuard>,
    stderr_capture: Option<imp::StderrCapture>,
}

static STATE: OnceLock<Mutex<Option<State>>> = OnceLock::new();

fn state() -> &'static Mutex<Option<State>> {
    STATE.get_or_init(|| Mutex::new(None))
}

const ISO_FILE_FMT: &[FormatItem<'static>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]");
const CONSOLE_TRACE_FMT: &[FormatItem<'static>] =
    format_description!("[hour]:[minute]:[second].[subsecond digits:3]");

// =====================================================================
// Emit
// =====================================================================

// The one place a category maps to a target string; `$mac` is re-invoked
// with the target literal prepended to its arguments.
macro_rules! with_category_target {
    ($category:expr, $mac:ident $(, $arg:expr)*) => {
        match $category {
            Category::Main => $mac!("Main" $(, $arg)*),
            Category::Mpv => $mac!("mpv" $(, $arg)*),
            Category::Cef => $mac!("CEF" $(, $arg)*),
            Category::Media => $mac!("Media" $(, $arg)*),
            Category::Platform => $mac!("Platform" $(, $arg)*),
            Category::Js => $mac!("JS" $(, $arg)*),
            Category::Resource => $mac!("Resource" $(, $arg)*),
        }
    };
}

// `tracing::event!` requires a literal `target` (it builds a `static`
// Callsite at the call site), so the level match keeps `target` a literal
// and materializes one static callsite per (category, level).
macro_rules! emit_at {
    ($tgt:expr, $lvl:expr, $msg:expr) => {{
        use tracing::Level as L;
        match $lvl {
            Level::Trace => tracing::event!(target: $tgt, L::TRACE, "{}", $msg),
            Level::Debug => tracing::event!(target: $tgt, L::DEBUG, "{}", $msg),
            Level::Info => tracing::event!(target: $tgt, L::INFO, "{}", $msg),
            Level::Warn => tracing::event!(target: $tgt, L::WARN, "{}", $msg),
            Level::Error => tracing::event!(target: $tgt, L::ERROR, "{}", $msg),
        }
    }};
}

macro_rules! enabled_at {
    ($tgt:expr, $lvl:expr) => {{
        use tracing::Level as L;
        match $lvl {
            Level::Trace => tracing::event_enabled!(target: $tgt, L::TRACE),
            Level::Debug => tracing::event_enabled!(target: $tgt, L::DEBUG),
            Level::Info => tracing::event_enabled!(target: $tgt, L::INFO),
            Level::Warn => tracing::event_enabled!(target: $tgt, L::WARN),
            Level::Error => tracing::event_enabled!(target: $tgt, L::ERROR),
        }
    }};
}

fn emit(category: Category, level: Level, msg: &str) {
    let msg = msg.trim_end_matches(['\r', '\n']);
    with_category_target!(category, emit_at, level, msg);
}

/// True if the filter admits any TRACE-level callsite — used to decide
/// whether to prepend HH:MM:SS.mmm on console lines.
fn filter_is_trace(filter: &EnvFilter) -> bool {
    filter.max_level_hint() == Some(LevelFilter::TRACE)
}

pub fn jfn_log_init(path: &str, filter: &str) {
    let path_str = path.to_string();
    let filter_str_raw = filter.to_string();
    let filter_str = if filter_str_raw.trim().is_empty() {
        "info".to_string()
    } else {
        filter_str_raw
    };

    // Bail early on second init: dispatcher is already installed and the
    // capture pipe / guards live in STATE. Mirrors prior behavior.
    {
        let guard = state().lock();
        if guard.is_some() {
            return;
        }
    }

    // Capture a dup of stderr now so console writes survive the later
    // dup2() redirect installed by StderrCapture, and aren't fed back into
    // the capture pipe.
    let (console_writer, is_tty) = imp::make_console_writer();
    let color = is_tty && std::env::var_os("NO_COLOR").is_none();
    let (console_nb, console_guard) = NonBlockingBuilder::default()
        .lossy(true)
        .finish(console_writer);

    let (file_nb, file_guard) = if !path_str.is_empty() {
        match RotatingFile::open(PathBuf::from(&path_str), MAX_FILE_BYTES, MAX_BACKUPS) {
            Ok(rf) => {
                let (nb, g) = NonBlockingBuilder::default().lossy(true).finish(rf);
                (Some(nb), Some(g))
            }
            Err(_) => (None, None),
        }
    } else {
        (None, None)
    };

    let env_filter = match EnvFilter::try_new(&filter_str) {
        Ok(f) => f,
        Err(_) => EnvFilter::new("info"),
    };

    let trace_mode = filter_is_trace(&env_filter);

    let console_layer = fmt::layer()
        .event_format(ConsoleFormat { trace_mode, color })
        .with_writer(RedactMake(console_nb));

    let subscriber = Registry::default().with(env_filter).with(console_layer);

    // Add file layer conditionally without changing the subscriber type
    // for the install call. SubscriberExt::with returns a new type each
    // time, so we use boxed dispatch.
    let dispatch: tracing::Dispatch = if let Some(file_nb) = file_nb {
        let file_layer = fmt::layer()
            .event_format(FileFormat)
            .with_writer(RedactMake(file_nb));
        subscriber.with(file_layer).into()
    } else {
        subscriber.into()
    };

    // Fail-soft: if a dispatcher was already installed (unlikely given
    // the STATE.is_some() early-return above, but possible across crate
    // boundaries in tests), proceed without panicking.
    let _ = tracing::dispatcher::set_global_default(dispatch);

    let stderr_capture = imp::StderrCapture::start();

    let mut guard = state().lock();
    *guard = Some(State {
        active_log_path: path_str,
        _console_guard: console_guard,
        _file_guard: file_guard,
        stderr_capture,
    });
}

pub fn jfn_log_shutdown() {
    let mut guard = state().lock();
    if let Some(mut s) = guard.take() {
        if let Some(mut cap) = s.stderr_capture.take() {
            cap.stop();
        }
        // Drop file guard first so the file worker flushes before the
        // console worker; final console line therefore appears after
        // file flush completes on exit.
        s._file_guard = None;
        drop(s);
    }
}

pub fn log_enabled(category: Category, level: Level) -> bool {
    with_category_target!(category, enabled_at, level)
}

pub fn log(category: Category, level: Level, msg: &str) {
    emit(category, level, msg);
}

pub fn active_path() -> String {
    let guard = state().lock();
    guard
        .as_ref()
        .map(|st| st.active_log_path.clone())
        .unwrap_or_default()
}

// =====================================================================
// tracing-subscriber FormatEvent impls (commit 2 wires these in)
// =====================================================================

use tracing::{Event, Subscriber, field::Field};
use tracing_subscriber::{
    fmt::{FmtContext, FormatEvent, FormatFields, format::Writer},
    registry::LookupSpan,
};

/// Records only the `"message"` field; ignores any structured fields.
#[derive(Default)]
struct MsgVisitor(String);

impl tracing::field::Visit for MsgVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            use std::fmt::Write;
            let _ = write!(self.0, "{value:?}");
        }
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.0.push_str(value);
        }
    }
}

struct ConsoleFormat {
    trace_mode: bool,
    color: bool,
}

fn level_to_local_level(l: &tracing::Level) -> Level {
    match *l {
        tracing::Level::TRACE => Level::Trace,
        tracing::Level::DEBUG => Level::Debug,
        tracing::Level::INFO => Level::Info,
        tracing::Level::WARN => Level::Warn,
        tracing::Level::ERROR => Level::Error,
    }
}

fn ansi_for(level: &tracing::Level) -> (&'static str, &'static str) {
    match *level {
        tracing::Level::ERROR => ("\x1b[31m", "\x1b[0m"),
        tracing::Level::WARN => ("\x1b[33m", "\x1b[0m"),
        tracing::Level::INFO => ("\x1b[32m", "\x1b[0m"),
        tracing::Level::DEBUG => ("\x1b[36m", "\x1b[0m"),
        tracing::Level::TRACE => ("\x1b[2m", "\x1b[0m"),
    }
}

impl<S, N> FormatEvent<S, N> for ConsoleFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        _ctx: &FmtContext<'_, S, N>,
        mut w: Writer<'_>,
        event: &Event<'_>,
    ) -> std::fmt::Result {
        if self.trace_mode
            && let Ok(now) = OffsetDateTime::now_local()
            && let Ok(s) = now.format(&CONSOLE_TRACE_FMT)
        {
            write!(w, "{s} ")?;
        }
        let meta = event.metadata();
        let target = meta.target();
        if self.color {
            let (open, close) = ansi_for(meta.level());
            write!(w, "{open}[{target}]{close} ")?;
        } else {
            write!(w, "[{target}] ")?;
        }
        let mut v = MsgVisitor::default();
        event.record(&mut v);
        writeln!(w, "{}", v.0)
    }
}

struct FileFormat;

impl<S, N> FormatEvent<S, N> for FileFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        _ctx: &FmtContext<'_, S, N>,
        mut w: Writer<'_>,
        event: &Event<'_>,
    ) -> std::fmt::Result {
        if let Ok(now) = OffsetDateTime::now_local()
            && let Ok(s) = now.format(&ISO_FILE_FMT)
        {
            write!(w, "{s} ")?;
        }
        let meta = event.metadata();
        let label = level_to_local_level(meta.level()).label();
        write!(w, "{label}")?;
        for _ in label.len()..7 {
            w.write_char(' ')?;
        }
        write!(w, " [{}] ", meta.target())?;
        let mut v = MsgVisitor::default();
        event.record(&mut v);
        writeln!(w, "{}", v.0)
    }
}

// =====================================================================
// Redacting MakeWriter — runs `jfn_log_redact::censor` on each event's
// bytes before they reach the underlying non-blocking writer.
// =====================================================================

use tracing_subscriber::fmt::MakeWriter;

struct RedactMake<W>(W);

struct RedactGuard<W: Write> {
    inner: W,
    buf: Vec<u8>,
}

impl<W: Write> Write for RedactGuard<W> {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<W: Write> Drop for RedactGuard<W> {
    fn drop(&mut self) {
        if redact::contains_secret(&self.buf) {
            redact::censor(&mut self.buf);
        }
        let _ = self.inner.write_all(&self.buf);
    }
}

impl<'a, W> MakeWriter<'a> for RedactMake<W>
where
    W: Write + Clone + 'a,
{
    type Writer = RedactGuard<W>;
    fn make_writer(&'a self) -> Self::Writer {
        RedactGuard {
            inner: self.0.clone(),
            buf: Vec::new(),
        }
    }
}

// =====================================================================
// Tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex as StdMutex;
    use std::sync::Arc;

    #[derive(Clone)]
    struct VecSink(Arc<StdMutex<Vec<u8>>>);
    impl Write for VecSink {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.0.lock().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn redact_make_writer_censors_before_forwarding() -> io::Result<()> {
        let sink = VecSink(Arc::new(StdMutex::new(Vec::new())));
        let make = RedactMake(sink.clone());
        {
            let mut w = make.make_writer();
            w.write_all(b"GET /Items?api_key=abc123secret&x=1\n")?;
        }
        let bytes = sink.0.lock().clone();
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            !text.contains("abc123secret"),
            "secret leaked through redactor: {text}"
        );
        assert!(
            text.contains("api_key=xxx"),
            "expected censored bytes: {text}"
        );
        Ok(())
    }

    #[test]
    fn redact_make_writer_passes_clean_bytes() -> io::Result<()> {
        let sink = VecSink(Arc::new(StdMutex::new(Vec::new())));
        let make = RedactMake(sink.clone());
        {
            let mut w = make.make_writer();
            w.write_all(b"[mpv] hello\n")?;
        }
        assert_eq!(&*sink.0.lock(), b"[mpv] hello\n");
        Ok(())
    }

    #[test]
    fn level_label_padding_matches_file_format() {
        // FileFormat pads level label to 7 chars via a fill loop.
        // Sanity-check Level::label widths so padding code stays correct.
        assert_eq!(Level::Trace.label(), "TRACE");
        assert_eq!(Level::Debug.label(), "DEBUG");
        assert_eq!(Level::Info.label(), "INFO");
        assert_eq!(Level::Warn.label(), "WARN");
        assert_eq!(Level::Error.label(), "ERROR");
        for l in [
            Level::Trace,
            Level::Debug,
            Level::Info,
            Level::Warn,
            Level::Error,
        ] {
            assert!(l.label().len() <= 7);
        }
    }

    fn enabled_under(directive: &str, category: Category, level: Level) -> bool {
        let dispatch = tracing::Dispatch::new(Registry::default().with(EnvFilter::new(directive)));
        tracing::dispatcher::with_default(&dispatch, || log_enabled(category, level))
    }

    #[test]
    fn log_enabled_respects_global_filter() {
        // "warn" → Info disabled, Warn/Error enabled for any category.
        assert!(!enabled_under("warn", Category::Main, Level::Info));
        assert!(enabled_under("warn", Category::Main, Level::Warn));
        assert!(enabled_under("warn", Category::Main, Level::Error));
    }

    #[test]
    fn log_enabled_respects_target_override() {
        // Global warn, but mpv=trace → mpv Trace enabled, Main Trace not.
        assert!(enabled_under("warn,mpv=trace", Category::Mpv, Level::Trace));
        assert!(!enabled_under(
            "warn,mpv=trace",
            Category::Main,
            Level::Trace
        ));
    }

    #[test]
    fn log_enabled_respects_off_directive() {
        // Global info, CEF=off → CEF Error disabled, Main Error enabled.
        assert!(!enabled_under("info,CEF=off", Category::Cef, Level::Error));
        assert!(enabled_under("info,CEF=off", Category::Main, Level::Error));
    }

    #[test]
    fn filter_is_trace_detects_bare_and_target_directives() {
        for directive in [
            "trace",
            "info,mpv=trace",
            "debug,CEF=trace,mpv=warn",
            "mpv=5",
            "CEF[{field}]=trace",
        ] {
            assert!(
                filter_is_trace(&EnvFilter::new(directive)),
                "expected trace mode for {directive:?}"
            );
        }
        for directive in ["info", "debug,mpv=warn", "warn,CEF=off"] {
            assert!(
                !filter_is_trace(&EnvFilter::new(directive)),
                "unexpected trace mode for {directive:?}"
            );
        }
    }

    #[test]
    fn open_shifts_previous_run_into_backup_one() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("run.log");
        std::fs::write(&path, b"previous run\n")?;
        let mut rf = RotatingFile::open(path.clone(), 1024, 3)?;
        rf.write_all(b"current run\n")?;
        rf.flush()?;
        assert_eq!(std::fs::read(&path)?, b"current run\n");
        assert_eq!(std::fs::read(backup_path(&path, 1))?, b"previous run\n");
        Ok(())
    }

    #[test]
    fn write_past_limit_rotates_without_splitting_a_record() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("run.log");
        let mut rf = RotatingFile::open(path.clone(), 8, 3)?;
        rf.write_all(b"aaaaa\n")?;
        rf.write_all(b"bbbbb\n")?;
        rf.flush()?;
        assert_eq!(std::fs::read(&path)?, b"bbbbb\n");
        assert_eq!(std::fs::read(backup_path(&path, 1))?, b"aaaaa\n");
        Ok(())
    }

    #[test]
    fn backups_beyond_max_are_dropped() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("run.log");
        let mut rf = RotatingFile::open(path.clone(), 8, 2)?;
        for record in [&b"1111\n"[..], b"2222\n", b"3333\n", b"4444\n"] {
            rf.write_all(record)?;
        }
        rf.flush()?;
        assert_eq!(std::fs::read(&path)?, b"4444\n");
        assert_eq!(std::fs::read(backup_path(&path, 1))?, b"3333\n");
        assert_eq!(std::fs::read(backup_path(&path, 2))?, b"2222\n");
        assert!(!backup_path(&path, 3).exists());
        Ok(())
    }

    fn capture_console(color: bool) -> String {
        let sink = VecSink(Arc::new(StdMutex::new(Vec::new())));
        let layer = fmt::layer()
            .event_format(ConsoleFormat {
                trace_mode: false,
                color,
            })
            .with_writer({
                let sink = sink.clone();
                move || sink.clone()
            });
        let subscriber = Registry::default().with(layer);
        let dispatch = tracing::Dispatch::new(subscriber);
        tracing::dispatcher::with_default(&dispatch, || {
            tracing::event!(target: "mpv", tracing::Level::ERROR, "boom");
        });
        let bytes = sink.0.lock().clone();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    #[test]
    fn console_format_no_color_is_plain() {
        let s = capture_console(false);
        assert!(s.starts_with("[mpv] "), "unexpected line: {s:?}");
        assert!(!s.contains('\x1b'), "unexpected ANSI in: {s:?}");
        assert!(s.ends_with("boom\n"), "unexpected line: {s:?}");
    }

    #[test]
    fn console_format_color_wraps_target() {
        let s = capture_console(true);
        let (open, close) = ansi_for(&tracing::Level::ERROR);
        let expected_prefix = format!("{open}[mpv]{close} ");
        assert!(
            s.starts_with(&expected_prefix),
            "expected colored prefix in: {s:?}"
        );
    }

    #[test]
    fn ansi_for_each_level_distinct() {
        let palette = [
            tracing::Level::ERROR,
            tracing::Level::WARN,
            tracing::Level::INFO,
            tracing::Level::DEBUG,
            tracing::Level::TRACE,
        ];
        let mut seen = std::collections::HashSet::new();
        for l in palette {
            let (open, _) = ansi_for(&l);
            assert!(open.starts_with("\x1b["), "missing ANSI escape: {open:?}");
            assert!(seen.insert(open), "duplicate color for {l:?}");
        }
    }
}
