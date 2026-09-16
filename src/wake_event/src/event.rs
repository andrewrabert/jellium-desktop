use core::ptr;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::Threading::{
    CreateEventW, INFINITE, ResetEvent, SetEvent, WaitForSingleObject,
};

pub struct WakeEvent {
    handle: HANDLE,
}

unsafe impl Send for WakeEvent {}
unsafe impl Sync for WakeEvent {}

impl WakeEvent {
    pub fn new() -> Option<Self> {
        let h = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
        if h.is_null() {
            return None;
        }
        Some(WakeEvent { handle: h })
    }

    pub fn signal(&self) {
        unsafe {
            SetEvent(self.handle);
        }
    }

    pub fn drain(&self) {
        unsafe {
            ResetEvent(self.handle);
        }
    }

    pub fn wait(&self) {
        unsafe {
            WaitForSingleObject(self.handle, INFINITE);
        }
    }
}

impl Drop for WakeEvent {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe { CloseHandle(self.handle) };
        }
    }
}
