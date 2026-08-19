//! The two text rules every backend and the shell overlay share.

/// Pairs the UTF-16 code units a platform delivers into whole characters.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct Utf16 {
    high: Option<u16>,
}

const HIGH_SURROGATE: std::ops::Range<u16> = 0xD800..0xDC00;
const LOW_SURROGATE: std::ops::Range<u16> = 0xDC00..0xE000;

impl Utf16 {
    pub const fn new() -> Utf16 {
        Utf16 { high: None }
    }

    /// The character `unit` completes.
    ///
    /// `None` while a high surrogate waits for its low half, and for a
    /// surrogate that never pairs.
    pub fn feed(&mut self, unit: u16) -> Option<char> {
        let pending = self.high.take();
        if HIGH_SURROGATE.contains(&unit) {
            self.high = Some(unit);
            return None;
        }
        if LOW_SURROGATE.contains(&unit) {
            let high = pending?;
            let code = 0x1_0000 + ((u32::from(high) - 0xD800) << 10) + (u32::from(unit) - 0xDC00);
            return char::from_u32(code);
        }
        char::from_u32(u32::from(unit))
    }
}

/// `text` with every control character removed, for insertion into a shell
/// field on one line.
pub fn one_line(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lone_unit_is_its_own_character() {
        let mut utf16 = Utf16::new();
        assert_eq!(utf16.feed(u16::from(b'a')), Some('a'));
    }

    #[test]
    fn a_surrogate_pair_makes_one_character() {
        let mut utf16 = Utf16::new();
        assert_eq!(utf16.feed(0xD83D), None);
        assert_eq!(utf16.feed(0xDE00), Some('\u{1F600}'));
    }

    #[test]
    fn an_unpaired_high_surrogate_yields_nothing() {
        let mut utf16 = Utf16::new();
        assert_eq!(utf16.feed(0xD83D), None);
        assert_eq!(utf16.feed(0xD83D), None);
        assert_eq!(utf16.feed(0xDE00), Some('\u{1F600}'));
    }

    #[test]
    fn an_unpaired_low_surrogate_yields_nothing() {
        let mut utf16 = Utf16::new();
        assert_eq!(utf16.feed(0xDE00), None);
    }

    #[test]
    fn one_line_drops_newlines_and_tabs() {
        assert_eq!(one_line("a\nb\tc\r\nd"), "abcd");
    }

    #[test]
    fn one_line_keeps_ordinary_text() {
        assert_eq!(one_line("héllo wörld"), "héllo wörld");
    }
}
