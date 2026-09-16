#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Visibility {
    Shown,
    Hidden,
}

impl Visibility {
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

#[must_use = "a visibility change completes only when its commit is acknowledged"]
pub struct VisibilityCommit {
    visibility: Visibility,
    ack: Ack,
}

impl VisibilityCommit {
    pub fn issued(visibility: Visibility, ack: Ack) -> VisibilityCommit {
        VisibilityCommit { visibility, ack }
    }

    pub fn acknowledged(self) -> Visibility {
        (self.ack.0)();
        self.visibility
    }
}

pub struct Ack(Box<dyn FnOnce() + Send>);

impl Ack {
    pub fn immediate() -> Ack {
        Ack(Box::new(|| {}))
    }

    pub fn deferred(wait: Box<dyn FnOnce() + Send>) -> Ack {
        Ack(wait)
    }
}
