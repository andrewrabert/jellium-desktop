use std::sync::OnceLock;

static SHUTDOWN_CB: OnceLock<fn()> = OnceLock::new();

unsafe extern "system" fn console_ctrl_handler(_t: u32) -> i32 {
    if let Some(cb) = SHUTDOWN_CB.get() {
        cb();
    }
    1
}

pub fn install_shutdown(on_shutdown: fn()) {
    let _ = SHUTDOWN_CB.set(on_shutdown);
    unsafe extern "system" {
        fn SetConsoleCtrlHandler(handler: unsafe extern "system" fn(u32) -> i32, add: i32) -> i32;
    }
    unsafe { SetConsoleCtrlHandler(console_ctrl_handler, 1) };
}
