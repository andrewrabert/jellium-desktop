use crate::event::Event;
use crate::handle::Handle;
use crossbeam_channel::{Receiver, Sender, unbounded};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};

pub struct EventLoop {
    handle: Arc<Handle>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl EventLoop {
    pub fn spawn(handle: Arc<Handle>) -> std::io::Result<(Self, Receiver<Event>)> {
        let (tx, rx) = unbounded();
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let handle = Arc::clone(&handle);
            let stop = Arc::clone(&stop);
            thread::Builder::new()
                .name("jfn-mpv-events".into())
                .spawn(move || drain(handle, stop, tx))?
        };
        Ok((
            Self {
                handle,
                stop,
                thread: Some(thread),
            },
            rx,
        ))
    }

    pub fn stop(&mut self) {
        if self.thread.is_none() {
            return;
        }
        self.stop.store(true, Ordering::Release);
        self.handle.wakeup();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for EventLoop {
    fn drop(&mut self) {
        self.stop();
    }
}

fn drain(handle: Arc<Handle>, stop: Arc<AtomicBool>, tx: Sender<Event>) {
    loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        let event = handle.wait_event(-1.0);
        match event {
            Event::None => continue,
            Event::LogMessage(ref m) => crate::log::forward_to_tracing(m),
            Event::Shutdown => {
                let _ = tx.send(Event::Shutdown);
                return;
            }
            other => {
                if tx.send(other).is_err() {
                    return;
                }
            }
        }
    }
}
