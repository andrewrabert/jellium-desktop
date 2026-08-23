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
}

#[allow(clippy::expect_used)] // boot invariant: wake eventfd alloc is fatal if it fails
fn manager() -> &'static Manager {
    static MANAGER: OnceLock<&'static Manager> = OnceLock::new();
    MANAGER.get_or_init(|| {
        Box::leak(Box::new(Manager {
            queue: Mutex::new(VecDeque::new()),
            wake: WakeEvent::new().expect("manager WakeEvent allocation failed"),
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

#[allow(clippy::expect_used)] // boot invariant: control-plane thread spawn is fatal if it fails
pub fn jfn_manager_start(overlay: jfn_cef::WebOverlay) -> JoinHandle<Result<(), ManagerError>> {
    let _ = manager();
    jfn_playback::lifecycle::jfn_lifecycle_set_handlers(
        |visible| jfn_manager_send(ManagerMsg::SetVisible(visible)),
        || jfn_manager_send(ManagerMsg::Suspend),
        || jfn_manager_send(ManagerMsg::Resume),
    );
    thread::Builder::new()
        .name("jfn-manager".into())
        .spawn(move || {
            run_and_wake(
                || manager_loop(&overlay),
                || jfn_platform_abi::get().wake_main_loop(),
            )
        })
        .expect("spawn jfn-manager thread")
}

pub fn jfn_manager_notify_shutdown() {
    manager().wake.signal();
}

pub fn jfn_manager_send(msg: ManagerMsg) {
    manager().queue.lock().push_back(msg);
    manager().wake.signal();
}

fn manager_loop(overlay: &jfn_cef::WebOverlay) -> Result<(), ManagerError> {
    let manager = manager();
    let mut state = LifecycleState::Running;
    loop {
        manager.wake.wait();
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
