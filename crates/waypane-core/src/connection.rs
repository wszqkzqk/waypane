use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use bytes::{Bytes, BytesMut};
use waypipe_client::{Client, ClientConfig, ClientEvent, Rid};

use crate::compositor::Compositor;
use crate::error::{Error, Result};
use crate::types::{CoreEvent, FrameId, DEFAULT_KEYMAP};

const MAX_PENDING_FRAME_BYTES: usize = 256 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct CoreBatch {
    pub outgoing: Bytes,
    pub events: Vec<CoreEvent>,
}

pub struct Connection {
    client: Client,
    compositor: Compositor,
    rids: VecDeque<Rid>,
}

impl Connection {
    pub fn new(config: ClientConfig) -> Self {
        Self::with_keymap(config, Arc::from(DEFAULT_KEYMAP))
    }

    pub fn with_keymap(config: ClientConfig, keymap: Arc<[u8]>) -> Self {
        Self {
            client: Client::new(config),
            compositor: Compositor::new(keymap),
            rids: VecDeque::new(),
        }
    }

    pub fn receive(&mut self, input: &[u8]) -> Result<CoreBatch> {
        let batch = self.client.receive(input)?;
        let mut outgoing = BytesMut::from(batch.outgoing.as_ref());
        let mut events = Vec::new();
        let mut allocations = BTreeMap::new();
        for event in batch.events {
            match event {
                ClientEvent::Negotiated {
                    remote_version,
                    version,
                } => events.push(CoreEvent::Negotiated {
                    remote_version,
                    version,
                }),
                ClientEvent::ShmUpdated { .. } => {}
                ClientEvent::ShmOpened { .. } | ClientEvent::ShmExtended { .. } => {}
                ClientEvent::PipeOpened { rid, .. } | ClientEvent::PipeUpdated { rid } => {
                    events.extend(self.compositor.process_pipe(&mut self.client, rid)?);
                }
                ClientEvent::RidsInjected(rids) => self.rids.extend(rids),
                ClientEvent::Protocol(messages) => {
                    self.compositor
                        .process(&mut self.client, &messages, &mut self.rids)?;
                    drain_events(&mut self.compositor, &mut events, &mut allocations)?;
                }
                ClientEvent::Closed => events.push(CoreEvent::Closed),
            }
        }
        if !self.rids.is_empty() {
            return Err(Error::protocol(format!(
                "{} injected RIDs were not consumed by Wayland messages",
                self.rids.len()
            )));
        }
        self.client.resources_mut().clear_damage();
        drain_events(&mut self.compositor, &mut events, &mut allocations)?;
        outgoing.extend_from_slice(&self.flush()?);
        Ok(CoreBatch {
            outgoing: outgoing.freeze(),
            events,
        })
    }

    pub fn request_clipboard_text(&mut self) -> Result<Bytes> {
        self.compositor.request_clipboard_text(&mut self.client)?;
        self.flush()
    }

    pub fn complete_frame(&mut self, frame_id: FrameId, time: u32) -> Result<Bytes> {
        self.compositor
            .complete_frame(&mut self.client, frame_id, time)?;
        self.flush()
    }

    pub fn flush(&mut self) -> Result<Bytes> {
        if self.client.is_closed() {
            return Ok(Bytes::new());
        }
        let messages = self.compositor.take_outgoing();
        self.client.encode_messages(&messages).map_err(Error::from)
    }

    pub fn close(&mut self) -> Result<Bytes> {
        let _ = self.flush()?;
        self.client.close_message().map_err(Error::from)
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    pub fn client_mut(&mut self) -> &mut Client {
        &mut self.client
    }

    pub fn compositor(&self) -> &Compositor {
        &self.compositor
    }

    pub fn compositor_mut(&mut self) -> &mut Compositor {
        &mut self.compositor
    }
}

fn drain_events(
    compositor: &mut Compositor,
    events: &mut Vec<CoreEvent>,
    allocations: &mut BTreeMap<usize, usize>,
) -> Result<()> {
    for event in compositor.take_events() {
        if let CoreEvent::Frame(frame) = &event {
            if let Some(buffer) = &frame.buffer {
                allocations
                    .entry(buffer.data.allocation_id())
                    .or_insert(buffer.data.len());
            }
        }
        events.push(event);
    }
    let total = allocations
        .values()
        .try_fold(0usize, |total, size| total.checked_add(*size))
        .ok_or_else(|| Error::protocol("pending frame snapshot size overflow"))?;
    if total > MAX_PENDING_FRAME_BYTES {
        return Err(Error::protocol(format!(
            "pending frame snapshots require {total} bytes, exceeding limit {MAX_PENDING_FRAME_BYTES}"
        )));
    }
    Ok(())
}
