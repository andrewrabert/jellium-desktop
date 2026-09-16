#[derive(Debug, Clone)]
pub struct SurfaceStack<T: Copy + PartialEq> {
    live: Vec<T>,
    stack: Vec<T>,
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

    pub fn register(&mut self, h: T) {
        self.live.push(h);
    }

    pub fn deregister(&mut self, h: T) {
        self.live.retain(|&x| x != h);
        self.stack.retain(|&x| x != h);
        if self.main == Some(h) {
            self.main = self.stack.first().copied();
        }
    }

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

    fn h(n: usize) -> usize {
        n
    }

    #[test]
    fn empty_has_no_main() {
        let s: SurfaceStack<usize> = SurfaceStack::new();
        assert!(!s.is_main(h(1)));
        assert!(s.stack().is_empty());
    }

    #[test]
    fn macos_replace_stack_tracks_first_as_main() {
        let mut s = SurfaceStack::new();
        s.replace_stack(&[h(10), h(11), h(12)]);
        assert!(s.is_main(h(10)));
        assert_eq!(s.stack(), &[h(10), h(11), h(12)]);

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
        s.replace_stack(&[h(1)]);
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
