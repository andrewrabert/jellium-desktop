use crate::ffi::coord_slot;
use crate::types::{MediaType, PlaybackPhase, PlayerPresence};

#[repr(u8)]
pub enum HotkeyAction {
    None = 0,
    Shutdown = 1,
    ToggleFullscreen = 2,
}

const VK_F: i32 = 0x46;
const VK_F4: i32 = 0x73;
const VK_F11: i32 = 0x7A;

const EVENTFLAG_ALT_DOWN: u32 = 1 << 3;

fn video_player_active() -> bool {
    let guard = coord_slot().lock();
    let Some(c) = guard.as_ref() else {
        return false;
    };
    let s = c.snapshot();
    s.media_type == MediaType::Video
        && s.presence == PlayerPresence::Present
        && s.phase != PlaybackPhase::Stopped
}

pub fn jfn_hotkey_classify_keydown(windows_key_code: i32, modifiers: u32) -> u8 {
    if windows_key_code == VK_F4 && (modifiers & EVENTFLAG_ALT_DOWN) != 0 {
        return HotkeyAction::Shutdown as u8;
    }
    if windows_key_code == VK_F || windows_key_code == VK_F11 {
        if !video_player_active() {
            return HotkeyAction::None as u8;
        }
        return HotkeyAction::ToggleFullscreen as u8;
    }
    HotkeyAction::None as u8
}
