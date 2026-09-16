#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Navigation(u64);
impl Navigation {
    pub fn new(id: u64) -> Self {
        Self(id)
    }
}

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
pub type WebEventHandler = std::sync::Arc<dyn Fn(WebEvent) + Send + Sync>;
