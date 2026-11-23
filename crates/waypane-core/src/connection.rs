use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use bytes::{Bytes, BytesMut};
use waypipe_client::{Client, ClientConfig, ClientEvent, Rid};

use crate::compositor::Compositor;
use crate::error::{Error, Result};
use crate::types::{CoreEvent, FrameId, DEFAULT_KEYMAP};

const MAX_PENDING_FRAME_BYTES: usize = 256 * 1024 * 1024;
const MAX_PENDING_INJECTED_RIDS: usize = 1024;

#[derive(Debug, Clone)]
pub struct CoreBatch {
    pub outgoing: Bytes,
    pub events: Vec<CoreEvent>,
}

pub struct Connection {
    client: Client,
    compositor: Compositor,
    // Fd arguments await their Protocol frame: upstream can flush InjectRIDs before protocol
    // bytes are ready, and stream reads need not end at frame boundaries. Consumed in order.
    rids: VecDeque<Rid>,
}

impl Connection {
    pub fn new(config: ClientConfig) -> Self {
        Self::with_keymap(config, Arc::from(DEFAULT_KEYMAP))
    }

    pub fn with_keymap(config: ClientConfig, keymap: Arc<[u8]>) -> Self {
        Self::with_keymap_and_output_scale(config, keymap, 1)
    }

    pub fn with_output_scale(config: ClientConfig, scale: i32) -> Self {
        Self::with_keymap_and_output_scale(config, Arc::from(DEFAULT_KEYMAP), scale)
    }

    pub fn with_keymap_and_output_scale(
        config: ClientConfig,
        keymap: Arc<[u8]>,
        scale: i32,
    ) -> Self {
        Self {
            client: Client::new(config),
            compositor: Compositor::with_output_scale(keymap, scale),
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
                ClientEvent::RidsInjected(rids) => {
                    if rids.len() > MAX_PENDING_INJECTED_RIDS.saturating_sub(self.rids.len()) {
                        return Err(Error::protocol(format!(
                            "more than {MAX_PENDING_INJECTED_RIDS} injected RIDs await protocol messages"
                        )));
                    }
                    self.rids.extend(rids);
                }
                ClientEvent::Protocol(messages) => {
                    self.compositor
                        .process(&mut self.client, &messages, &mut self.rids)?;
                    drain_events(&mut self.compositor, &mut events, &mut allocations)?;
                }
                ClientEvent::Closed => events.push(CoreEvent::Closed),
            }
        }
        if self.compositor.has_waiting_requests() {
            // File-only traffic can unblock requests queued by an earlier protocol frame.
            self.compositor
                .process(&mut self.client, &[], &mut self.rids)?;
            drain_events(&mut self.compositor, &mut events, &mut allocations)?;
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
        self.compositor.complete_frame(frame_id, time)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compositor::MAX_WAITING_REQUESTS;
    use crate::protocol::wayland as wl;
    use crate::state::ObjectRecord;
    use waypipe_client::{
        encode_frame, encode_protocol_messages, encode_rids, Compression, MessageType,
        ProtocolMessage, WAYPIPE_PROTOCOL_VERSION,
    };

    fn header() -> [u8; 16] {
        let mut bytes = [0u8; 16];
        let version = WAYPIPE_PROTOCOL_VERSION;
        let lead = ((version >> 4) << 16)
            | ((version & 0xf) << 3)
            | (1 << 7)
            | (1 << 2)
            | (1 << 8)
            | (1 << 11);
        bytes[..4].copy_from_slice(&lead.to_le_bytes());
        bytes
    }

    fn connection() -> Connection {
        let mut connection = Connection::new(ClientConfig::default());
        connection.receive(&header()).unwrap();
        connection
            .compositor
            .insert_test_record(2, ObjectRecord::new(&wl::WL_SHM_INTERFACE, 1))
            .unwrap();
        connection
    }

    #[test]
    fn injected_rid_can_precede_its_protocol_frame_across_receives() {
        let mut connection = connection();
        let rid = Rid(7);
        prepare_file(&mut connection, rid);
        connection
            .receive(&encode_rids(&[rid]).unwrap())
            .expect("injected RID waits for protocol");
        assert_eq!(connection.rids, [rid]);

        let (protocol, rids) = encode_protocol_messages(&[create_pool(100, rid)]).unwrap();
        assert_eq!(rids, [rid]);
        connection.receive(&protocol).unwrap();
        assert!(connection.rids.is_empty());
        assert_eq!(connection.compositor.pool_rid(100), Some(rid));
    }

    fn unfilled_file(connection: &mut Connection, rid: Rid) {
        connection
            .client_mut()
            .resources_mut()
            .open_file(rid, 4)
            .unwrap();
    }

    fn prepare_file(connection: &mut Connection, rid: Rid) {
        unfilled_file(connection, rid);
        fill_remote_file(connection, rid, &[0x5a; 4]);
    }

    fn create_pool(new_pool: u32, rid: Rid) -> ProtocolMessage {
        let mut raw = Vec::new();
        raw.extend_from_slice(&2_u32.to_le_bytes());
        raw.extend_from_slice(&(16_u32 << 16).to_le_bytes());
        raw.extend_from_slice(&new_pool.to_le_bytes());
        raw.extend_from_slice(&4_i32.to_le_bytes());
        ProtocolMessage::with_fds(Bytes::from(raw), vec![rid]).unwrap()
    }

    #[test]
    fn separated_injections_are_consumed_in_fd_order_by_later_protocol() {
        let mut connection = connection();
        prepare_file(&mut connection, Rid(8));
        connection
            .receive(&encode_rids(&[Rid(8)]).unwrap())
            .unwrap();

        prepare_file(&mut connection, Rid(9));
        let inject_second = encode_rids(&[Rid(9)]).unwrap();
        let split = inject_second.len() - 2;
        connection.receive(&inject_second[..split]).unwrap();
        connection.receive(&inject_second[split..]).unwrap();
        assert_eq!(connection.rids, [Rid(8), Rid(9)]);

        let (protocol, _) =
            encode_protocol_messages(&[create_pool(101, Rid(8)), create_pool(102, Rid(9))])
                .unwrap();
        connection.receive(&protocol).unwrap();
        assert!(
            connection.rids.is_empty(),
            "leftover RIDs: {:?}",
            connection.rids
        );
        assert_eq!(connection.compositor.pool_rid(101), Some(Rid(8)));
        assert_eq!(connection.compositor.pool_rid(102), Some(Rid(9)));
    }

    #[test]
    fn outstanding_injected_rids_are_bounded() {
        let mut connection = connection();
        for index in 0..MAX_PENDING_INJECTED_RIDS {
            let rid = Rid((index + 1000) as i32);
            prepare_file(&mut connection, rid);
            connection.receive(&encode_rids(&[rid]).unwrap()).unwrap();
        }
        let rid = Rid(5000);
        prepare_file(&mut connection, rid);
        let error = connection
            .receive(&encode_rids(&[rid]).unwrap())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(&format!("{MAX_PENDING_INJECTED_RIDS} injected RIDs")),
            "{error}"
        );
    }

    fn fill_remote_file(connection: &mut Connection, rid: Rid, bytes: &[u8]) {
        let mut payload = 0_u32.to_le_bytes().to_vec();
        payload.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        payload.extend_from_slice(bytes);
        connection
            .client_mut()
            .resources_mut()
            .apply_fill(rid, &payload, Compression::None)
            .unwrap();
    }

    fn create_pool_raw(new_pool: u32) -> Vec<u8> {
        let mut raw = Vec::new();
        raw.extend_from_slice(&2_u32.to_le_bytes());
        raw.extend_from_slice(&(16_u32 << 16).to_le_bytes());
        raw.extend_from_slice(&new_pool.to_le_bytes());
        raw.extend_from_slice(&4_i32.to_le_bytes());
        raw
    }

    fn sync_request(new_callback: u32) -> ProtocolMessage {
        let mut raw = Vec::new();
        raw.extend_from_slice(&1_u32.to_le_bytes());
        raw.extend_from_slice(&(12_u32 << 16).to_le_bytes());
        raw.extend_from_slice(&new_callback.to_le_bytes());
        ProtocolMessage::new(Bytes::from(raw)).unwrap()
    }

    #[test]
    fn protocol_waits_for_shm_content_in_order_across_receives() {
        let mut connection = connection();
        let (slow, ready) = (Rid(7), Rid(8));
        unfilled_file(&mut connection, slow);
        connection.receive(&encode_rids(&[slow]).unwrap()).unwrap();
        prepare_file(&mut connection, ready);
        fill_remote_file(&mut connection, ready, &[1, 2, 3, 4]);
        connection.receive(&encode_rids(&[ready]).unwrap()).unwrap();

        let protocol = [create_pool(100, slow), create_pool(101, ready)]
            .into_iter()
            .chain([sync_request(200)]);
        let (frames, _) = encode_protocol_messages(&protocol.collect::<Vec<_>>()).unwrap();
        connection.receive(&frames).unwrap();

        assert_eq!(connection.compositor.pool_rid(100), None);
        assert_eq!(
            connection.compositor.pool_rid(101),
            None,
            "a ready request must not overtake earlier unrelated shared memory"
        );
        assert_eq!(connection.rids, [slow, ready]);

        let mut payload = slow.0.to_le_bytes().to_vec();
        payload.extend_from_slice(&0_u32.to_le_bytes());
        payload.extend_from_slice(&4_u32.to_le_bytes());
        payload.extend_from_slice(&[9, 8, 7, 6]);
        let frame = encode_frame(MessageType::BufferFill, &payload).unwrap();
        let split = frame.len() - 3;
        connection.receive(&frame[..split]).unwrap();
        assert_eq!(connection.compositor.pool_rid(100), None);
        connection.receive(&frame[split..]).unwrap();

        assert_eq!(connection.compositor.pool_rid(100), Some(slow));
        assert_eq!(connection.compositor.pool_rid(101), Some(ready));
        assert!(connection.rids.is_empty());
        let store = connection.client().resources();
        assert_eq!(store.data(slow).unwrap().to_vec(), vec![9, 8, 7, 6]);
        assert_eq!(store.data(ready).unwrap().to_vec(), vec![1, 2, 3, 4]);
    }

    #[test]
    fn malformed_fd_requests_fail_instead_of_waiting() {
        let mut missing = connection();
        let (protocol, _) = encode_protocol_messages(&[create_pool(100, Rid(7))]).unwrap();
        let error = missing.receive(&protocol).unwrap_err().to_string();
        assert!(error.contains("missing RID"), "{error}");

        let mut blocked = connection();
        prepare_file(&mut blocked, Rid(7));
        blocked.receive(&encode_rids(&[Rid(7)]).unwrap()).unwrap();
        let untagged = ProtocolMessage::new(Bytes::from(create_pool_raw(100))).unwrap();
        let (protocol, _) = encode_protocol_messages(&[untagged]).unwrap();
        let error = blocked.receive(&protocol).unwrap_err().to_string();
        assert!(error.contains("no FD tag"), "{error}");
    }

    #[test]
    fn requests_waiting_for_shm_are_bounded() {
        let mut connection = connection();
        unfilled_file(&mut connection, Rid(7));
        connection
            .receive(&encode_rids(&[Rid(7)]).unwrap())
            .unwrap();
        let mut batch = vec![create_pool(100, Rid(7))];
        batch.extend((1000..).take(MAX_WAITING_REQUESTS).map(sync_request));
        let (frames, _) = encode_protocol_messages(&batch).unwrap();
        let error = connection.receive(&frames).unwrap_err().to_string();
        assert!(error.contains("too many Wayland requests"), "{error}");
    }
}
