// Waypane: window-level Wayland forwarding over SSH.
// Copyright (C) 2026  Zhou Qiankang <wszqkzqk@qq.com>
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.
//
// SPDX-License-Identifier: GPL-3.0-or-later

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
