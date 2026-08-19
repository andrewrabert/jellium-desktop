//! A produced frame and the proof that it reached the screen.
//!
//! A frame is a linear value: the only ways to consume it are presenting it,
//! which yields [`Presented`], or superseding it, which names the producer that
//! owes its successor and yields [`Superseded`]. Nothing releases one silently.

use std::sync::Arc;

use crate::{JfnRect, PhysicalSize};

/// The proof a commit site mints, minted where the commits are issued.
pub use jfn_gpu_paint::Presented;

/// Anything that can be asked to produce the frame replacing one that was not
/// presented.
pub trait FrameSource: Send + Sync {
    /// Invalidates the view and, where the host drives frames, requests the
    /// next one.
    fn request_frame(&self);
}

/// A frame a producer handed to a surface, owed to the compositor.
#[must_use = "a produced frame is presented or superseded, never dropped"]
pub struct PaintFrame<'a> {
    source: Arc<dyn FrameSource>,
    content: Content<'a>,
}

/// The two shapes CEF produces.
pub enum Content<'a> {
    /// A texture the app owns. By value, because a backend that presents off
    /// the callback thread (X11 and Wayland both do) has to keep it.
    Accelerated(jfn_gpu_paint::SharedTexture),
    /// CPU pixels in BGRA, tightly packed, with the regions that changed.
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

    /// The producer that owes this frame's successor.
    pub fn source(&self) -> Arc<dyn FrameSource> {
        Arc::clone(&self.source)
    }

    /// Hands the content to the commit site, which returns the proof it issued.
    pub fn present(self, commit: impl FnOnce(Content<'a>) -> Presented) -> Presented {
        commit(self.content)
    }

    /// Discharges this frame by asking its producer for the successor.
    pub fn supersede(self) -> Superseded {
        self.source.request_frame();
        Superseded(())
    }
}

/// Proof that a frame that was not presented has a named successor on the way.
#[derive(Clone, Copy, Debug)]
pub struct Superseded(());
