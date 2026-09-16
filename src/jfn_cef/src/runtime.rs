use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::OnceLock;

use crate::version::{CefVersion, VersionError};

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("CEF must be loaded on the process main thread")]
    WrongThread,
    #[error("CEF bootstrap has already been claimed")]
    BootstrapAlreadyClaimed,
    #[error("cannot locate the executable: {0}")]
    Executable(#[source] std::io::Error),
    #[error("cannot resolve CEF framework {path}: {source}")]
    Path {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("CEF framework path contains a NUL byte: {0}")]
    InvalidPath(std::path::PathBuf),
    #[error("failed to load CEF framework {0}")]
    Framework(std::path::PathBuf),
    #[error("CEF version discovery: {0}")]
    Version(#[from] VersionError),
}

#[must_use]
pub struct LoadedCef {
    version: CefVersion,
    _thread: PhantomData<Rc<()>>,
}

pub(crate) struct LibraryLoaded {
    _private: (),
}

static CLAIMED: OnceLock<()> = OnceLock::new();

impl LoadedCef {
    pub fn load() -> Result<Self, LoadError> {
        #[cfg(target_os = "macos")]
        {
            let exe = std::env::current_exe().map_err(LoadError::Executable)?;
            let parent = exe
                .parent()
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "executable has no parent",
                    )
                })
                .map_err(LoadError::Executable)?;
            Self::load_framework(&parent.join(
                "../Frameworks/Chromium Embedded Framework.framework/Chromium Embedded Framework",
            ))
        }
        #[cfg(not(target_os = "macos"))]
        {
            CLAIMED
                .set(())
                .map_err(|()| LoadError::BootstrapAlreadyClaimed)?;
            Self::from_library(LibraryLoaded { _private: () })
        }
    }

    #[cfg(target_os = "macos")]
    pub fn load_framework(path: &std::path::Path) -> Result<Self, LoadError> {
        use std::os::unix::ffi::OsStrExt;
        unsafe extern "C" {
            fn pthread_main_np() -> std::ffi::c_int;
        }
        if unsafe { pthread_main_np() } == 0 {
            return Err(LoadError::WrongThread);
        }
        let resolved = path.canonicalize().map_err(|source| LoadError::Path {
            path: path.to_owned(),
            source,
        })?;
        let path = std::ffi::CString::new(resolved.as_os_str().as_bytes())
            .map_err(|_| LoadError::InvalidPath(resolved.clone()))?;
        CLAIMED
            .set(())
            .map_err(|()| LoadError::BootstrapAlreadyClaimed)?;
        if unsafe { cef::load_library(Some(&*path.as_ptr().cast())) } != 1 {
            return Err(LoadError::Framework(resolved));
        }
        Self::from_library(LibraryLoaded { _private: () })
    }

    fn from_library(library: LibraryLoaded) -> Result<Self, LoadError> {
        Ok(Self {
            version: crate::version::probe(&library)?,
            _thread: PhantomData,
        })
    }

    pub fn version(&self) -> &CefVersion {
        &self.version
    }
}

pub enum ProcessDispatch {
    Browser(BrowserCef),
    SubprocessExit(std::os::raw::c_int),
}

#[must_use]
pub struct BrowserCef {
    loaded: LoadedCef,
}

#[derive(Clone, Copy, Debug)]
pub enum LogSeverity {
    Verbose,
    Info,
    Warning,
    Error,
}
impl LogSeverity {
    pub(crate) fn native(self) -> cef::LogSeverity {
        match self {
            Self::Verbose => cef::LogSeverity::VERBOSE,
            Self::Info => cef::LogSeverity::INFO,
            Self::Warning => cef::LogSeverity::WARNING,
            Self::Error => cef::LogSeverity::ERROR,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DebuggingPort {
    #[default]
    Disabled,
    Enabled(DebugPort),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DebugPort(u16);

#[derive(Debug, thiserror::Error)]
#[error("remote debugging port must be 0 (disabled) or 1024..=65535")]
pub struct InvalidDebuggingPort;
impl TryFrom<u16> for DebuggingPort {
    type Error = InvalidDebuggingPort;
    fn try_from(port: u16) -> Result<Self, Self::Error> {
        match port {
            0 => Ok(Self::Disabled),
            1024..=65535 => Ok(Self::Enabled(DebugPort(port))),
            _ => Err(InvalidDebuggingPort),
        }
    }
}
impl std::str::FromStr for DebuggingPort {
    type Err = InvalidDebuggingPort;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value
            .parse::<u16>()
            .map_err(|_| InvalidDebuggingPort)?
            .try_into()
    }
}

impl DebuggingPort {
    pub(crate) fn native(self) -> i32 {
        match self {
            Self::Disabled => 0,
            Self::Enabled(port) => i32::from(port.0),
        }
    }
}

pub struct InitOptions {
    pub log_severity: LogSeverity,
    pub remote_debugging_port: DebuggingPort,
    pub disable_gpu_compositing: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum InitError {
    #[error("CEF initialization failed")]
    Native,
    #[error("CEF readiness task was rejected")]
    Readiness,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SessionState {
    Active { overlay: bool },
    Closing,
    Drained,
}
struct DispatchState {
    phase: SessionState,
    in_flight: usize,
}
pub(crate) struct Session {
    dispatch_state: parking_lot::Mutex<DispatchState>,
    idle: parking_lot::Condvar,
    producers: parking_lot::Mutex<Vec<crossbeam_channel::Receiver<std::convert::Infallible>>>,
}
struct DispatchPermit<'a>(&'a Session);
impl Drop for DispatchPermit<'_> {
    fn drop(&mut self) {
        let mut state = self.0.dispatch_state.lock();
        state.in_flight -= 1;
        if state.in_flight == 0 {
            self.0.idle.notify_all();
        }
    }
}
impl Session {
    pub(crate) fn new() -> Self {
        Self {
            producers: parking_lot::Mutex::new(Vec::new()),
            dispatch_state: parking_lot::Mutex::new(DispatchState {
                phase: SessionState::Active { overlay: false },
                in_flight: 0,
            }),
            idle: parking_lot::Condvar::new(),
        }
    }
    pub(crate) fn track_producer(
        &self,
        owner: crossbeam_channel::Receiver<std::convert::Infallible>,
    ) {
        let mut producers = self.producers.lock();
        producers.retain(|owner| {
            !matches!(
                owner.try_recv(),
                Err(crossbeam_channel::TryRecvError::Disconnected)
            )
        });
        producers.push(owner);
    }
    pub(crate) fn wait_for_producers(
        &self,
        deadline: std::time::Instant,
    ) -> Result<(), crate::CloseDeliveryError> {
        for owner in self.producers.lock().iter() {
            match owner.recv_deadline(deadline) {
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {}
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    return Err(crate::CloseDeliveryError::Timeout("probe owner release"));
                }
                Ok(never) => match never {},
            }
        }
        Ok(())
    }

    pub(crate) fn register_overlay(&self) -> bool {
        let mut state = self.dispatch_state.lock();
        if state.phase != (SessionState::Active { overlay: false }) {
            return false;
        }
        state.phase = SessionState::Active { overlay: true };
        true
    }
    fn acquire(&self) -> Option<DispatchPermit<'_>> {
        let mut state = self.dispatch_state.lock();
        if !matches!(state.phase, SessionState::Active { .. }) {
            return None;
        }
        state.in_flight += 1;
        Some(DispatchPermit(self))
    }
    pub(crate) fn dispatch<R>(&self, work: impl FnOnce() -> R) -> Option<R> {
        let _permit = self.acquire()?;
        Some(work())
    }
    pub(crate) fn drain<E>(&self, close: impl FnOnce() -> Result<(), E>) -> Result<(), E> {
        self.revoke();
        close()?;
        self.dispatch_state.lock().phase = SessionState::Drained;
        Ok(())
    }
    pub(crate) fn revoke(&self) {
        let mut state = self.dispatch_state.lock();
        state.phase = match state.phase {
            SessionState::Active { overlay: false } | SessionState::Drained => {
                SessionState::Drained
            }
            _ => SessionState::Closing,
        };
    }
    pub(crate) fn revoke_until(
        &self,
        deadline: std::time::Instant,
    ) -> Result<(), crate::CloseDeliveryError> {
        self.revoke();
        let mut state = self.dispatch_state.lock();
        while state.in_flight != 0 {
            if self.idle.wait_until(&mut state, deadline).timed_out() && state.in_flight != 0 {
                return Err(crate::CloseDeliveryError::Timeout("producer dispatch"));
            }
        }
        Ok(())
    }
    pub(crate) fn is_active(&self) -> bool {
        matches!(
            self.dispatch_state.lock().phase,
            SessionState::Active { .. }
        )
    }
    pub(crate) fn is_drained(&self) -> bool {
        matches!(
            self.dispatch_state.lock().phase,
            SessionState::Drained | SessionState::Active { overlay: false }
        )
    }
}

#[derive(Debug, thiserror::Error)]
#[error("CEF browser drain was not confirmed")]
pub struct ShutdownError;

#[must_use = "CEF requires explicit shutdown after confirmed browser drain"]
pub struct InitializedCef {
    _loaded: LoadedCef,
    platform: jfn_platform_abi::PlatformLease,
    shutdown_complete: bool,
    pub(crate) session: std::sync::Arc<Session>,
}

impl LoadedCef {
    pub fn dispatch(self) -> ProcessDispatch {
        let code = crate::ffi::jfn_cef_start(&self);
        if code >= 0 {
            ProcessDispatch::SubprocessExit(code)
        } else {
            ProcessDispatch::Browser(BrowserCef { loaded: self })
        }
    }
}

struct InitializationPin<T>(Option<T>);
impl<T> Drop for InitializationPin<T> {
    fn drop(&mut self) {
        if let Some(value) = self.0.take() {
            let _ = Box::leak(Box::new(value));
        }
    }
}

impl BrowserCef {
    pub fn version(&self) -> &CefVersion {
        self.loaded.version()
    }

    pub fn initialize(
        self,
        platform: &jfn_platform_abi::PlatformRuntime,
        options: InitOptions,
    ) -> Result<InitializedCef, InitError> {
        let mut pin = InitializationPin(Some(platform.lease()));
        let result = crate::ffi::jfn_cef_initialize(&self.loaded, platform.platform(), &options);
        let lease = pin.0.take().ok_or(InitError::Native)?;
        result?;
        Ok(InitializedCef {
            _loaded: self.loaded,
            platform: lease,
            shutdown_complete: false,
            session: std::sync::Arc::new(Session::new()),
        })
    }
}

impl InitializedCef {
    pub fn try_shutdown(&mut self) -> Result<(), ShutdownError> {
        if self.shutdown_complete {
            return Ok(());
        }
        self.session
            .revoke_until(std::time::Instant::now() + std::time::Duration::from_secs(10))
            .map_err(|_| ShutdownError)?;
        crate::ready::stop();
        if !self.session.is_drained() {
            return Err(ShutdownError);
        }
        crate::ffi::jfn_cef_shutdown(self);
        self.shutdown_complete = true;
        Ok(())
    }
    pub fn abandon(self) {
        drop(self);
    }

    pub(crate) fn platform_lease(&self) -> jfn_platform_abi::PlatformLease {
        self.platform.clone()
    }

    pub(crate) fn platform(&self) -> &dyn jfn_platform_abi::Platform {
        self.platform.platform()
    }
}
impl Drop for InitializedCef {
    fn drop(&mut self) {
        self.session.revoke();
        crate::ready::stop();
        if !self.shutdown_complete {
            let _ = Box::leak(Box::new(self.platform.clone()));
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::Session;
    use std::sync::{Arc, mpsc::channel};

    #[test]
    fn debugging_port_obeys_cef_settings_range() {
        use super::DebuggingPort;
        for invalid in ["-1", "1", "1023", "65536", "abc"] {
            assert!(invalid.parse::<DebuggingPort>().is_err());
        }
        for (input, native) in [("0", 0), ("1024", 1024), ("9222", 9222), ("65535", 65535)] {
            assert_eq!(input.parse::<DebuggingPort>().unwrap().native(), native);
        }
    }

    #[test]
    fn failed_close_revokes_dispatch_without_confirming_drain() {
        let session = Session::new();
        assert!(session.register_overlay());
        assert_eq!(
            session.drain(|| Err::<(), _>("close rejected")),
            Err("close rejected")
        );
        assert!(!session.is_active());
        assert!(session.dispatch(|| ()).is_none());
        assert!(!session.is_drained());
        assert_eq!(session.drain(|| Ok::<(), &str>(())), Ok(()));
        assert!(session.is_drained());
    }

    #[test]
    fn native_initialization_unwind_pins_its_dependency() {
        let dependency = Arc::new(());
        let retained = Arc::clone(&dependency);
        let result = std::panic::catch_unwind(move || {
            let _guard = super::InitializationPin(Some(retained));
            std::panic::resume_unwind(Box::new("injected native initialization panic"));
        });
        assert!(result.is_err());
        assert_eq!(Arc::strong_count(&dependency), 2);
    }

    #[test]
    fn reported_initialization_failure_releases_the_pin() {
        let dependency = Arc::new(());
        let mut guard = super::InitializationPin(Some(Arc::clone(&dependency)));
        drop(guard.0.take());
        drop(guard);
        assert_eq!(Arc::strong_count(&dependency), 1);
    }

    #[test]
    fn native_dispatch_allows_reentry_and_other_threads_without_holding_a_lock() {
        let session = Arc::new(Session::new());
        session.dispatch(|| {
            assert_eq!(session.dispatch(|| 7), Some(7));
            let other = Arc::clone(&session);
            assert_eq!(
                std::thread::spawn(move || other.dispatch(|| 9))
                    .join()
                    .unwrap(),
                Some(9)
            );
        });
        assert_eq!(session.dispatch_state.lock().in_flight, 0);
    }

    #[test]
    fn unwinding_native_dispatch_releases_its_permit() {
        let session = Session::new();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            session.dispatch(|| std::panic::resume_unwind(Box::new("injected")));
        }));
        assert!(result.is_err());
        assert_eq!(session.dispatch_state.lock().in_flight, 0);
        assert!(session.revoke_until(std::time::Instant::now()).is_ok());
    }

    #[test]
    fn blocked_native_dispatch_has_a_bounded_revocation_wait() {
        let session = Arc::new(Session::new());
        assert!(session.register_overlay());
        let gate = session.acquire().unwrap();
        let worker = Arc::clone(&session);
        let result = std::thread::spawn(move || worker.revoke_until(std::time::Instant::now()))
            .join()
            .unwrap();
        assert_eq!(
            result,
            Err(crate::CloseDeliveryError::Timeout("producer dispatch"))
        );
        assert!(!session.is_active());
        drop(gate);
        assert!(session.revoke_until(std::time::Instant::now()).is_ok());
    }

    #[test]
    fn producer_ownership_must_be_released_before_drain_confirmation() {
        let session = Session::new();
        let (owner, disconnected) = crossbeam_channel::unbounded();
        session.track_producer(disconnected);
        session.revoke();
        assert_eq!(
            session.wait_for_producers(std::time::Instant::now()),
            Err(crate::CloseDeliveryError::Timeout("probe owner release"))
        );
        drop(owner);
        assert!(
            session
                .wait_for_producers(std::time::Instant::now())
                .is_ok()
        );
    }

    #[test]
    fn registration_is_exclusive_and_cannot_restart_a_closed_session() {
        let session = Session::new();
        assert!(session.register_overlay());
        assert!(!session.register_overlay());
        session.revoke();
        assert!(!session.register_overlay());
        assert!(session.drain(|| Ok::<_, ()>(())).is_ok());
        assert!(session.is_drained());
        assert!(!session.register_overlay());
    }

    #[test]
    fn rollback_without_an_overlay_requires_no_browser_drain() {
        let session = Session::new();
        session.revoke();
        assert!(session.is_drained());
        let mut called = false;
        assert!(session.dispatch(|| called = true).is_none());
        assert!(!called);
    }

    #[test]
    fn revocation_serializes_with_an_inflight_producer() {
        let session = Arc::new(Session::new());
        let dispatch = session.acquire().unwrap();
        let worker_session = Arc::clone(&session);
        let (started_tx, started_rx) = channel();
        let (done_tx, done_rx) = channel();
        let worker = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            worker_session
                .revoke_until(std::time::Instant::now() + std::time::Duration::from_secs(1))
                .unwrap();
            done_tx.send(()).unwrap();
        });
        started_rx.recv().unwrap();
        assert!(done_rx.try_recv().is_err());
        drop(dispatch);
        done_rx.recv().unwrap();
        worker.join().unwrap();
        assert!(session.dispatch(|| ()).is_none());
    }
}
