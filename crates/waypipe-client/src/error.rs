use std::fmt;

use crate::header::Compression;
use crate::store::Rid;
use crate::wire::MessageType;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    InvalidHeader(&'static str),
    ProtocolVersion(u32),
    CompressionMismatch {
        expected: Compression,
        actual: Compression,
    },
    DmabufUnsupported,
    UnknownFrameType(u32),
    FrameTooLarge(usize),
    InvalidFrame(String),
    InvalidWaylandMessage(String),
    UnsupportedFrame(MessageType),
    UnknownRid(Rid),
    UnknownRidContext {
        rid: Rid,
        operation: String,
        state: &'static str,
    },
    DuplicateRid(Rid),
    ConnectionClosed,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidHeader(reason) => write!(f, "invalid waypipe connection header: {reason}"),
            Self::ProtocolVersion(version) => {
                write!(f, "unsupported waypipe protocol version {version:#x}")
            }
            Self::CompressionMismatch { expected, actual } => {
                write!(
                    f,
                    "compression mismatch: expected {expected:?}, got {actual:?}"
                )
            }
            Self::DmabufUnsupported => write!(f, "DMABUF transport is not enabled"),
            Self::UnknownFrameType(kind) => write!(f, "unknown waypipe frame type {kind}"),
            Self::FrameTooLarge(size) => write!(f, "waypipe frame of {size} bytes exceeds limit"),
            Self::InvalidFrame(reason) => write!(f, "invalid waypipe frame: {reason}"),
            Self::InvalidWaylandMessage(reason) => write!(f, "invalid Wayland message: {reason}"),
            Self::UnsupportedFrame(kind) => write!(f, "unsupported waypipe frame type {kind:?}"),
            Self::UnknownRid(rid) => write!(f, "unknown waypipe resource id {}", rid.0),
            Self::UnknownRidContext {
                rid,
                operation,
                state,
            } => write!(
                f,
                "unknown waypipe resource id {} while {operation} ({state})",
                rid.0
            ),
            Self::DuplicateRid(rid) => write!(f, "duplicate waypipe resource id {}", rid.0),
            Self::ConnectionClosed => write!(f, "waypipe connection is closed"),
        }
    }
}

impl std::error::Error for Error {}
