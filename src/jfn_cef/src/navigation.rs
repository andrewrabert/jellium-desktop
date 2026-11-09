//! Navigation identities and results reported to the application.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Navigation(u64);
impl Navigation {
    /// The caller must use a fresh ID for each navigation in its session.
    pub fn new(id: u64) -> Self {
        Self(id)
    }
}

/// A navigation whose document has committed a frame to its surface.
/// Only the CEF paint path constructs this result.
#[derive(Clone, Copy, Debug)]
pub struct NavigationPresented {
    navigation: Navigation,
}
impl NavigationPresented {
    pub(crate) fn witnessed(navigation: Navigation, presented: jfn_gpu_paint::Presented) -> Self {
        let _consumed = presented;
        Self { navigation }
    }
    pub fn navigation(self) -> Navigation {
        self.navigation
    }
}

#[derive(Clone, Debug)]
pub enum WebEvent {
    ProbeFinished { cycle: u64, base: Option<String> },
    NavigationFailed(Navigation),
    FramePresented(NavigationPresented),
}
/// Called from CEF callbacks; handlers should enqueue work and return promptly.
pub type WebEventHandler = std::sync::Arc<dyn Fn(WebEvent) + Send + Sync>;
