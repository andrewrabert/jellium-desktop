//! The frame's linearity is a compile-time claim, so it is checked by
//! compiling: remove `#[must_use]` from the swapchain frame and these pass.

#[test]
fn a_frame_that_is_neither_presented_nor_superseded_does_not_compile() {
    trybuild::TestCases::new().compile_fail("tests/ui/*.rs");
}
