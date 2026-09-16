use std::ffi::{CString, NulError};
use std::os::raw::c_char;

pub struct Command {
    storage: Vec<CString>,
    ptrs: Vec<*const c_char>,
}

impl Command {
    pub fn new<I, S>(args: I) -> Result<Self, NulError>
    where
        I: IntoIterator<Item = S>,
        S: Into<Vec<u8>>,
    {
        let storage: Vec<CString> = args
            .into_iter()
            .map(|s| CString::new(s.into()))
            .collect::<Result<_, _>>()?;
        let mut ptrs: Vec<*const c_char> = storage.iter().map(|c| c.as_ptr()).collect();
        ptrs.push(std::ptr::null());
        Ok(Self { storage, ptrs })
    }

    pub fn as_ptr(&self) -> *mut *const c_char {
        self.ptrs.as_ptr() as *mut _
    }

    pub fn len(&self) -> usize {
        self.storage.len()
    }

    pub fn is_empty(&self) -> bool {
        self.storage.is_empty()
    }
}

unsafe impl Send for Command {}
unsafe impl Sync for Command {}
