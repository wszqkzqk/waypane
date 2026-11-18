use std::ops::Range;

use bytes::{Bytes, BytesMut};

use crate::error::Error;
use crate::header::{Compression, ConnectionHeader, HEADER_SIZE, MIN_PROTOCOL_VERSION};
use crate::shm::{Rid, ShmStore, DEFAULT_MAX_SHM_FILE_SIZE};
use crate::wire::{
    decode_rids, encode_close, encode_protocol, encode_version, parse_wayland_messages,
    FrameDecoder, MessageType, RawFrame, WaylandMessage, DEFAULT_MAX_FRAME_SIZE,
};

#[derive(Debug, Clone, Copy)]
pub struct ClientConfig {
    pub max_frame_size: usize,
    pub max_shm_file_size: usize,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            max_frame_size: DEFAULT_MAX_FRAME_SIZE,
            max_shm_file_size: DEFAULT_MAX_SHM_FILE_SIZE,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientEvent {
    Negotiated { remote_version: u32, version: u32 },
    ShmOpened { rid: Rid, size: usize },
    ShmExtended { rid: Rid, size: usize },
    ShmUpdated { rid: Rid, damage: Vec<Range<usize>> },
    RidsInjected(Vec<Rid>),
    Protocol(Vec<WaylandMessage>),
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientBatch {
    pub events: Vec<ClientEvent>,
    pub outgoing: Bytes,
}

#[derive(Debug, Clone, Copy)]
enum Stage {
    AwaitHeader {
        bytes: [u8; HEADER_SIZE],
        len: usize,
    },
    Ready(ConnectionHeader),
}

#[derive(Debug)]
pub struct Client {
    stage: Stage,
    decoder: FrameDecoder,
    store: ShmStore,
    closed: bool,
}

impl Client {
    pub fn new(config: ClientConfig) -> Self {
        Self {
            stage: Stage::AwaitHeader {
                bytes: [0; HEADER_SIZE],
                len: 0,
            },
            decoder: FrameDecoder::new(config.max_frame_size),
            store: ShmStore::new(config.max_shm_file_size),
            closed: false,
        }
    }

    pub fn receive(&mut self, input: &[u8]) -> Result<ClientBatch, Error> {
        if self.closed {
            return Err(Error::ConnectionClosed);
        }

        let mut events = Vec::new();
        let mut outgoing = BytesMut::new();
        let mut remaining = input;

        if let Stage::AwaitHeader { mut bytes, mut len } = self.stage {
            let copied = (HEADER_SIZE - len).min(remaining.len());
            bytes[len..len + copied].copy_from_slice(&remaining[..copied]);
            len += copied;
            remaining = &remaining[copied..];

            if len < HEADER_SIZE {
                self.stage = Stage::AwaitHeader { bytes, len };
                return Ok(ClientBatch {
                    events,
                    outgoing: outgoing.freeze(),
                });
            }

            let header = ConnectionHeader::decode(&bytes)?;
            if header.compression != Compression::None {
                return Err(Error::CompressionMismatch {
                    expected: Compression::None,
                    actual: header.compression,
                });
            }
            if header.has_dmabuf {
                return Err(Error::DmabufUnsupported);
            }
            if header.version > MIN_PROTOCOL_VERSION {
                outgoing.extend_from_slice(&encode_version(header.version));
            }
            events.push(ClientEvent::Negotiated {
                remote_version: header.remote_version,
                version: header.version,
            });
            self.stage = Stage::Ready(header);
        }

        self.decoder.push(remaining);
        while let Some(frame) = self.decoder.next_frame()? {
            self.process_frame(frame, &mut events)?;
            if self.closed {
                break;
            }
        }

        Ok(ClientBatch {
            events,
            outgoing: outgoing.freeze(),
        })
    }

    pub fn encode_protocol(&self, messages: Bytes) -> Result<Bytes, Error> {
        if self.closed {
            return Err(Error::ConnectionClosed);
        }
        if !matches!(self.stage, Stage::Ready(_)) {
            return Err(Error::InvalidFrame(
                "cannot send Protocol before connection negotiation".to_string(),
            ));
        }
        encode_protocol(messages)
    }

    pub fn close_message(&self) -> Bytes {
        encode_close()
    }

    pub fn header(&self) -> Option<&ConnectionHeader> {
        match &self.stage {
            Stage::AwaitHeader { .. } => None,
            Stage::Ready(header) => Some(header),
        }
    }

    pub fn shm(&self) -> &ShmStore {
        &self.store
    }

    pub fn shm_mut(&mut self) -> &mut ShmStore {
        &mut self.store
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    fn process_frame(
        &mut self,
        frame: RawFrame,
        events: &mut Vec<ClientEvent>,
    ) -> Result<(), Error> {
        let payload = frame.payload;
        match frame.kind {
            MessageType::Protocol => {
                let messages = parse_wayland_messages(payload)?;
                if !messages.is_empty() {
                    events.push(ClientEvent::Protocol(messages));
                }
            }
            MessageType::InjectRids => {
                let rids = decode_rids(&payload)?;
                for rid in &rids {
                    self.store.inject(*rid)?;
                }
                events.push(ClientEvent::RidsInjected(rids));
            }
            MessageType::OpenFile => {
                let (rid, size) = read_rid_and_size(&payload, "OpenFile")?;
                let size = self.store.open_file(rid, size)?;
                events.push(ClientEvent::ShmOpened { rid, size });
            }
            MessageType::ExtendFile => {
                let (rid, size) = read_rid_and_size(&payload, "ExtendFile")?;
                let size = self.store.extend_file(rid, size)?;
                events.push(ClientEvent::ShmExtended { rid, size });
            }
            MessageType::BufferFill | MessageType::BufferDiff => {
                let rid = read_rid(&payload)?;
                let damage = if frame.kind == MessageType::BufferFill {
                    self.store.apply_fill(rid, &payload[4..])?
                } else {
                    self.store.apply_diff(rid, &payload[4..])?
                };
                if !damage.is_empty() {
                    events.push(ClientEvent::ShmUpdated { rid, damage });
                }
            }
            MessageType::Close => {
                if !(payload.is_empty() || payload.len() == 4) {
                    return Err(Error::InvalidFrame(format!(
                        "Close payload has {} bytes, expected 0 or 4",
                        payload.len()
                    )));
                }
                self.closed = true;
                events.push(ClientEvent::Closed);
            }
            MessageType::Version => {
                return Err(Error::InvalidFrame(
                    "waypipe server sent a Version frame".to_string(),
                ));
            }
            MessageType::AckNblocks | MessageType::Restart => {
                return Err(Error::InvalidFrame(format!(
                    "reconnection frame {:?} is disabled at protocol 0x11",
                    frame.kind
                )));
            }
            other => return Err(Error::UnsupportedFrame(other)),
        }
        Ok(())
    }
}

impl Default for Client {
    fn default() -> Self {
        Self::new(ClientConfig::default())
    }
}

fn read_rid(payload: &[u8]) -> Result<Rid, Error> {
    if payload.len() < 4 {
        return Err(Error::InvalidFrame(
            "frame is missing its resource id".to_string(),
        ));
    }
    Ok(Rid(i32::from_le_bytes(payload[..4].try_into().unwrap())))
}

fn read_rid_and_size(payload: &[u8], kind: &str) -> Result<(Rid, i32), Error> {
    if payload.len() != 8 {
        return Err(Error::InvalidFrame(format!(
            "{kind} payload has {} bytes, expected 8",
            payload.len()
        )));
    }
    let rid = read_rid(payload)?;
    let size = i32::from_le_bytes(payload[4..8].try_into().unwrap());
    Ok((rid, size))
}
