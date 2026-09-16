use std::os::raw::c_int;

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum PhysicalKey {
    Xkb(u16),
    Windows(u16),
    MacOS(u16),
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct KeyReport {
    pub pressed: bool,
    pub modifiers: u32,
    pub windows_key_code: c_int,
    pub native_key_code: c_int,
    pub is_system_key: bool,
    pub character: u16,
    pub unmodified_character: u16,
    pub logical: Option<char>,
    pub physical: PhysicalKey,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct ShellKey {
    pub pressed: bool,
    pub modifiers: u32,
    pub windows_key_code: c_int,
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
