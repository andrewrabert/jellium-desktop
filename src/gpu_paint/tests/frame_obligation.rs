#[test]
fn a_frame_that_is_neither_presented_nor_superseded_does_not_compile() {
    trybuild::TestCases::new().compile_fail("tests/ui/*.rs");
}
