use std::sync::OnceLock;

use windows_sys::Win32::Foundation::TRUE;
use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
use windows_sys::core::BOOL;

static SHUTDOWN_CB: OnceLock<fn()> = OnceLock::new();

unsafe extern "system" fn console_ctrl_handler(_t: u32) -> BOOL {
    if let Some(cb) = SHUTDOWN_CB.get() {
        cb();
    }
    TRUE
}

pub fn install_shutdown(on_shutdown: fn()) {
    let _ = SHUTDOWN_CB.set(on_shutdown);
    unsafe { SetConsoleCtrlHandler(Some(console_ctrl_handler), TRUE) };
}
