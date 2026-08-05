//! Per-surface registry: the bottom-to-top stacking order plus
//! main-surface tracking, shared by both compositors.
//!
//! Generic over the handle type `T` (the backend's opaque `*mut Surface`)
//! so the OS layer keeps its own pointer type and does its own
//! reparenting/visual work — this struct only does the bookkeeping. `T` is
//! `Copy + PartialEq` (raw pointers qualify); equality is by value/identity.
//!
//! macOS derives the main surface from `stack.first()` and tracks `live`
//! only to answer "is this handle still valid". The Windows compositor keeps
//! its own ordered registry: its stack order *is* its child order, and it has
//! no main surface to name.

/// Bottom-to-top surface registry with main-surface tracking.
#[derive(Debug, Clone)]
pub struct SurfaceStack<T: Copy + PartialEq> {
    /// All allocated surfaces, registered through `register`.
    live: Vec<T>,
    /// Current bottom-to-top stacking order.
    stack: Vec<T>,
    /// The "main" (bottom-most / mpv) surface that transition gating keys
    /// off, kept equal to `stack.first()`.
    main: Option<T>,
}

impl<T: Copy + PartialEq> SurfaceStack<T> {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            live: Vec::new(),
            stack: Vec::new(),
            main: None,
        }
    }

    /// macOS alloc: register a newly allocated surface in the live set
    /// *without* touching `main`.
    ///
    /// macOS derives the main surface from `stack.first()`, so registering
    /// must not name one.
    pub fn register(&mut self, h: T) {
        self.live.push(h);
    }

    /// macOS free: the counterpart to [`Self::register`] — drop the surface
    /// from both lists and re-derive `main` as `stack.first()` alone.
    pub fn deregister(&mut self, h: T) {
        self.live.retain(|&x| x != h);
        self.stack.retain(|&x| x != h);
        if self.main == Some(h) {
            self.main = self.stack.first().copied();
        }
    }

    /// macOS restack: replace the entire stacking order and set main to the
    /// new bottom (`None` if empty).
    pub fn replace_stack(&mut self, ordered: &[T]) {
        self.stack.clear();
        self.stack.extend_from_slice(ordered);
        self.main = self.stack.first().copied();
    }

    #[must_use]
    pub fn is_main(&self, h: T) -> bool {
        self.main == Some(h)
    }

    #[must_use]
    pub fn stack(&self) -> &[T] {
        &self.stack
    }

    #[must_use]
    pub fn live(&self) -> &[T] {
        &self.live
    }

    /// macOS cleanup: drain the stack (to detach each subview) and reset
    /// main.
    pub fn take_stack(&mut self) -> Vec<T> {
        self.main = None;
        std::mem::take(&mut self.stack)
    }
}

impl<T: Copy + PartialEq> Default for SurfaceStack<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Use distinct integers as stand-in surface handles.
    fn h(n: usize) -> usize {
        n
    }

    #[test]
    fn empty_has_no_main() {
        let s: SurfaceStack<usize> = SurfaceStack::new();
        assert!(!s.is_main(h(1)));
        assert!(s.stack().is_empty());
    }

    // ---- macOS model ------------------------------------------------

    #[test]
    fn macos_replace_stack_tracks_first_as_main() {
        let mut s = SurfaceStack::new();
        s.replace_stack(&[h(10), h(11), h(12)]);
        assert!(s.is_main(h(10)));
        assert_eq!(s.stack(), &[h(10), h(11), h(12)]);

        // Restack to a new order updates main to the new bottom.
        s.replace_stack(&[h(11), h(12)]);
        assert!(s.is_main(h(11)));
    }

    #[test]
    fn macos_replace_stack_empty_clears_main() {
        let mut s = SurfaceStack::new();
        s.replace_stack(&[h(10)]);
        s.replace_stack(&[]);
        assert!(!s.is_main(h(10)));
    }

    #[test]
    fn register_never_sets_main() {
        let mut s = SurfaceStack::new();
        s.register(h(1));
        assert!(!s.is_main(h(1)));
        assert_eq!(s.live(), &[h(1)]);
    }

    #[test]
    fn deregister_rederives_main_from_stack_only() {
        let mut s = SurfaceStack::new();
        s.register(h(1));
        s.register(h(2));
        s.replace_stack(&[h(1)]); // main = 1, h(2) live but unstacked
        s.deregister(h(1));
        assert!(!s.is_main(h(1)));
        assert!(!s.is_main(h(2)));
        assert!(s.stack().is_empty());
        assert_eq!(s.live(), &[h(2)]);
    }

    #[test]
    fn deregister_keeps_main_equal_to_stack_first() {
        let mut s = SurfaceStack::new();
        s.register(h(1));
        s.register(h(2));
        s.replace_stack(&[h(1), h(2)]);
        s.deregister(h(1));
        assert!(s.is_main(h(2)));
    }

    #[test]
    fn macos_take_stack_resets() {
        let mut s = SurfaceStack::new();
        s.replace_stack(&[h(10), h(11)]);
        let drained = s.take_stack();
        assert_eq!(drained, vec![h(10), h(11)]);
        assert!(!s.is_main(h(10)));
    }
}
