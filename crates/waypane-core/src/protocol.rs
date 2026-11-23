use crate::wayland_backend::protocol::{AllowNull, ArgumentType, Interface, MessageDesc};
use bytes::{BufMut, Bytes, BytesMut};
use waypipe_client::{ProtocolMessage, Rid, WaylandMessage};

use crate::error::{Error, Result};

pub mod wayland {
    use crate::wayland_backend;
    wayland_scanner::generate_interfaces!("protocols/wayland.xml");
}

pub mod xdg {
    use super::wayland::{
        wl_output_interface, wl_seat_interface, wl_surface_interface, WL_OUTPUT_INTERFACE,
        WL_SEAT_INTERFACE, WL_SURFACE_INTERFACE,
    };
    use crate::wayland_backend;
    wayland_scanner::generate_interfaces!("protocols/xdg-shell.xml");
}

pub mod text_input {
    use super::wayland::{
        wl_seat_interface, wl_surface_interface, WL_SEAT_INTERFACE, WL_SURFACE_INTERFACE,
    };
    use crate::wayland_backend;
    wayland_scanner::generate_interfaces!("protocols/text-input-unstable-v3.xml");
}

pub const GLOBALS: &[(&str, u32)] = &[
    ("wl_compositor", 6),
    ("wl_subcompositor", 1),
    ("wl_shm", 2),
    ("xdg_wm_base", 6),
    ("wl_seat", 9),
    ("wl_output", 4),
    ("wl_data_device_manager", 3),
    ("zwp_text_input_manager_v3", 1),
];

pub fn event_opcode(interface: &Interface, name: &str) -> u16 {
    interface
        .events
        .iter()
        .position(|event| event.name == name)
        .expect("known Wayland event") as u16
}

pub fn request_opcode(interface: &Interface, name: &str) -> u16 {
    interface
        .requests
        .iter()
        .position(|request| request.name == name)
        .expect("known Wayland request") as u16
}

pub fn interface_by_name(name: &str) -> Option<&'static Interface> {
    [
        &wayland::WL_CALLBACK_INTERFACE,
        &wayland::WL_COMPOSITOR_INTERFACE,
        &wayland::WL_DATA_DEVICE_INTERFACE,
        &wayland::WL_DATA_DEVICE_MANAGER_INTERFACE,
        &wayland::WL_DATA_OFFER_INTERFACE,
        &wayland::WL_DATA_SOURCE_INTERFACE,
        &wayland::WL_DISPLAY_INTERFACE,
        &wayland::WL_BUFFER_INTERFACE,
        &wayland::WL_KEYBOARD_INTERFACE,
        &wayland::WL_OUTPUT_INTERFACE,
        &wayland::WL_POINTER_INTERFACE,
        &wayland::WL_REGION_INTERFACE,
        &wayland::WL_REGISTRY_INTERFACE,
        &wayland::WL_SEAT_INTERFACE,
        &wayland::WL_SHELL_INTERFACE,
        &wayland::WL_SHELL_SURFACE_INTERFACE,
        &wayland::WL_SHM_INTERFACE,
        &wayland::WL_SHM_POOL_INTERFACE,
        &wayland::WL_SUBCOMPOSITOR_INTERFACE,
        &wayland::WL_SUBSURFACE_INTERFACE,
        &wayland::WL_SURFACE_INTERFACE,
        &wayland::WL_TOUCH_INTERFACE,
        &xdg::XDG_POPUP_INTERFACE,
        &xdg::XDG_POSITIONER_INTERFACE,
        &xdg::XDG_SURFACE_INTERFACE,
        &xdg::XDG_TOPLEVEL_INTERFACE,
        &xdg::XDG_WM_BASE_INTERFACE,
        &text_input::ZWP_TEXT_INPUT_V3_INTERFACE,
        &text_input::ZWP_TEXT_INPUT_MANAGER_V3_INTERFACE,
    ]
    .into_iter()
    .find(|interface| interface.name == name)
}

#[derive(Debug, Clone)]
pub enum Value {
    Int(i32),
    Uint(u32),
    Fixed(i32),
    Str(Option<String>),
    Object(Option<u32>),
    NewId {
        id: u32,
        interface: Option<&'static Interface>,
    },
    Array(Vec<u8>),
    Fd(Rid),
}

impl Value {
    pub fn uint(&self) -> Option<u32> {
        match self {
            Self::Uint(value) => Some(*value),
            _ => None,
        }
    }

    pub fn int(&self) -> Option<i32> {
        match self {
            Self::Int(value) => Some(*value),
            _ => None,
        }
    }

    pub fn fixed(&self) -> Option<f64> {
        match self {
            Self::Fixed(value) => Some(f64::from(*value) / 256.0),
            _ => None,
        }
    }

    pub fn string(&self) -> Option<&str> {
        match self {
            Self::Str(Some(value)) => Some(value),
            _ => None,
        }
    }

    pub fn object(&self) -> Option<Option<u32>> {
        match self {
            Self::Object(value) => Some(*value),
            _ => None,
        }
    }

    pub fn new_id(&self) -> Option<(u32, Option<&'static Interface>)> {
        match self {
            Self::NewId { id, interface } => Some((*id, *interface)),
            _ => None,
        }
    }

    pub fn array(&self) -> Option<&[u8]> {
        match self {
            Self::Array(value) => Some(value),
            _ => None,
        }
    }

    pub fn fd(&self) -> Option<Rid> {
        match self {
            Self::Fd(rid) => Some(*rid),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DecodedRequest {
    pub object_id: u32,
    pub interface: &'static Interface,
    pub method: &'static MessageDesc,
    pub opcode: u16,
    pub version: u32,
    pub args: Vec<Value>,
}

pub(crate) fn request_method(
    message: &WaylandMessage,
    interface: &'static Interface,
    version: u32,
) -> Result<&'static MessageDesc> {
    let opcode = message.opcode() as usize;
    let method = interface.requests.get(opcode).ok_or(Error::InvalidOpcode {
        object_id: message.object_id(),
        interface: interface.name,
        opcode: message.opcode(),
    })?;
    if method.since > version {
        return Err(Error::InvalidOpcode {
            object_id: message.object_id(),
            interface: interface.name,
            opcode: message.opcode(),
        });
    }

    Ok(method)
}

pub(crate) fn fd_count(method: &MessageDesc) -> usize {
    method
        .signature
        .iter()
        .filter(|arg| matches!(arg, ArgumentType::Fd))
        .count()
}

pub(crate) fn decode_request(
    message: &WaylandMessage,
    interface: &'static Interface,
    version: u32,
    rids: &mut std::collections::VecDeque<Rid>,
) -> Result<DecodedRequest> {
    let method = request_method(message, interface, version)?;
    let expected_fds = fd_count(method);
    if expected_fds > 0 && !message.has_file_descriptors() {
        return Err(Error::protocol(format!(
            "{}.{} requires {expected_fds} fd(s) but has no FD tag",
            interface.name, method.name
        )));
    }

    let payload = message.payload();
    let mut cursor = Cursor {
        bytes: payload,
        offset: 0,
        interface,
        method,
    };
    let mut args = Vec::with_capacity(method.signature.len());
    for argument in method.signature {
        let value = match *argument {
            ArgumentType::Int => Value::Int(cursor.i32()?),
            ArgumentType::Uint => Value::Uint(cursor.u32()?),
            ArgumentType::Fixed => Value::Fixed(cursor.i32()?),
            ArgumentType::Str(allow_null) => Value::Str(cursor.string(allow_null)?),
            ArgumentType::Object(allow_null) => {
                let id = cursor.u32()?;
                if id == 0 && allow_null == AllowNull::No {
                    return Err(Error::malformed(
                        (interface, method),
                        "null object argument",
                    ));
                }
                Value::Object((id != 0).then_some(id))
            }
            ArgumentType::NewId => {
                let id = cursor.u32()?;
                if id == 0 {
                    return Err(Error::malformed((interface, method), "zero new_id"));
                }
                Value::NewId {
                    id,
                    interface: method.child_interface,
                }
            }
            ArgumentType::Array => Value::Array(cursor.array()?.to_vec()),
            ArgumentType::Fd => {
                let rid = rids.pop_front().ok_or_else(|| {
                    Error::malformed((interface, method), "missing RID for fd argument")
                })?;
                Value::Fd(rid)
            }
        };
        args.push(value);
    }
    if cursor.offset != payload.len() {
        return Err(Error::malformed(
            (interface, method),
            format!("{} trailing payload bytes", payload.len() - cursor.offset),
        ));
    }
    Ok(DecodedRequest {
        object_id: message.object_id(),
        interface,
        method,
        opcode: message.opcode(),
        version,
        args,
    })
}

struct Cursor<'a> {
    bytes: &'a [u8],
    offset: usize,
    interface: &'static Interface,
    method: &'static MessageDesc,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, size: usize) -> Result<&'a [u8]> {
        let end = self.offset.checked_add(size).ok_or_else(|| {
            Error::malformed((self.interface, self.method), "payload offset overflow")
        })?;
        let bytes = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| Error::malformed((self.interface, self.method), "truncated argument"))?;
        self.offset = end;
        Ok(bytes)
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn padded(&mut self, len: usize) -> Result<&'a [u8]> {
        let bytes = self.take(len)?;
        let padding = (4 - (len % 4)) % 4;
        self.take(padding)?;
        Ok(bytes)
    }

    fn string(&mut self, allow_null: AllowNull) -> Result<Option<String>> {
        let len = self.u32()? as usize;
        if len == 0 {
            if allow_null == AllowNull::No {
                return Err(Error::malformed(
                    (self.interface, self.method),
                    "null string where non-null required",
                ));
            }
            return Ok(None);
        }
        let bytes = self.padded(len)?;
        if bytes.last() != Some(&0) {
            return Err(Error::malformed(
                (self.interface, self.method),
                "string lacks NUL terminator",
            ));
        }
        std::str::from_utf8(&bytes[..bytes.len() - 1])
            .map(|text| Some(text.to_owned()))
            .map_err(|error| Error::malformed((self.interface, self.method), error.to_string()))
    }

    fn array(&mut self) -> Result<&'a [u8]> {
        let len = self.u32()? as usize;
        self.padded(len)
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Argument<'a> {
    Int(i32),
    Uint(u32),
    Fixed(f64),
    Object(u32),
    Str(Option<&'a str>),
    Array(&'a [u8]),
}

pub fn encode_message(object_id: u32, opcode: u16, args: &[Argument<'_>]) -> Bytes {
    let mut payload = BytesMut::new();
    for arg in args {
        match *arg {
            Argument::Int(value) => payload.put_i32_le(value),
            Argument::Uint(value) | Argument::Object(value) => payload.put_u32_le(value),
            Argument::Fixed(value) => payload.put_i32_le((value * 256.0).round() as i32),
            Argument::Str(value) => {
                if let Some(value) = value {
                    let len = value.len() + 1;
                    payload.put_u32_le(len as u32);
                    payload.extend_from_slice(value.as_bytes());
                    payload.put_u8(0);
                    pad4(&mut payload, len);
                } else {
                    payload.put_u32_le(0);
                }
            }
            Argument::Array(value) => {
                payload.put_u32_le(value.len() as u32);
                payload.extend_from_slice(value);
                pad4(&mut payload, value.len());
            }
        }
    }

    let len = payload.len() + 8;
    let mut output = BytesMut::with_capacity(len);
    output.put_u32_le(object_id);
    output.put_u32_le(((len as u32) << 16) | u32::from(opcode));
    output.extend_from_slice(&payload);
    output.freeze()
}

fn pad4(output: &mut BytesMut, len: usize) {
    let padding = (4 - (len % 4)) % 4;
    output.resize(output.len() + padding, 0);
}

pub(crate) fn event(object_id: u32, opcode: u16, args: &[Argument<'_>]) -> ProtocolMessage {
    ProtocolMessage::new(encode_message(object_id, opcode, args)).unwrap()
}

pub(crate) fn event_with_fd(
    object_id: u32,
    opcode: u16,
    args: &[Argument<'_>],
    rid: Rid,
) -> ProtocolMessage {
    ProtocolMessage::with_fds(encode_message(object_id, opcode, args), vec![rid]).unwrap()
}
