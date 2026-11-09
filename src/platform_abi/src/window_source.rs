//! Live window geometry, sourced from whichever component owns the window,
//! plus the payload-free change wakeup. Producers update their source and
//! call [`notify_window_changed`]; consumers subscribe and pull a
//! [`WindowSnapshot`].

use crate::subscriptions::Subscribers;
pub use crate::subscriptions::Subscription as WindowSubscription;
use std::sync::LazyLock;

use crate::geometry::{WindowExtent, WindowPos};

#[derive(Clone, Copy)]
pub struct WindowSnapshot {
    pub extent: Option<WindowExtent>,
    pub position: Option<WindowPos>,
    pub maximized: bool,
    pub fullscreen: bool,
}

pub trait WindowSource: Send + Sync {
    fn snapshot(&self) -> WindowSnapshot;
}

static WINDOW_SUBSCRIBERS: LazyLock<Subscribers> = LazyLock::new(Subscribers::new);

/// Registers a wakeup run inline on the publishing thread. Callbacks must post
/// their work without blocking the publisher. Retain the token while listening.
pub fn subscribe_window_changed(f: fn()) -> WindowSubscription {
    WINDOW_SUBSCRIBERS.subscribe(f)
}

/// Wake subscribers after committing the snapshot they will read.
pub fn notify_window_changed() {
    WINDOW_SUBSCRIBERS.notify();
}
