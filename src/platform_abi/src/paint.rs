use std::sync::Arc;
use std::time::Instant;

use crate::{JfnRect, PhysicalSize};

pub use jfn_gpu_paint::Presented;

pub trait FrameSource: Send + Sync {
    fn request_frame(&self);
}

pub struct FrameRetry<F> {
    held: Option<Held<F>>,
}

struct Held<F> {
    frame: F,
    source: Arc<dyn FrameSource>,
    at: Instant,
}

impl<F> Default for FrameRetry<F> {
    fn default() -> Self {
        FrameRetry { held: None }
    }
}

impl<F> FrameRetry<F> {
    pub fn due(&self) -> Option<Instant> {
        self.held.as_ref().map(|held| held.at)
    }

    pub fn take(
        &mut self,
        fresh: Option<(F, Arc<dyn FrameSource>)>,
    ) -> Option<(F, Arc<dyn FrameSource>)> {
        match fresh {
            Some(fresh) => {
                self.held = None;
                Some(fresh)
            }
            None => self.held.take().map(|held| (held.frame, held.source)),
        }
    }

    pub fn defer(&mut self, frame: F, source: Arc<dyn FrameSource>, retry_at: Option<Instant>) {
        match retry_at {
            Some(at) => self.held = Some(Held { frame, source, at }),
            None => source.request_frame(),
        }
    }
}

#[must_use = "a produced frame is presented or superseded, never dropped"]
pub struct PaintFrame<'a> {
    source: Arc<dyn FrameSource>,
    content: Content<'a>,
}

pub enum Content<'a> {
    Accelerated(jfn_gpu_paint::SharedTexture),
    Software {
        size: PhysicalSize,
        pixels: &'a [u8],
        dirty: &'a [JfnRect],
    },
}

impl<'a> PaintFrame<'a> {
    pub fn accelerated(
        source: Arc<dyn FrameSource>,
        texture: jfn_gpu_paint::SharedTexture,
    ) -> PaintFrame<'a> {
        PaintFrame {
            source,
            content: Content::Accelerated(texture),
        }
    }

    pub fn software(
        source: Arc<dyn FrameSource>,
        size: PhysicalSize,
        pixels: &'a [u8],
        dirty: &'a [JfnRect],
    ) -> PaintFrame<'a> {
        PaintFrame {
            source,
            content: Content::Software {
                size,
                pixels,
                dirty,
            },
        }
    }

    pub fn content(&self) -> &Content<'a> {
        &self.content
    }

    pub fn source(&self) -> Arc<dyn FrameSource> {
        Arc::clone(&self.source)
    }

    pub fn present(self, commit: impl FnOnce(Content<'a>) -> Presented) -> Presented {
        commit(self.content)
    }

    pub fn supersede(self) -> Superseded {
        self.source.request_frame();
        Superseded(())
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Superseded(());
