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

pub fn subscribe_window_changed(f: fn()) -> WindowSubscription {
    WINDOW_SUBSCRIBERS.subscribe(f)
}

pub fn notify_window_changed() {
    WINDOW_SUBSCRIBERS.notify();
}
