//! The key identity every backend reports and the shell overlay receives.

use std::os::raw::c_int;

/// The physical key a backend reported, in that backend's own numbering.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum PhysicalKey {
    /// An xkb keycode: the evdev code plus 8.
    Xkb(u16),
    /// A Windows scancode, `0xE0`-prefixed for an extended key.
    Windows(u16),
    /// A macOS virtual key code.
    MacOS(u16),
}

/// A key press or release as a backend reports it.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct KeyReport {
    pub pressed: bool,
    /// CEF `EVENTFLAG_*` mask.
    pub modifiers: u32,
    pub windows_key_code: c_int,
    pub native_key_code: c_int,
    pub is_system_key: bool,
    /// CEF's UTF-16 character, carried to jellyfin-web unchanged.
    pub character: u16,
    /// CEF's UTF-16 unmodified character, carried to jellyfin-web unchanged.
    pub unmodified_character: u16,
    /// The character the key produces with no modifier applied.
    pub logical: Option<char>,
    pub physical: PhysicalKey,
}

/// A key press or release as the shell overlay receives it.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct ShellKey {
    pub pressed: bool,
    /// CEF `EVENTFLAG_*` mask.
    pub modifiers: u32,
    pub windows_key_code: c_int,
    /// The character the key produces with no modifier applied.
    pub logical: Option<char>,
    pub physical: PhysicalKey,
}

impl KeyReport {
    pub fn shell_key(&self) -> ShellKey {
        ShellKey {
            pressed: self.pressed,
            modifiers: self.modifiers,
            windows_key_code: self.windows_key_code,
            logical: self.logical,
            physical: self.physical,
        }
    }
}

/// The character a key produces with no modifier applied: `codepoint`
/// lowercased.
///
/// `None` for a control character, a surrogate, and a key that produces none.
pub fn logical_char(codepoint: u32) -> Option<char> {
    let ch = char::from_u32(codepoint).filter(|c| !c.is_control())?;
    ch.to_lowercase().next()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_uppercase_letter_lowercases() {
        assert_eq!(logical_char(u32::from(b'C')), Some('c'));
    }

    #[test]
    fn a_control_codepoint_has_no_character() {
        assert_eq!(logical_char(0x0D), None);
        assert_eq!(logical_char(0), None);
    }

    #[test]
    fn a_surrogate_has_no_character() {
        assert_eq!(logical_char(0xD800), None);
    }

    #[test]
    fn a_cyrillic_letter_lowercases() {
        assert_eq!(logical_char(0x0421), Some('\u{0441}'));
    }
}
