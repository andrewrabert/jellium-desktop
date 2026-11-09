//! Blocking work keeps ownership until a worker has actually been acquired.

pub type Work = Box<dyn FnOnce() + Send>;

pub struct BlockingError {
    source: std::io::Error,
    work: Work,
}
impl BlockingError {
    pub fn into_work(self) -> Work {
        self.work
    }
    /// Preserve pending native work and everything it owns until process exit.
    pub fn abandon(self) {
        let _ = Box::leak(Box::new(self));
    }
}
impl std::fmt::Debug for BlockingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlockingError")
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Display for BlockingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "blocking worker spawn: {}", self.source)
    }
}
impl std::error::Error for BlockingError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// `spawn` acquires a worker for the supplied job. Rejection returns the exact
/// original work, including native owners captured by it, to the caller.
pub fn spawn_preserving(
    work: Work,
    spawn: impl FnOnce(Work) -> std::io::Result<std::thread::JoinHandle<()>>,
) -> Result<std::thread::JoinHandle<()>, BlockingError> {
    let slot = std::sync::Arc::new(parking_lot::Mutex::new(Some(work)));
    let worker_slot = slot.clone();
    match spawn(Box::new(move || {
        let work = worker_slot.lock().take();
        if let Some(work) = work {
            work();
        }
    })) {
        Ok(worker) => Ok(worker),
        Err(source) => {
            #[allow(clippy::expect_used)] // A rejected worker never executes its job.
            let work = slot
                .lock()
                .take()
                .expect("rejected worker did not acquire its work");
            Err(BlockingError { source, work })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejected_spawn_returns_exact_capture_without_running_or_dropping_it() {
        let owner = std::sync::Arc::new(());
        let observer = std::sync::Arc::downgrade(&owner);
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = calls.clone();
        let result = spawn_preserving(
            Box::new(move || {
                count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                drop(owner);
            }),
            |_| Err(std::io::Error::other("injected rejection")),
        );
        let Err(error) = result else { unreachable!() };
        assert!(observer.upgrade().is_some());
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 0);
        error.into_work()();
        assert!(observer.upgrade().is_none());
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    }
}
