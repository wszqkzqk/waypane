use bytes::{BufMut, Bytes, BytesMut};

use crate::error::Error;
use crate::store::Rid;

pub const DEFAULT_MAX_FRAME_SIZE: usize = 64 * 1024 * 1024;
const WAYPIPE_FD_TAG: u32 = 1 << 11;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageType {
    Protocol,
    InjectRids,
    OpenFile,
    ExtendFile,
    OpenDmabuf,
    BufferFill,
    BufferDiff,
    OpenIrPipe,
    OpenIwPipe,
    OpenRwPipe,
    PipeTransfer,
    PipeShutdownR,
    PipeShutdownW,
    OpenDmaVidSrc,
    OpenDmaVidDst,
    SendDmaVidPacket,
    AckNblocks,
    Restart,
    Close,
    OpenDmaVidSrcV2,
    OpenDmaVidDstV2,
    OpenTimeline,
    SignalTimeline,
    Version,
}

impl MessageType {
    fn from_code(code: u32) -> Option<Self> {
        Some(match code {
            0 => Self::Protocol,
            1 => Self::InjectRids,
            2 => Self::OpenFile,
            3 => Self::ExtendFile,
            4 => Self::OpenDmabuf,
            5 => Self::BufferFill,
            6 => Self::BufferDiff,
            7 => Self::OpenIrPipe,
            8 => Self::OpenIwPipe,
            9 => Self::OpenRwPipe,
            10 => Self::PipeTransfer,
            11 => Self::PipeShutdownR,
            12 => Self::PipeShutdownW,
            13 => Self::OpenDmaVidSrc,
            14 => Self::OpenDmaVidDst,
            15 => Self::SendDmaVidPacket,
            16 => Self::AckNblocks,
            17 => Self::Restart,
            18 => Self::Close,
            19 => Self::OpenDmaVidSrcV2,
            20 => Self::OpenDmaVidDstV2,
            21 => Self::OpenTimeline,
            22 => Self::SignalTimeline,
            23 => Self::Version,
            _ => return None,
        })
    }

    fn code(self) -> u32 {
        match self {
            Self::Protocol => 0,
            Self::InjectRids => 1,
            Self::OpenFile => 2,
            Self::ExtendFile => 3,
            Self::OpenDmabuf => 4,
            Self::BufferFill => 5,
            Self::BufferDiff => 6,
            Self::OpenIrPipe => 7,
            Self::OpenIwPipe => 8,
            Self::OpenRwPipe => 9,
            Self::PipeTransfer => 10,
            Self::PipeShutdownR => 11,
            Self::PipeShutdownW => 12,
            Self::OpenDmaVidSrc => 13,
            Self::OpenDmaVidDst => 14,
            Self::SendDmaVidPacket => 15,
            Self::AckNblocks => 16,
            Self::Restart => 17,
            Self::Close => 18,
            Self::OpenDmaVidSrcV2 => 19,
            Self::OpenDmaVidDstV2 => 20,
            Self::OpenTimeline => 21,
            Self::SignalTimeline => 22,
            Self::Version => 23,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawFrame {
    pub kind: MessageType,
    pub payload: Bytes,
}

#[derive(Debug)]
pub struct FrameDecoder {
    buffer: BytesMut,
    max_frame_size: usize,
}

impl FrameDecoder {
    pub fn new(max_frame_size: usize) -> Self {
        Self {
            buffer: BytesMut::with_capacity(64 * 1024),
            max_frame_size,
        }
    }

    pub fn push(&mut self, data: &[u8]) {
        self.buffer.extend_from_slice(data);
    }

    pub fn next_frame(&mut self) -> Result<Option<RawFrame>, Error> {
        if self.buffer.len() < 4 {
            return Ok(None);
        }

        let header = u32::from_le_bytes(self.buffer[..4].try_into().unwrap());
        let kind =
            MessageType::from_code(header & 0x1f).ok_or(Error::UnknownFrameType(header & 0x1f))?;
        let len = (header >> 5) as usize;
        if len < 4 {
            return Err(Error::InvalidFrame(format!(
                "frame length {len} is below 4"
            )));
        }
        if len > self.max_frame_size {
            return Err(Error::FrameTooLarge(len));
        }
        let padded_len = len
            .checked_add(3)
            .map(|v| v & !3)
            .ok_or(Error::FrameTooLarge(len))?;
        if self.buffer.len() < padded_len {
            return Ok(None);
        }

        let bytes = self.buffer.split_to(padded_len).freeze();
        Ok(Some(RawFrame {
            kind,
            payload: bytes.slice(4..len),
        }))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaylandMessage {
    raw: Bytes,
}

impl WaylandMessage {
    pub fn object_id(&self) -> u32 {
        u32::from_le_bytes(self.raw[..4].try_into().unwrap())
    }

    pub fn opcode(&self) -> u16 {
        (u32::from_le_bytes(self.raw[4..8].try_into().unwrap()) & 0xff) as u16
    }

    pub fn has_file_descriptors(&self) -> bool {
        u32::from_le_bytes(self.raw[4..8].try_into().unwrap()) & WAYPIPE_FD_TAG != 0
    }

    pub fn payload(&self) -> &[u8] {
        &self.raw[8..]
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.raw
    }

    pub fn into_bytes(self) -> Bytes {
        self.raw
    }
}

pub fn parse_wayland_messages(mut bytes: Bytes) -> Result<Vec<WaylandMessage>, Error> {
    let mut messages = Vec::new();
    while !bytes.is_empty() {
        if bytes.len() < 8 {
            return Err(Error::InvalidWaylandMessage(
                "truncated message header".to_string(),
            ));
        }
        let header = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        let len = (header >> 16) as usize;
        if len < 8 || !len.is_multiple_of(4) || len > bytes.len() {
            return Err(Error::InvalidWaylandMessage(format!(
                "invalid length {len} with {} bytes remaining",
                bytes.len()
            )));
        }
        messages.push(WaylandMessage {
            raw: bytes.split_to(len),
        });
    }
    Ok(messages)
}

pub fn encode_frame(kind: MessageType, payload: &[u8]) -> Result<Bytes, Error> {
    let len = payload
        .len()
        .checked_add(4)
        .ok_or(Error::FrameTooLarge(usize::MAX))?;
    let header = u32::try_from(len)
        .ok()
        .and_then(|len| len.checked_mul(1 << 5))
        .map(|header| header | kind.code())
        .ok_or(Error::FrameTooLarge(len))?;

    let padded_len = len
        .checked_add(3)
        .map(|v| v & !3)
        .ok_or(Error::FrameTooLarge(len))?;
    let mut bytes = BytesMut::with_capacity(padded_len);
    bytes.put_u32_le(header);
    bytes.extend_from_slice(payload);
    bytes.resize(padded_len, 0);
    Ok(bytes.freeze())
}

pub fn encode_version(version: u32) -> Bytes {
    encode_frame(MessageType::Version, &version.to_le_bytes()).unwrap()
}

pub fn encode_close() -> Bytes {
    encode_frame(MessageType::Close, &0u32.to_le_bytes()).unwrap()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolMessage {
    bytes: Bytes,
    fds: Vec<Rid>,
}

impl ProtocolMessage {
    pub fn new(bytes: Bytes) -> Result<Self, Error> {
        Self::with_fds(bytes, Vec::new())
    }

    pub fn with_fds(bytes: Bytes, fds: Vec<Rid>) -> Result<Self, Error> {
        let messages = parse_wayland_messages(bytes.clone())?;
        if messages.len() != 1 {
            return Err(Error::InvalidWaylandMessage(format!(
                "expected one message, got {}",
                messages.len()
            )));
        }
        if messages[0].has_file_descriptors() {
            return Err(Error::InvalidWaylandMessage(
                "FD tags are assigned when ProtocolMessage is encoded".to_string(),
            ));
        }
        Ok(Self { bytes, fds })
    }
}

pub fn encode_protocol(messages: Bytes) -> Result<Bytes, Error> {
    let parsed = parse_wayland_messages(messages.clone())?;
    if parsed.iter().any(WaylandMessage::has_file_descriptors) {
        return Err(Error::InvalidWaylandMessage(
            "FD-tagged message requires ProtocolMessage RIDs".to_string(),
        ));
    }
    encode_frame(MessageType::Protocol, &messages)
}

pub fn encode_protocol_messages(messages: &[ProtocolMessage]) -> Result<(Bytes, Vec<Rid>), Error> {
    let mut bytes = BytesMut::new();
    let mut rids = Vec::new();
    for message in messages {
        if message.fds.is_empty() {
            bytes.extend_from_slice(&message.bytes);
        } else {
            let mut tagged = message.bytes.to_vec();
            let header = u32::from_le_bytes(tagged[4..8].try_into().unwrap());
            let header = (header & !0xff00) | WAYPIPE_FD_TAG;
            tagged[4..8].copy_from_slice(&header.to_le_bytes());
            bytes.extend_from_slice(&tagged);
            rids.extend_from_slice(&message.fds);
        }
    }
    Ok((encode_frame(MessageType::Protocol, &bytes)?, rids))
}

pub fn encode_rids(rids: &[Rid]) -> Result<Bytes, Error> {
    let mut payload = BytesMut::with_capacity(rids.len() * 4);
    for rid in rids {
        payload.put_i32_le(rid.0);
    }
    encode_frame(MessageType::InjectRids, &payload)
}

pub fn decode_rids(payload: &[u8]) -> Result<Vec<Rid>, Error> {
    if !payload.len().is_multiple_of(4) {
        return Err(Error::InvalidFrame(format!(
            "InjectRIDs payload length {} is not divisible by 4",
            payload.len()
        )));
    }
    Ok(payload
        .as_chunks::<4>()
        .0
        .iter()
        .map(|bytes| Rid(i32::from_le_bytes(*bytes)))
        .collect())
}
