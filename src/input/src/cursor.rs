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

pub fn cursor_from_web(shape: CursorShape) {
    WEB_SHAPE.store(shape.as_raw(), Ordering::Relaxed);
    if OWNER.load(Ordering::Relaxed) == OWNER_WEB {
        apply(shape);
    }
}

pub fn cursor_from_shell(shape: CursorShape) {
    SHELL_SHAPE.store(shape.as_raw(), Ordering::Relaxed);
    if OWNER.load(Ordering::Relaxed) == OWNER_SHELL {
        apply(shape);
    }
}

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
    if let Some(p) = jfn_platform_abi::try_lease() {
        p.platform().set_cursor(shape);
    }
}
