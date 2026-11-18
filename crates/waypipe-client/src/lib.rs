mod client;
mod error;
mod header;
mod shm;
mod wire;

pub use client::{Client, ClientBatch, ClientConfig, ClientEvent};
pub use error::Error;
pub use header::{
    Compression, ConnectionHeader, VideoCodec, MIN_PROTOCOL_VERSION, WAYPIPE_PROTOCOL_VERSION,
};
pub use shm::{Rid, ShmStore, DEFAULT_MAX_SHM_FILE_SIZE};
pub use wire::{
    decode_rids, encode_close, encode_frame, encode_protocol, encode_rids, encode_version,
    parse_wayland_messages, FrameDecoder, MessageType, RawFrame, WaylandMessage,
    DEFAULT_MAX_FRAME_SIZE,
};

#[cfg(test)]
mod tests {
    #![allow(clippy::single_range_in_vec_init)]

    use std::ops::Range;

    use bytes::Bytes;

    use super::*;
    use crate::header::{server_header, HEADER_SIZE};

    fn get_registry() -> Bytes {
        Bytes::from_static(&[1, 0, 0, 0, 1, 0, 12, 0, 2, 0, 0, 0])
    }

    fn push_frame(output: &mut Vec<u8>, kind: MessageType, payload: &[u8]) {
        output.extend_from_slice(&encode_frame(kind, payload).unwrap());
    }

    fn negotiated_client() -> Client {
        let mut client = Client::default();
        let batch = client
            .receive(&server_header(Compression::None, true))
            .unwrap();
        assert_eq!(batch.outgoing, encode_version(WAYPIPE_PROTOCOL_VERSION));
        client
    }

    fn feed_byte_by_byte(client: &mut Client, bytes: &[u8]) -> Vec<ClientEvent> {
        let mut events = Vec::new();
        for byte in bytes {
            if client.is_closed() {
                break;
            }
            let batch = client.receive(std::slice::from_ref(byte)).unwrap();
            events.extend(batch.events);
        }
        events
    }

    #[test]
    fn negotiates_across_split_reads_and_decodes_protocol() {
        let header = server_header(Compression::None, true);
        let mut client = Client::default();

        let first = client.receive(&header[..5]).unwrap();
        assert!(first.events.is_empty());
        assert!(first.outgoing.is_empty());

        let mut input = header[5..].to_vec();
        push_frame(&mut input, MessageType::Protocol, &get_registry());
        let batch = client.receive(&input).unwrap();

        assert_eq!(batch.outgoing, encode_version(WAYPIPE_PROTOCOL_VERSION));
        assert_eq!(
            batch.events[0],
            ClientEvent::Negotiated {
                remote_version: WAYPIPE_PROTOCOL_VERSION,
                version: WAYPIPE_PROTOCOL_VERSION,
            }
        );
        let ClientEvent::Protocol(messages) = &batch.events[1] else {
            panic!("expected Protocol event");
        };
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].object_id(), 1);
        assert_eq!(messages[0].opcode(), 1);
        assert_eq!(messages[0].payload(), &[2, 0, 0, 0]);
    }

    #[test]
    fn replicates_shared_memory_fills_diffs_and_trailing_bytes() {
        let mut client = negotiated_client();
        let rid = Rid(7);
        let mut input = Vec::new();

        let mut open = Vec::new();
        open.extend_from_slice(&rid.0.to_le_bytes());
        open.extend_from_slice(&72i32.to_le_bytes());
        push_frame(&mut input, MessageType::OpenFile, &open);
        input.extend_from_slice(&encode_rids(&[rid]).unwrap());

        let mut fill = Vec::new();
        fill.extend_from_slice(&rid.0.to_le_bytes());
        fill.extend_from_slice(&0u32.to_le_bytes());
        fill.extend_from_slice(&12u32.to_le_bytes());
        fill.extend_from_slice(&(1u8..=12).collect::<Vec<_>>());
        push_frame(&mut input, MessageType::BufferFill, &fill);

        let mut diff = Vec::new();
        diff.extend_from_slice(&rid.0.to_le_bytes());
        diff.extend_from_slice(&12u32.to_le_bytes());
        diff.extend_from_slice(&0u32.to_le_bytes());
        diff.extend_from_slice(&1u32.to_le_bytes());
        diff.extend_from_slice(&2u32.to_le_bytes());
        diff.extend_from_slice(&[20, 21, 22, 23]);
        push_frame(&mut input, MessageType::BufferDiff, &diff);

        let mut trailing = Vec::new();
        trailing.extend_from_slice(&rid.0.to_le_bytes());
        trailing.extend_from_slice(&0u32.to_le_bytes());
        trailing.extend_from_slice(&4u32.to_le_bytes());
        trailing.extend_from_slice(&[90, 91, 92, 93]);
        push_frame(&mut input, MessageType::BufferDiff, &trailing);

        push_frame(&mut input, MessageType::Protocol, &get_registry());

        let mut extend = Vec::new();
        extend.extend_from_slice(&rid.0.to_le_bytes());
        extend.extend_from_slice(&80i32.to_le_bytes());
        push_frame(&mut input, MessageType::ExtendFile, &extend);
        push_frame(&mut input, MessageType::Close, &[]);

        let events = feed_byte_by_byte(&mut client, &input);
        assert_eq!(events[0], ClientEvent::ShmOpened { rid, size: 72 });
        assert_eq!(events[1], ClientEvent::RidsInjected(vec![rid]));
        assert_eq!(
            events[2],
            ClientEvent::ShmUpdated {
                rid,
                damage: vec![0..12],
            }
        );
        assert_eq!(
            events[3],
            ClientEvent::ShmUpdated {
                rid,
                damage: vec![4..8],
            }
        );
        assert_eq!(
            events[4],
            ClientEvent::ShmUpdated {
                rid,
                damage: vec![68..72],
            }
        );
        assert!(matches!(events[5], ClientEvent::Protocol(_)));
        assert_eq!(events[6], ClientEvent::ShmExtended { rid, size: 80 });
        assert_eq!(events[7], ClientEvent::Closed);

        let data = client.shm().data(rid).unwrap();
        assert_eq!(&data[..4], &[1, 2, 3, 4]);
        assert_eq!(&data[4..12], &[20, 21, 22, 23, 9, 10, 11, 12]);
        assert_eq!(&data[12..68], &[0; 56]);
        assert_eq!(&data[68..72], &[90, 91, 92, 93]);
        assert_eq!(&data[72..], &[0; 8]);
        assert_eq!(
            client.shm_mut().take_damage(rid),
            Some(vec![Range { start: 0, end: 12 }, 68..72])
        );
        assert_eq!(client.shm_mut().take_damage(rid), Some(Vec::new()));
        assert!(client.is_closed());
        assert_eq!(client.receive(&[]), Err(Error::ConnectionClosed));
    }

    #[test]
    fn encodes_outgoing_protocol_with_padding() {
        let client = negotiated_client();
        let mut messages = get_registry().to_vec();
        messages.extend_from_slice(&[1, 0, 0, 0, 0, 0, 12, 0, 3, 0, 0, 0]);

        let encoded = client.encode_protocol(Bytes::from(messages)).unwrap();
        assert_eq!(encoded.len() % 4, 0);

        let mut decoder = FrameDecoder::new(DEFAULT_MAX_FRAME_SIZE);
        decoder.push(&encoded);
        let frame = decoder.next_frame().unwrap().unwrap();
        assert_eq!(frame.kind, MessageType::Protocol);
        let decoded = parse_wayland_messages(frame.payload).unwrap();
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[1].opcode(), 0);
        assert_eq!(decoded[1].payload(), &[3, 0, 0, 0]);
        assert!(decoder.next_frame().unwrap().is_none());
    }

    #[test]
    fn rejects_invalid_handshakes_and_unsupported_frames() {
        let mut client = Client::default();
        assert_eq!(
            client.receive(&server_header(Compression::Lz4, true)),
            Err(Error::CompressionMismatch {
                expected: Compression::None,
                actual: Compression::Lz4,
            })
        );

        let mut client = Client::default();
        assert_eq!(
            client.receive(&server_header(Compression::None, false)),
            Err(Error::DmabufUnsupported)
        );

        let mut header = server_header(Compression::None, true);
        header[3] |= 0x80;
        let mut client = Client::default();
        assert!(matches!(
            client.receive(&header),
            Err(Error::InvalidHeader(_))
        ));

        let mut client = negotiated_client();
        let mut input = Vec::new();
        let mut open = Vec::new();
        open.extend_from_slice(&1i32.to_le_bytes());
        open.extend_from_slice(&8i32.to_le_bytes());
        push_frame(&mut input, MessageType::OpenFile, &open);
        push_frame(&mut input, MessageType::OpenIrPipe, &open[..4]);
        input.extend_from_slice(&encode_version(WAYPIPE_PROTOCOL_VERSION));

        assert!(client.receive(&input[..12]).is_ok());
        assert_eq!(
            client.receive(&input[12..20]),
            Err(Error::UnsupportedFrame(MessageType::OpenIrPipe))
        );
        assert!(matches!(
            client.receive(&input[20..]),
            Err(Error::InvalidFrame(_))
        ));
    }

    #[test]
    fn rejects_malformed_updates_without_panicking() {
        let mut client = negotiated_client();
        let rid = Rid(1);
        client.shm_mut().open_file(rid, 64).unwrap();

        let mut fill = Vec::new();
        fill.extend_from_slice(&rid.0.to_le_bytes());
        fill.extend_from_slice(&0u32.to_le_bytes());
        fill.extend_from_slice(&64u32.to_le_bytes());
        fill.extend_from_slice(&[0u8; 4]);
        assert!(matches!(
            client.receive(&encode_frame(MessageType::BufferFill, &fill).unwrap()),
            Err(Error::InvalidFrame(_))
        ));

        let mut diff = Vec::new();
        diff.extend_from_slice(&rid.0.to_le_bytes());
        diff.extend_from_slice(&12u32.to_le_bytes());
        diff.extend_from_slice(&0u32.to_le_bytes());
        diff.extend_from_slice(&0u32.to_le_bytes());
        diff.extend_from_slice(&100u32.to_le_bytes());
        diff.extend_from_slice(&[0u8; 4]);
        assert!(matches!(
            client.receive(&encode_frame(MessageType::BufferDiff, &diff).unwrap()),
            Err(Error::InvalidFrame(_))
        ));

        let invalid_wayland = [1, 0, 0, 0, 1, 0, 9, 0, 2, 0, 0, 0];
        assert!(matches!(
            client.receive(&encode_frame(MessageType::Protocol, &invalid_wayland).unwrap()),
            Err(Error::InvalidWaylandMessage(_))
        ));
    }

    #[test]
    fn enforces_size_limits_and_rid_invariants() {
        let mut store = ShmStore::new(64);
        assert!(matches!(
            store.open_file(Rid(1), 65),
            Err(Error::InvalidFrame(_))
        ));
        assert!(matches!(
            store.open_file(Rid(1), -1),
            Err(Error::InvalidFrame(_))
        ));

        let rid = Rid(1);
        store.open_file(rid, 64).unwrap();
        assert_eq!(store.open_file(rid, 64), Err(Error::DuplicateRid(rid)));
        store.inject(rid).unwrap();
        assert!(matches!(store.inject(rid), Err(Error::InvalidFrame(_))));
        assert!(matches!(
            store.extend_file(rid, 32),
            Err(Error::InvalidFrame(_))
        ));
        assert_eq!(
            store.apply_diff(Rid(2), &[0; 8]),
            Err(Error::UnknownRid(Rid(2)))
        );

        let mut decoder = FrameDecoder::new(16);
        decoder.push(&encode_frame(MessageType::BufferFill, &[0; 16]).unwrap());
        assert_eq!(decoder.next_frame(), Err(Error::FrameTooLarge(20)));
    }

    #[test]
    fn exposes_negotiated_header() {
        let client = negotiated_client();
        let header = client.header().unwrap();
        assert_eq!(HEADER_SIZE, 16);
        assert_eq!(header.version, WAYPIPE_PROTOCOL_VERSION);
        assert_eq!(header.compression, Compression::None);
        assert!(!header.has_dmabuf);
    }
}
