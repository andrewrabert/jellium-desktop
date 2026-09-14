//! Headless app control-plane thread.

use parking_lot::Mutex;
use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::OnceLock;
use std::thread::{self, JoinHandle};

use jfn_playback::shutdown::jfn_shutting_down;
use jfn_wake_event::WakeEvent;

pub enum ManagerMsg {
    SetVisible(bool),
    Suspend,
    Resume,
    Shutdown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LifecycleState {
    Running,
    Hidden,
    Suspended,
    ShuttingDown,
}

struct Manager {
    queue: Mutex<VecDeque<ManagerMsg>>,
    wake: WakeEvent,
    boot_wake: WakeEvent,
}

#[allow(clippy::expect_used)] // boot invariant: wake eventfd alloc is fatal if it fails
fn manager() -> &'static Manager {
    static MANAGER: OnceLock<&'static Manager> = OnceLock::new();
    MANAGER.get_or_init(|| {
        Box::leak(Box::new(Manager {
            queue: Mutex::new(VecDeque::new()),
            wake: WakeEvent::new().expect("manager WakeEvent allocation failed"),
            boot_wake: WakeEvent::new().expect("startup WakeEvent allocation failed"),
        }))
    })
}

#[derive(Debug, thiserror::Error)]
pub enum ManagerError {
    #[error(transparent)]
    CloseDelivery(#[from] jfn_cef::CloseDeliveryError),
    #[error("the shutdown manager thread panicked")]
    ThreadPanicked,
}

/// Prepare outside signal context, before producers or OS shutdown hooks exist.
pub fn prepare_shutdown() {
    let _ = manager();
    jfn_playback::jfn_shutdown_set_handler(Some(jfn_manager_notify_shutdown));
    jfn_playback::lifecycle::jfn_lifecycle_set_handlers(
        |visible| jfn_manager_send(ManagerMsg::SetVisible(visible)),
        || jfn_manager_send(ManagerMsg::Suspend),
        || jfn_manager_send(ManagerMsg::Resume),
    );
}

/// Acquires the worker before any browser exists. A failed thread spawn cannot
/// leave an overlay needing the main loop to drain.
pub struct PreparedManager {
    sender: Option<std::sync::mpsc::SyncSender<jfn_cef::WebOverlay>>,
    worker: Option<JoinHandle<Result<(), ManagerError>>>,
}
impl PreparedManager {
    #[allow(clippy::expect_used)] // Each field is consumed exactly once here or in Drop.
    pub fn activate(
        mut self,
        overlay: jfn_cef::WebOverlay,
    ) -> JoinHandle<Result<(), ManagerError>> {
        // The worker only waits for this value before entering application code.
        self.sender
            .take()
            .expect("unactivated manager")
            .send(overlay)
            .unwrap_or_else(|_| {
                unreachable!("prepared manager receiver cannot exit before activation")
            });
        self.worker.take().expect("unactivated manager")
    }
}
impl Drop for PreparedManager {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
pub fn jfn_manager_prepare() -> std::io::Result<PreparedManager> {
    let _ = manager();
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let worker = thread::Builder::new()
        .name("jfn-manager".into())
        .spawn(move || {
            let Ok(overlay) = receiver.recv() else {
                return Ok(());
            };
            run_and_wake(
                || manager_loop(&overlay),
                || {
                    if let Some(lease) = jfn_platform_abi::try_lease() {
                        lease.platform().wake_main_loop();
                    }
                },
            )
        })?;
    Ok(PreparedManager {
        sender: Some(sender),
        worker: Some(worker),
    })
}

pub fn jfn_manager_notify_shutdown() {
    manager().boot_wake.signal();
    manager().wake.signal();
}

/// Forwards the signal-safe wake into mpv while startup owns event ingestion.
/// The worker is joined before playback ingestion or native teardown can start.
pub struct BootShutdownWake {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl BootShutdownWake {
    pub fn start() -> std::io::Result<Self> {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopped = std::sync::Arc::clone(&stop);
        let manager = manager();
        let worker = thread::Builder::new()
            .name("jfn-startup-wake".into())
            .spawn(move || {
                loop {
                    manager.boot_wake.wait();
                    manager.boot_wake.drain();
                    if stopped.load(std::sync::atomic::Ordering::Acquire) {
                        break;
                    }
                    if jfn_shutting_down() {
                        jfn_mpv::api::jfn_mpv_wakeup();
                    }
                }
            })?;
        Ok(Self {
            stop,
            worker: Some(worker),
        })
    }
}
impl Drop for BootShutdownWake {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        manager().boot_wake.signal();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub fn jfn_manager_send(msg: ManagerMsg) {
    manager().queue.lock().push_back(msg);
    manager().wake.signal();
}

fn manager_loop(overlay: &jfn_cef::WebOverlay) -> Result<(), ManagerError> {
    let manager = manager();
    let mut state = LifecycleState::Running;
    loop {
        manager.wake.drain();

        let work: VecDeque<ManagerMsg> = {
            let mut queue = manager.queue.lock();
            if jfn_shutting_down() && state != LifecycleState::ShuttingDown {
                queue.push_back(ManagerMsg::Shutdown);
            }
            std::mem::take(&mut *queue)
        };
        for message in work {
            state = transition(overlay, state, message)?;
            if state == LifecycleState::ShuttingDown {
                return Ok(());
            }
        }
        manager.wake.wait();
    }
}

fn transition(
    overlay: &jfn_cef::WebOverlay,
    state: LifecycleState,
    message: ManagerMsg,
) -> Result<LifecycleState, ManagerError> {
    use LifecycleState::{Hidden, Running, ShuttingDown, Suspended};
    match (state, message) {
        (ShuttingDown, _) => Ok(ShuttingDown),
        (_, ManagerMsg::Shutdown) => {
            run_shutdown(overlay)?;
            Ok(ShuttingDown)
        }
        (Running, ManagerMsg::SetVisible(false)) => {
            overlay.set_hidden(true);
            Ok(Hidden)
        }
        (Hidden, ManagerMsg::SetVisible(true)) => {
            overlay.set_hidden(false);
            Ok(Running)
        }
        (Running | Hidden, ManagerMsg::Suspend) => {
            if state == Running {
                overlay.set_hidden(true);
            }
            Ok(Suspended)
        }
        (Suspended, ManagerMsg::Resume) => {
            overlay.set_hidden(false);
            Ok(Running)
        }
        _ => Ok(state),
    }
}

fn run_shutdown(overlay: &jfn_cef::WebOverlay) -> Result<(), ManagerError> {
    jfn_playback::shutdown::jfn_shutdown_fanout();
    overlay.close_blocking()?;
    Ok(())
}

fn run_and_wake<R, W>(run: R, wake: W) -> Result<(), ManagerError>
where
    R: FnOnce() -> Result<(), ManagerError>,
    W: FnOnce(),
{
    let result = catch_unwind(AssertUnwindSafe(run)).unwrap_or(Err(ManagerError::ThreadPanicked));
    wake();
    result
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    #[test]
    fn dropping_unactivated_manager_joins_without_entering_runtime() -> std::io::Result<()> {
        let manager = jfn_manager_prepare()?;
        // There is no installed platform or CEF session in this unit test.
        // Entering manager_loop or its wake callback would violate that setup.
        drop(manager);
        Ok(())
    }

    #[test]
    fn confirmed_close_wakes_main_and_returns_success() {
        let woken = AtomicBool::new(false);
        let result = run_and_wake(|| Ok(()), || woken.store(true, Ordering::Release));
        assert!(woken.load(Ordering::Acquire));
        assert!(result.is_ok());
    }

    #[test]
    fn post_rejection_wakes_main_and_returns_its_diagnostic() {
        let woken = AtomicBool::new(false);
        let result = run_and_wake(
            || Err(jfn_cef::CloseDeliveryError::PostRejected.into()),
            || woken.store(true, Ordering::Release),
        );
        assert!(woken.load(Ordering::Acquire));
        assert!(matches!(
            result,
            Err(ManagerError::CloseDelivery(
                jfn_cef::CloseDeliveryError::PostRejected
            ))
        ));
    }

    #[test]
    fn task_cancellation_wakes_main_and_returns_its_diagnostic() {
        let woken = AtomicBool::new(false);
        let result = run_and_wake(
            || Err(jfn_cef::CloseDeliveryError::TaskCanceled.into()),
            || woken.store(true, Ordering::Release),
        );
        assert!(woken.load(Ordering::Acquire));
        assert!(matches!(
            result,
            Err(ManagerError::CloseDelivery(
                jfn_cef::CloseDeliveryError::TaskCanceled
            ))
        ));
    }

    #[test]
    fn manager_unwind_wakes_main_and_returns_thread_panicked() {
        let woken = AtomicBool::new(false);
        let result = run_and_wake(
            || std::panic::resume_unwind(Box::new("manager unwind")),
            || woken.store(true, Ordering::Release),
        );
        assert!(woken.load(Ordering::Acquire));
        assert!(matches!(result, Err(ManagerError::ThreadPanicked)));
    }
}
