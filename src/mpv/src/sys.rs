#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(non_upper_case_globals)]
#![allow(dead_code)]
#![allow(clippy::all)]

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct mpv_node_list {
    pub num: ::std::os::raw::c_int,
    pub values: *mut mpv_node,
    pub keys: *mut *mut ::std::os::raw::c_char,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct mpv_byte_array {
    pub data: *mut ::std::os::raw::c_void,
    pub size: usize,
}
