//! A surface's visibility, owned by the backend that allocated the surface.
//!
//! No other value in the process states whether a surface is on screen, and a
//! request completes only on a commit that was issued — on backends whose
//! protocol acknowledges commits, only on one that landed.

/// What a surface's owner asked the compositor for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Visibility {
    Shown,
    Hidden,
}

impl Visibility {
    /// The visibility a boolean predicate asks for.
    pub fn shown(shown: bool) -> Visibility {
        if shown {
            Visibility::Shown
        } else {
            Visibility::Hidden
        }
    }

    pub fn is_shown(self) -> bool {
        matches!(self, Visibility::Shown)
    }
}

/// A visibility change whose commit has been issued, carrying the
/// acknowledgement it waits on.
#[must_use = "a visibility change completes only when its commit is acknowledged"]
pub struct VisibilityCommit {
    visibility: Visibility,
    ack: Ack,
}

impl VisibilityCommit {
    /// Minted at the commit site, by the surface's allocator alone.
    pub fn issued(visibility: Visibility, ack: Ack) -> VisibilityCommit {
        VisibilityCommit { visibility, ack }
    }

    /// Blocks until the compositor acknowledged the commit.
    pub fn acknowledged(self) -> Visibility {
        (self.ack.0)();
        self.visibility
    }
}

/// How a backend learns its commit landed.
pub struct Ack(Box<dyn FnOnce() + Send>);

impl Ack {
    /// The platform call applied the change before it returned.
    pub fn immediate() -> Ack {
        Ack(Box::new(|| {}))
    }

    /// `wait` blocks until the compositor acknowledged the commit.
    pub fn deferred(wait: Box<dyn FnOnce() + Send>) -> Ack {
        Ack(wait)
    }
}
