use crate::subscriptions::Subscribers;
pub use crate::subscriptions::Subscription as WindowSubscription;
use std::sync::LazyLock;

use crate::geometry::{
    ExtentError, PointsToPixelsError, WindowBasis, WindowExtent, WindowPlacement, WindowPos,
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WindowState {
    pub extent: WindowExtent,
    pub position: Option<WindowPos>,
    pub maximized: bool,
    pub fullscreen: bool,
    pub web_top: std::ffi::c_int,
    pub basis: WindowBasis,
}

#[derive(Clone, Copy, Debug, PartialEq, thiserror::Error)]
pub enum WindowStateError {
    #[error("the window does not exist yet")]
    NoWindow,
    #[error("the window has stated no web offset yet")]
    WebOffsetUnstated,
    #[error(transparent)]
    Extent(#[from] ExtentError),
    #[error("the window DPI is zero")]
    ZeroDpi,
    #[error("the window's client rect could not be read")]
    ClientRect,
    #[error("the window rect could not be read")]
    WindowRect,
    #[error("the window's monitor info could not be read")]
    MonitorInfo,
    #[error("window position: {0}")]
    Position(#[from] PointsToPixelsError),
    #[error("the platform is gone")]
    NoPlatform,
}

pub trait WindowSource: Send + Sync {
    fn window_state(&self) -> Result<WindowState, WindowStateError>;

    fn window_placement(&self) -> Result<Option<WindowPlacement>, WindowStateError> {
        Ok(self
            .window_state()?
            .position
            .map(|position| WindowPlacement::new(None, position)))
    }
}

static WINDOW_SUBSCRIBERS: LazyLock<Subscribers> = LazyLock::new(Subscribers::new);

/// Registers a wakeup run inline on the publishing thread. Callbacks must post
/// their work without blocking the publisher. Retain the token while listening.
pub fn subscribe_window_changed(f: fn()) -> WindowSubscription {
    WINDOW_SUBSCRIBERS.subscribe(f)
}

pub fn notify_window_changed() {
    WINDOW_SUBSCRIBERS.notify();
}
