//! Every translation between the router's key identity and iced's.

use std::os::raw::c_int;

use iced_core::keyboard::{self, Key, Location, Modifiers, key};
use jfn_input::key::{PhysicalKey, ShellKey};
use jfn_platform_abi::DisplayBackend;
use keycode::{KeyMap, KeyMapping, KeyMappingCode};

/// The iced key event `key` becomes.
pub fn key_event(key: ShellKey) -> keyboard::Event {
    let logical = logical(key.windows_key_code, key.logical);
    let physical = physical(key.physical);
    let modifiers = modifiers(key.modifiers);
    if key.pressed {
        keyboard::Event::KeyPressed {
            key: logical.clone(),
            modified_key: logical,
            physical_key: physical,
            location: Location::Standard,
            modifiers,
            text: None,
            repeat: false,
        }
    } else {
        keyboard::Event::KeyReleased {
            key: logical.clone(),
            modified_key: logical,
            physical_key: physical,
            location: Location::Standard,
            modifiers,
        }
    }
}

/// The iced modifiers a CEF `EVENTFLAG_*` mask becomes.
pub fn modifiers(raw: u32) -> Modifiers {
    use jfn_platform_abi::event_flags as ef;
    let mut mods = Modifiers::empty();
    mods.set(Modifiers::SHIFT, raw & ef::EVENTFLAG_SHIFT_DOWN != 0);
    mods.set(Modifiers::CTRL, raw & ef::EVENTFLAG_CONTROL_DOWN != 0);
    mods.set(Modifiers::ALT, raw & ef::EVENTFLAG_ALT_DOWN != 0);
    mods.set(Modifiers::LOGO, raw & ef::EVENTFLAG_COMMAND_DOWN != 0);
    mods
}

/// The physical key iced resolves a shortcut from when the character cannot:
/// the W3C code the platform's own code names, through `keycode`'s Chrome
/// mapping.
///
/// A code the mapping does not name carries the platform's own code as a
/// [`key::NativeCode`].
pub fn physical(key: PhysicalKey) -> key::Physical {
    let (mapping, native) = match key {
        PhysicalKey::Xkb(code) => (KeyMapping::Xkb(code), key::NativeCode::Xkb(u32::from(code))),
        PhysicalKey::Windows(code) => (KeyMapping::Win(code), key::NativeCode::Windows(code)),
        PhysicalKey::MacOS(code) => (KeyMapping::Mac(code), key::NativeCode::MacOS(code)),
    };
    KeyMap::from_key_mapping(mapping)
        .ok()
        .and_then(|map| map.code)
        .and_then(code)
        .map_or(key::Physical::Unidentified(native), key::Physical::Code)
}

/// The named key the virtual-key code names, else the character the key
/// produces, else [`Key::Unidentified`].
pub fn logical(windows_key_code: c_int, logical: Option<char>) -> Key {
    let named = match windows_key_code {
        0x08 => Some(key::Named::Backspace),
        0x09 => Some(key::Named::Tab),
        0x0d => Some(key::Named::Enter),
        0x10 => Some(key::Named::Shift),
        0x11 => Some(key::Named::Control),
        0x12 => Some(key::Named::Alt),
        0x1b => Some(key::Named::Escape),
        0x20 => Some(key::Named::Space),
        0x21 => Some(key::Named::PageUp),
        0x22 => Some(key::Named::PageDown),
        0x23 => Some(key::Named::End),
        0x24 => Some(key::Named::Home),
        0x25 => Some(key::Named::ArrowLeft),
        0x26 => Some(key::Named::ArrowUp),
        0x27 => Some(key::Named::ArrowRight),
        0x28 => Some(key::Named::ArrowDown),
        0x2e => Some(key::Named::Delete),
        0x5d => Some(key::Named::ContextMenu),
        _ => None,
    };
    match (named, logical) {
        (Some(named), _) => Key::Named(named),
        (None, Some(c)) => Key::Character(c.to_string().into()),
        (None, None) => Key::Unidentified,
    }
}

/// The key press one typed character becomes for the focused field.
pub fn text_event(ch: char) -> keyboard::Event {
    let mut buffer = [0u8; 4];
    let text: iced_core::SmolStr = (&*ch.encode_utf8(&mut buffer)).into();
    let key = Key::Character(text.clone());
    keyboard::Event::KeyPressed {
        key: key.clone(),
        modified_key: key,
        physical_key: key::Physical::Unidentified(key::NativeCode::Unidentified),
        location: Location::Standard,
        modifiers: Modifiers::empty(),
        text: Some(text),
        repeat: false,
    }
}

/// The Windows virtual-key code of the Menu key.
const VK_APPS: c_int = 0x5d;
/// The Windows virtual-key code of F10.
const VK_F10: c_int = 0x79;

/// Whether `key` raises the edit menu for the focused field: the Menu key and
/// Shift+F10 on Wayland, X11 and Windows, and neither on macOS.
pub fn opens_edit_menu(backend: DisplayBackend, key: ShellKey) -> bool {
    use jfn_platform_abi::event_flags as ef;
    if !key.pressed || backend == DisplayBackend::MacOS {
        return false;
    }
    let shift = key.modifiers & ef::EVENTFLAG_SHIFT_DOWN != 0;
    key.windows_key_code == VK_APPS || (shift && key.windows_key_code == VK_F10)
}

/// The iced code a Chrome-mapping code names. The two enumerations share the
/// W3C names; the codes listed here are the ones both spell.
fn code(code: KeyMappingCode) -> Option<key::Code> {
    macro_rules! shared {
        ($($name:ident),* $(,)?) => {
            match code {
                $(KeyMappingCode::$name => Some(key::Code::$name),)*
                _ => None,
            }
        };
    }
    shared! {
    Abort,
    Again,
    AltLeft,
    AltRight,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    ArrowUp,
    AudioVolumeDown,
    AudioVolumeMute,
    AudioVolumeUp,
    Backquote,
    Backslash,
    Backspace,
    BracketLeft,
    BracketRight,
    BrowserBack,
    BrowserFavorites,
    BrowserForward,
    BrowserHome,
    BrowserRefresh,
    BrowserSearch,
    BrowserStop,
    CapsLock,
    Comma,
    ContextMenu,
    ControlLeft,
    ControlRight,
    Convert,
    Copy,
    Cut,
    Delete,
    Digit0,
    Digit1,
    Digit2,
    Digit3,
    Digit4,
    Digit5,
    Digit6,
    Digit7,
    Digit8,
    Digit9,
    Eject,
    End,
    Enter,
    Equal,
    Escape,
    F1,
    F10,
    F11,
    F12,
    F13,
    F14,
    F15,
    F16,
    F17,
    F18,
    F19,
    F2,
    F20,
    F21,
    F22,
    F23,
    F24,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    Find,
    Fn,
    FnLock,
    Help,
    Home,
    Hyper,
    Insert,
    IntlBackslash,
    IntlRo,
    IntlYen,
    KanaMode,
    KeyA,
    KeyB,
    KeyC,
    KeyD,
    KeyE,
    KeyF,
    KeyG,
    KeyH,
    KeyI,
    KeyJ,
    KeyK,
    KeyL,
    KeyM,
    KeyN,
    KeyO,
    KeyP,
    KeyQ,
    KeyR,
    KeyS,
    KeyT,
    KeyU,
    KeyV,
    KeyW,
    KeyX,
    KeyY,
    KeyZ,
    Lang1,
    Lang2,
    Lang3,
    Lang4,
    Lang5,
    LaunchApp1,
    LaunchApp2,
    LaunchMail,
    MediaPlayPause,
    MediaSelect,
    MediaStop,
    MediaTrackNext,
    MediaTrackPrevious,
    Minus,
    NonConvert,
    NumLock,
    Numpad0,
    Numpad1,
    Numpad2,
    Numpad3,
    Numpad4,
    Numpad5,
    Numpad6,
    Numpad7,
    Numpad8,
    Numpad9,
    NumpadAdd,
    NumpadBackspace,
    NumpadClear,
    NumpadClearEntry,
    NumpadComma,
    NumpadDecimal,
    NumpadDivide,
    NumpadEnter,
    NumpadEqual,
    NumpadMemoryAdd,
    NumpadMemoryClear,
    NumpadMemoryRecall,
    NumpadMemoryStore,
    NumpadMemorySubtract,
    NumpadMultiply,
    NumpadParenLeft,
    NumpadParenRight,
    NumpadSubtract,
    Open,
    PageDown,
    PageUp,
    Paste,
    Pause,
    Period,
    Power,
    PrintScreen,
    Props,
    Quote,
    Resume,
    ScrollLock,
    Select,
    Semicolon,
    ShiftLeft,
    ShiftRight,
    Slash,
    Sleep,
    Space,
    Suspend,
    Tab,
    Turbo,
    Undo,
    WakeUp
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jfn_platform_abi::event_flags::{EVENTFLAG_CONTROL_DOWN, EVENTFLAG_SHIFT_DOWN};

    /// The Windows virtual-key code of the Menu key, spelled here so a change
    /// to the constant it mirrors fails these tests.
    const VK_APPS: c_int = 0x5d;
    /// The Windows virtual-key code of F10, spelled here so a change to the
    /// constant it mirrors fails these tests.
    const VK_F10: c_int = 0x79;
    /// The Windows virtual-key code of F9, a key no edit menu binds.
    const VK_F9: c_int = 0x78;
    /// The Windows virtual-key code of the `A` key.
    const VK_A: c_int = 0x41;
    /// The Windows virtual-key code of the `Y` key.
    const VK_Y: c_int = 0x59;

    /// Every backend the Menu key and Shift+F10 raise the edit menu on.
    const WITH_MENU_KEY: [DisplayBackend; 3] = [
        DisplayBackend::Wayland,
        DisplayBackend::X11,
        DisplayBackend::Windows,
    ];

    fn press(windows_key_code: c_int, modifiers: u32, pressed: bool) -> ShellKey {
        ShellKey {
            pressed,
            modifiers,
            windows_key_code,
            logical: None,
            physical: PhysicalKey::Xkb(0),
        }
    }

    #[test]
    fn named_keys_name_themselves() {
        assert_eq!(logical(0x08, None), Key::Named(key::Named::Backspace));
        assert_eq!(logical(0x0d, None), Key::Named(key::Named::Enter));
        assert_eq!(logical(0x1b, None), Key::Named(key::Named::Escape));
        assert_eq!(logical(0x21, None), Key::Named(key::Named::PageUp));
        assert_eq!(logical(0x22, None), Key::Named(key::Named::PageDown));
        assert_eq!(logical(0x23, None), Key::Named(key::Named::End));
        assert_eq!(logical(0x24, None), Key::Named(key::Named::Home));
        assert_eq!(logical(0x25, None), Key::Named(key::Named::ArrowLeft));
        assert_eq!(logical(0x27, None), Key::Named(key::Named::ArrowRight));
        assert_eq!(logical(0x2e, None), Key::Named(key::Named::Delete));
        assert_eq!(logical(0x5d, None), Key::Named(key::Named::ContextMenu));
    }

    #[test]
    fn a_key_with_no_name_carries_its_character() {
        assert_eq!(logical(VK_A, Some('a')), Key::Character("a".into()));
        assert_eq!(logical(VK_Y, Some('y')), Key::Character("y".into()));
        assert_eq!(
            logical(VK_A, Some('\u{0444}')),
            Key::Character("\u{0444}".into())
        );
    }

    #[test]
    fn a_named_key_keeps_its_name_over_a_character() {
        assert_eq!(logical(0x20, Some(' ')), Key::Named(key::Named::Space));
    }

    #[test]
    fn a_key_with_neither_is_unidentified() {
        assert_eq!(logical(VK_F9, None), Key::Unidentified);
    }

    #[test]
    fn the_menu_key_raises_the_edit_menu_off_macos() {
        for backend in WITH_MENU_KEY {
            assert!(opens_edit_menu(backend, press(VK_APPS, 0, true)));
        }
        assert!(!opens_edit_menu(
            DisplayBackend::MacOS,
            press(VK_APPS, 0, true)
        ));
    }

    #[test]
    fn shift_f10_raises_the_edit_menu_off_macos() {
        for backend in WITH_MENU_KEY {
            assert!(opens_edit_menu(
                backend,
                press(VK_F10, EVENTFLAG_SHIFT_DOWN, true)
            ));
        }
        assert!(!opens_edit_menu(
            DisplayBackend::MacOS,
            press(VK_F10, EVENTFLAG_SHIFT_DOWN, true)
        ));
    }

    #[test]
    fn f10_without_shift_raises_nothing() {
        for backend in WITH_MENU_KEY {
            assert!(!opens_edit_menu(backend, press(VK_F10, 0, true)));
            assert!(!opens_edit_menu(
                backend,
                press(VK_F10, EVENTFLAG_CONTROL_DOWN, true)
            ));
        }
    }

    #[test]
    fn a_release_raises_nothing() {
        for backend in WITH_MENU_KEY {
            assert!(!opens_edit_menu(backend, press(VK_APPS, 0, false)));
            assert!(!opens_edit_menu(
                backend,
                press(VK_F10, EVENTFLAG_SHIFT_DOWN, false)
            ));
        }
    }
}
