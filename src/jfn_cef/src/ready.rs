//! CEF UI readiness, including failure and cancellation at session teardown.
//! Callbacks always execute on TID_UI. Before readiness they are queued; after
//! stop they are dropped. No callback is invoked while holding the state lock.

use cef::rc::Rc;
use cef::{ImplTask, Task, ThreadId, WrapTask, post_task, wrap_task};
use parking_lot::Mutex;
use std::sync::Arc;

type Callback = Box<dyn FnOnce() + Send>;
enum Readiness {
    Pending(Vec<Callback>),
    Ready,
    Stopped,
}
static STATE: Mutex<Readiness> = Mutex::new(Readiness::Pending(Vec::new()));

#[derive(Debug, thiserror::Error)]
pub enum ReadinessError {
    #[error("CEF session stopped")]
    Stopped,
    #[error("CEF readiness dispatch rejected")]
    PostRejected,
}

impl Readiness {
    fn register(&mut self, callback: Callback) -> Result<Option<Callback>, ReadinessError> {
        match self {
            Self::Pending(waiting) => {
                waiting.push(callback);
                Ok(None)
            }
            Self::Ready => Ok(Some(callback)),
            Self::Stopped => Err(ReadinessError::Stopped),
        }
    }
    fn mark_ready(&mut self) -> Vec<Callback> {
        if matches!(self, Self::Stopped) {
            return Vec::new();
        }
        match std::mem::replace(self, Self::Ready) {
            Self::Pending(waiting) => waiting,
            _ => Vec::new(),
        }
    }
}

pub(crate) fn on_cef_ready(
    session: &Arc<crate::runtime::Session>,
    f: Callback,
) -> Result<(), ReadinessError> {
    let authority = Arc::clone(session);
    let f: Callback = Box::new(move || {
        authority.dispatch(f);
    });
    session
        .dispatch(|| {
            let mut state = STATE.lock();
            let Some(callback) = state.register(f)? else {
                return Ok(());
            };
            let mut task = ReadyCallbackTask::new(Arc::new(Mutex::new(Some(callback))));
            let accepted = post_task(ThreadId::UI, Some(&mut task)) == 1;
            // Callback destructors are user code too; release the lock before a rejected
            // task drops its callback. Posting stays serialized with stop().
            drop(state);
            if accepted {
                Ok(())
            } else {
                Err(ReadinessError::PostRejected)
            }
        })
        .unwrap_or(Err(ReadinessError::Stopped))
}

pub(crate) fn post_cef_ready() -> Result<(), ReadinessError> {
    let mut task = MarkReadyTask::new();
    if post_task(ThreadId::UI, Some(&mut task)) != 1 {
        stop();
        return Err(ReadinessError::PostRejected);
    }
    Ok(())
}

pub(crate) fn stop() {
    let old = std::mem::replace(&mut *STATE.lock(), Readiness::Stopped);
    drop(old);
}

wrap_task! {
    struct MarkReadyTask {}
    impl Task {
        fn execute(&self) {
            let waiting = STATE.lock().mark_ready();
            for callback in waiting {
                if matches!(*STATE.lock(), Readiness::Stopped) { break; }
                callback();
            }
        }
    }
}
wrap_task! {
    struct ReadyCallbackTask { callback: Arc<Mutex<Option<Callback>>> }
    impl Task {
        fn execute(&self) {
            if matches!(*STATE.lock(), Readiness::Stopped) { return; }
            if let Some(callback) = self.callback.lock().take() { callback(); }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stopped_readiness_drops_waiters_and_cannot_be_revived() {
        let payload = Arc::new(());
        let observer = Arc::downgrade(&payload);
        let mut state = Readiness::Pending(Vec::new());
        assert!(state.register(Box::new(move || drop(payload))).is_ok());
        let discarded = std::mem::replace(&mut state, Readiness::Stopped);
        drop(discarded);
        assert!(observer.upgrade().is_none());
        assert!(state.mark_ready().is_empty());
        assert!(state.register(Box::new(|| {})).is_err());
    }
    #[test]
    fn readiness_delivers_queued_callbacks_once() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = Arc::clone(&calls);
        let mut state = Readiness::Pending(Vec::new());
        assert!(
            state
                .register(Box::new(move || {
                    count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }))
                .is_ok()
        );
        for callback in state.mark_ready() {
            callback();
        }
        assert!(state.mark_ready().is_empty());
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    }
}
