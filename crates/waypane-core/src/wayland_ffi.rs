use std::ffi::{c_char, c_int};

#[repr(C)]
pub struct wl_message {
    pub name: *const c_char,
    pub signature: *const c_char,
    pub types: *const *const wl_interface,
}

#[repr(C)]
#[derive(Debug)]
pub struct wl_interface {
    pub name: *const c_char,
    pub version: c_int,
    pub request_count: c_int,
    pub requests: *const wl_message,
    pub event_count: c_int,
    pub events: *const wl_message,
}

unsafe impl Send for wl_interface {}
unsafe impl Sync for wl_interface {}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AllowNull {
    Yes,
    No,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ArgumentType {
    Int,
    Uint,
    Fixed,
    Str(AllowNull),
    Object(AllowNull),
    NewId,
    Array,
    Fd,
}

#[derive(Debug)]
pub struct Interface {
    pub name: &'static str,
    pub version: u32,
    pub requests: &'static [MessageDesc],
    pub events: &'static [MessageDesc],
    pub c_ptr: Option<&'static wl_interface>,
}

#[derive(Copy, Clone, Debug)]
pub struct MessageDesc {
    pub name: &'static str,
    pub signature: &'static [ArgumentType],
    pub since: u32,
    pub is_destructor: bool,
    pub child_interface: Option<&'static Interface>,
    pub arg_interfaces: &'static [&'static Interface],
}

pub static ANONYMOUS_INTERFACE: Interface = Interface {
    name: "<anonymous>",
    version: 0,
    requests: &[],
    events: &[],
    c_ptr: None,
};
