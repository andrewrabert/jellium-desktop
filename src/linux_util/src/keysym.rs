//! xkb keysym → Windows VK code.

use xkbcommon::xkb::keysyms as ks;

pub fn keysym_to_vkey(sym: u32) -> i32 {
    if (ks::KEY_a..=ks::KEY_z).contains(&sym) {
        return (b'A' as u32 + (sym - ks::KEY_a)) as i32;
    }
    if (ks::KEY_A..=ks::KEY_Z).contains(&sym) {
        return sym as i32;
    }
    if (ks::KEY_0..=ks::KEY_9).contains(&sym) {
        return sym as i32;
    }
    if (ks::KEY_F1..=ks::KEY_F12).contains(&sym) {
        return 0x70 + (sym - ks::KEY_F1) as i32;
    }

    match sym {
        ks::KEY_Return => 0x0D,
        ks::KEY_Escape => 0x1B,
        ks::KEY_Tab | ks::KEY_ISO_Left_Tab => 0x09,
        ks::KEY_BackSpace => 0x08,
        ks::KEY_space => 0x20,
        ks::KEY_Left => 0x25,
        ks::KEY_Up => 0x26,
        ks::KEY_Right => 0x27,
        ks::KEY_Down => 0x28,
        ks::KEY_Home => 0x24,
        ks::KEY_End => 0x23,
        ks::KEY_Page_Up => 0x21,
        ks::KEY_Page_Down => 0x22,
        ks::KEY_Delete => 0x2E,
        ks::KEY_Insert => 0x2D,
        ks::KEY_Menu => 0x5D,
        ks::KEY_KP_Enter => 0x0D,
        ks::KEY_KP_Home => 0x24,
        ks::KEY_KP_Left => 0x25,
        ks::KEY_KP_Up => 0x26,
        ks::KEY_KP_Right => 0x27,
        ks::KEY_KP_Down => 0x28,
        ks::KEY_KP_Page_Up => 0x21,
        ks::KEY_KP_Page_Down => 0x22,
        ks::KEY_KP_End => 0x23,
        ks::KEY_KP_Insert => 0x2D,
        ks::KEY_KP_Delete => 0x2E,
        // OEM punctuation. Required so Chromium can derive event.key (e.g.
        // '>' from Shift+Period) for DOM keydown handlers; without a VK
        // here, jellyfin-web shortcuts like '<' / '>' never match.
        ks::KEY_semicolon | ks::KEY_colon => 0xBA,
        ks::KEY_equal | ks::KEY_plus => 0xBB,
        ks::KEY_comma | ks::KEY_less => 0xBC,
        ks::KEY_minus | ks::KEY_underscore => 0xBD,
        ks::KEY_period | ks::KEY_greater => 0xBE,
        ks::KEY_slash | ks::KEY_question => 0xBF,
        ks::KEY_grave | ks::KEY_asciitilde => 0xC0,
        ks::KEY_bracketleft | ks::KEY_braceleft => 0xDB,
        ks::KEY_backslash | ks::KEY_bar => 0xDC,
        ks::KEY_bracketright | ks::KEY_braceright => 0xDD,
        ks::KEY_apostrophe | ks::KEY_quotedbl => 0xDE,
        _ => 0,
    }
}
