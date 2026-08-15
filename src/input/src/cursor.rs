//! Cursor arbitration.
//!
//! Both producers keep a current shape at all times; the router's last pointer
//! decision picks which of the two the platform sees, and a change of owner
//! replays the incoming producer's shape so the pointer is right from the
//! switch onwards.

use crate::route::Target;
use jfn_platform_abi::cursor::CursorShape;
use std::sync::atomic::{AtomicI32, AtomicU8, Ordering};

const UNSET: i32 = i32::MIN;

static WEB_SHAPE: AtomicI32 = AtomicI32::new(UNSET);
static SHELL_SHAPE: AtomicI32 = AtomicI32::new(UNSET);
static OWNER: AtomicU8 = AtomicU8::new(OWNER_NONE);

const OWNER_NONE: u8 = 0;
const OWNER_WEB: u8 = 1;
const OWNER_SHELL: u8 = 2;

/// The shape jellyfin-web last asked for. Applied only while the router says
/// the web layer owns the pointer.
pub fn cursor_from_web(shape: CursorShape) {
    WEB_SHAPE.store(shape.as_raw(), Ordering::Relaxed);
    if OWNER.load(Ordering::Relaxed) == OWNER_WEB {
        apply(shape);
    }
}

/// The shape iced's `mouse_interaction` last resolved to. Applied only while
/// the router says the shell overlay owns the pointer.
pub fn cursor_from_shell(shape: CursorShape) {
    SHELL_SHAPE.store(shape.as_raw(), Ordering::Relaxed);
    if OWNER.load(Ordering::Relaxed) == OWNER_SHELL {
        apply(shape);
    }
}

/// Hand the pointer to `target`. A change of owner replays that owner's shape.
pub(crate) fn set_owner(target: Target) {
    let owner = match target {
        Target::Shell => OWNER_SHELL,
        Target::Web => OWNER_WEB,
        Target::None => OWNER_NONE,
    };
    if OWNER.swap(owner, Ordering::Relaxed) == owner {
        return;
    }
    let raw = match owner {
        OWNER_SHELL => SHELL_SHAPE.load(Ordering::Relaxed),
        OWNER_WEB => WEB_SHAPE.load(Ordering::Relaxed),
        _ => UNSET,
    };
    if let Some(shape) = CursorShape::from_cef(raw) {
        apply(shape);
    }
}

fn apply(shape: CursorShape) {
    if let Some(p) = jfn_platform_abi::try_get() {
        p.set_cursor(shape);
    }
}
