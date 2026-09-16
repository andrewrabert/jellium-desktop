use crate::WindowDecorations;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VoWait {
    Drain,
    Event,
}

pub trait MpvHost: Send + Sync {
    fn prepare(&self, _configured: Option<WindowDecorations>) {}

    fn host_ready(&self) -> bool {
        true
    }

    fn ensure_host_window(&self) {}

    fn embed_wid(&self) -> Option<i64> {
        None
    }

    fn run_vo_wait(&self, pump: &mut dyn FnMut(VoWait) -> std::ops::ControlFlow<()>) {
        loop {
            if let std::ops::ControlFlow::Break(()) = pump(VoWait::Event) {
                return;
            }
        }
    }

    fn logical_content_size(&self) -> Option<crate::geometry::LogicalSize>;

    fn detach(&self) {}
}

pub struct DefaultMpvHost;

impl MpvHost for DefaultMpvHost {
    fn logical_content_size(&self) -> Option<crate::geometry::LogicalSize> {
        None
    }
}
