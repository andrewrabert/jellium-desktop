use parking_lot::Mutex;
use std::ffi::CString;
use std::os::raw::c_char;
use std::sync::OnceLock;

type ExecJsCb = extern "C" fn(*const c_char);

fn slot() -> &'static Mutex<Option<ExecJsCb>> {
    static SLOT: OnceLock<Mutex<Option<ExecJsCb>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

pub(crate) fn call(js: &str) {
    let Some(cb) = *slot().lock() else {
        return;
    };
    if let Ok(c) = CString::new(js) {
        cb(c.as_ptr());
    }
}

pub fn jfn_playback_set_web_exec_js_handler(cb: Option<ExecJsCb>) {
    *slot().lock() = cb;
}
