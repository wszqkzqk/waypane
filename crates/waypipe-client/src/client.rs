use std::collections::VecDeque;
use std::ops::Range;
use std::sync::Arc;

use bytes::{Bytes, BytesMut};

use crate::compression::{compress, DEFAULT_MAX_SHM_TRANSFER_SIZE};
use crate::error::Error;
use crate::header::{Compression, ConnectionHeader, HEADER_SIZE, MIN_PROTOCOL_VERSION};
use crate::store::{
    PipeAccess, PipeOpenKind, ResourceStore, Rid, DEFAULT_MAX_SHM_FILE_SIZE,
    DEFAULT_MAX_TOTAL_SHM_SIZE,
};
use crate::wire::{
    decode_rids, encode_close, encode_frame, encode_protocol_messages, encode_rids, encode_version,
    FrameDecoder, MessageType, ProtocolMessage, RawFrame, WaylandMessage, DEFAULT_MAX_FRAME_SIZE,
};

const DIFF_CHUNK_SIZE: usize = 262_144;

#[derive(Debug, Clone, Copy)]
pub struct ClientConfig {
    pub preferred_compression: Compression,
    pub max_frame_size: usize,
    pub max_shm_file_size: usize,
    pub max_shm_transfer_size: usize,
    pub max_total_shm_size: usize,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            preferred_compression: Compression::None,
            max_frame_size: DEFAULT_MAX_FRAME_SIZE,
            max_shm_file_size: DEFAULT_MAX_SHM_FILE_SIZE,
            max_shm_transfer_size: DEFAULT_MAX_SHM_TRANSFER_SIZE,
            max_total_shm_size: DEFAULT_MAX_TOTAL_SHM_SIZE,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientEvent {
    Negotiated { remote_version: u32, version: u32 },
    ShmOpened { rid: Rid, size: usize },
    ShmExtended { rid: Rid, size: usize },
    ShmUpdated { rid: Rid, damage: Vec<Range<usize>> },
    PipeOpened { rid: Rid, access: PipeAccess },
    PipeUpdated { rid: Rid },
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
    store: ResourceStore,
    pending_frames: VecDeque<Bytes>,
    preferred_compression: Compression,
    next_local_rid: i32,
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
            store: ResourceStore::new(
                config.max_shm_file_size,
                config.max_shm_transfer_size,
                config.max_total_shm_size,
            ),
            pending_frames: VecDeque::new(),
            preferred_compression: config.preferred_compression,
            next_local_rid: -1,
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
            if header.compression != self.preferred_compression {
                return Err(Error::CompressionMismatch {
                    expected: self.preferred_compression,
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

    pub fn create_readonly_file(&mut self, data: Arc<[u8]>) -> Result<Rid, Error> {
        self.ensure_writable()?;
        let rid = self.allocate_local_rid()?;
        self.store.create_local_readonly_file(rid, data)?;
        Ok(rid)
    }

    pub fn create_pipe(&mut self, kind: PipeOpenKind) -> Result<Rid, Error> {
        self.ensure_writable()?;
        let rid = self.allocate_local_rid()?;
        self.store.create_local_pipe(rid, kind)?;
        Ok(rid)
    }

    pub fn write_pipe(&mut self, rid: Rid, data: &[u8]) -> Result<(), Error> {
        self.ensure_writable()?;
        self.store.validate_pipe_write(rid, data.len())?;
        let mut payload = Vec::with_capacity(4 + data.len());
        payload.extend_from_slice(&rid.0.to_le_bytes());
        payload.extend_from_slice(data);
        self.pending_frames
            .push_back(encode_frame(MessageType::PipeTransfer, &payload)?);
        Ok(())
    }

    pub fn close_pipe_read(&mut self, rid: Rid) -> Result<(), Error> {
        self.ensure_writable()?;
        self.store.close_local_pipe_read(rid)?;
        self.pending_frames.push_back(encode_frame(
            MessageType::PipeShutdownR,
            &rid.0.to_le_bytes(),
        )?);
        Ok(())
    }

    pub fn close_pipe_write(&mut self, rid: Rid) -> Result<(), Error> {
        self.ensure_writable()?;
        self.store.close_local_pipe_write(rid)?;
        self.pending_frames.push_back(encode_frame(
            MessageType::PipeShutdownW,
            &rid.0.to_le_bytes(),
        )?);
        Ok(())
    }

    pub fn encode_protocol(&mut self, messages: Bytes) -> Result<Bytes, Error> {
        let messages = crate::wire::parse_wayland_messages(messages)?
            .into_iter()
            .map(|message| ProtocolMessage::new(message.into_bytes()))
            .collect::<Result<Vec<_>, _>>()?;
        self.encode_messages(&messages)
    }

    pub fn encode_protocol_with_rids(
        &mut self,
        messages: Bytes,
        rids: &[Rid],
    ) -> Result<Bytes, Error> {
        if rids.is_empty() {
            return self.encode_protocol(messages);
        }
        if messages.is_empty() {
            return Err(Error::InvalidWaylandMessage(
                "cannot attach RIDs to an empty message".to_string(),
            ));
        }
        let message = ProtocolMessage::with_fds(messages, rids.to_vec())?;
        self.encode_messages(&[message])
    }

    pub fn encode_messages(&mut self, messages: &[ProtocolMessage]) -> Result<Bytes, Error> {
        self.ensure_writable()?;
        let (protocol, rids) = if messages.is_empty() {
            (Bytes::new(), Vec::new())
        } else {
            encode_protocol_messages(messages)?
        };
        for rid in &rids {
            self.store.inject_local(*rid)?;
        }

        let mut outgoing = self.take_pending_resource_frames()?;
        if !rids.is_empty() {
            outgoing.extend_from_slice(&encode_rids(&rids)?);
        }
        outgoing.extend_from_slice(&protocol);
        Ok(outgoing.freeze())
    }

    pub fn flush_resources(&mut self) -> Result<Bytes, Error> {
        self.ensure_writable()?;
        Ok(self.take_pending_resource_frames()?.freeze())
    }

    pub fn close_message(&mut self) -> Result<Bytes, Error> {
        let mut outgoing = self.take_pending_resource_frames()?;
        outgoing.extend_from_slice(&encode_close());
        self.closed = true;
        Ok(outgoing.freeze())
    }

    pub fn header(&self) -> Option<&ConnectionHeader> {
        match &self.stage {
            Stage::AwaitHeader { .. } => None,
            Stage::Ready(header) => Some(header),
        }
    }

    pub fn resources(&self) -> &ResourceStore {
        &self.store
    }

    pub fn resources_mut(&mut self) -> &mut ResourceStore {
        &mut self.store
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    fn take_pending_resource_frames(&mut self) -> Result<BytesMut, Error> {
        let compression = self.compression()?;
        let mut outgoing = BytesMut::new();

        for (rid, data) in self.store.take_local_files() {
            let mut open = Vec::with_capacity(8);
            open.extend_from_slice(&rid.0.to_le_bytes());
            open.extend_from_slice(&(data.len() as u32).to_le_bytes());
            outgoing.extend_from_slice(&encode_frame(MessageType::OpenFile, &open)?);

            for (index, chunk) in data.chunks(DIFF_CHUNK_SIZE).enumerate() {
                let start = index * DIFF_CHUNK_SIZE;
                let end = start + chunk.len();
                let encoded = compress(compression, chunk)?;
                let mut fill = Vec::with_capacity(12 + encoded.len());
                fill.extend_from_slice(&rid.0.to_le_bytes());
                fill.extend_from_slice(&(start as u32).to_le_bytes());
                fill.extend_from_slice(&(end as u32).to_le_bytes());
                fill.extend_from_slice(&encoded);
                outgoing.extend_from_slice(&encode_frame(MessageType::BufferFill, &fill)?);
            }
        }

        for (rid, kind) in self.store.take_local_pipe_openings() {
            outgoing.extend_from_slice(&encode_frame(kind.message_type(), &rid.0.to_le_bytes())?);
        }
        while let Some(frame) = self.pending_frames.pop_front() {
            outgoing.extend_from_slice(&frame);
        }
        Ok(outgoing)
    }

    fn compression(&self) -> Result<Compression, Error> {
        match &self.stage {
            Stage::Ready(header) => Ok(header.compression),
            Stage::AwaitHeader { .. } => Err(Error::InvalidFrame(
                "connection negotiation has not completed".to_string(),
            )),
        }
    }

    fn ensure_writable(&self) -> Result<(), Error> {
        if self.closed {
            return Err(Error::ConnectionClosed);
        }
        self.compression().map(|_| ())
    }

    fn allocate_local_rid(&mut self) -> Result<Rid, Error> {
        let rid = Rid(self.next_local_rid);
        self.next_local_rid = self
            .next_local_rid
            .checked_sub(1)
            .ok_or_else(|| Error::InvalidFrame("local RID space exhausted".to_string()))?;
        Ok(rid)
    }

    fn process_frame(
        &mut self,
        frame: RawFrame,
        events: &mut Vec<ClientEvent>,
    ) -> Result<(), Error> {
        let payload = frame.payload;
        let compression = self.compression()?;
        match frame.kind {
            MessageType::Protocol => {
                let messages = crate::wire::parse_wayland_messages(payload)?;
                if !messages.is_empty() {
                    events.push(ClientEvent::Protocol(messages));
                }
            }
            MessageType::InjectRids => {
                let rids = decode_rids(&payload)?;
                for rid in &rids {
                    self.store.inject_remote(*rid)?;
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
                    self.store.apply_fill(rid, &payload[4..], compression)?
                } else {
                    self.store.apply_diff(rid, &payload[4..], compression)?
                };
                if !damage.is_empty() {
                    events.push(ClientEvent::ShmUpdated { rid, damage });
                }
            }
            MessageType::OpenIrPipe | MessageType::OpenIwPipe | MessageType::OpenRwPipe => {
                let rid = read_rid(&payload)?;
                if payload.len() != 4 {
                    return Err(Error::InvalidFrame(format!(
                        "pipe creation payload has {} bytes, expected 4",
                        payload.len()
                    )));
                }
                let kind = match frame.kind {
                    MessageType::OpenIrPipe => PipeOpenKind::IR,
                    MessageType::OpenIwPipe => PipeOpenKind::IW,
                    MessageType::OpenRwPipe => PipeOpenKind::RW,
                    _ => unreachable!(),
                };
                self.store.open_remote_pipe(rid, kind)?;
                events.push(ClientEvent::PipeOpened {
                    rid,
                    access: self.store.pipe_access(rid).unwrap(),
                });
            }
            MessageType::PipeTransfer => {
                let rid = read_rid(&payload)?;
                self.store.receive_pipe_data(rid, &payload[4..])?;
                events.push(ClientEvent::PipeUpdated { rid });
            }
            MessageType::PipeShutdownR | MessageType::PipeShutdownW => {
                let rid = read_rid(&payload)?;
                if payload.len() != 4 {
                    return Err(Error::InvalidFrame(format!(
                        "pipe shutdown payload has {} bytes, expected 4",
                        payload.len()
                    )));
                }
                self.store.receive_pipe_shutdown(rid, frame.kind)?;
                events.push(ClientEvent::PipeUpdated { rid });
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
