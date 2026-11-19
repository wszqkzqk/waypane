mod client;
mod compression;
mod error;
mod header;
mod store;
mod wire;

pub use client::{Client, ClientBatch, ClientConfig, ClientEvent};
pub use compression::DEFAULT_MAX_SHM_TRANSFER_SIZE;
pub use error::Error;
pub use header::{
    Compression, ConnectionHeader, VideoCodec, MIN_PROTOCOL_VERSION, WAYPIPE_PROTOCOL_VERSION,
};
pub use store::{
    PipeAccess, PipeOpenKind, PipeRead, ResourceStore, Rid, ShmData, DEFAULT_MAX_PIPE_BUFFER_SIZE,
    DEFAULT_MAX_SHM_FILE_SIZE, DEFAULT_MAX_TOTAL_SHM_SIZE,
};
pub use wire::{
    decode_rids, encode_close, encode_frame, encode_protocol, encode_protocol_messages,
    encode_rids, encode_version, parse_wayland_messages, FrameDecoder, MessageType,
    ProtocolMessage, RawFrame, WaylandMessage, DEFAULT_MAX_FRAME_SIZE,
};

#[cfg(test)]
mod tests {
    #![allow(clippy::single_range_in_vec_init)]

    use std::ops::Range;
    use std::sync::Arc;

    use bytes::Bytes;

    use super::*;
    use crate::header::{server_header, server_header_at, HEADER_SIZE};

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
    fn handles_waypipe_c_ack_nblocks_by_negotiated_protocol_version() {
        let header = server_header_at(MIN_PROTOCOL_VERSION, Compression::None, true);
        assert_eq!(u32::from_le_bytes(header[..4].try_into().unwrap()) >> 16, 1);
        let mut client = Client::default();
        let mut input = header.to_vec();
        push_frame(&mut input, MessageType::AckNblocks, &7u32.to_le_bytes());
        push_frame(&mut input, MessageType::Protocol, &get_registry());
        let batch = client.receive(&input).unwrap();
        assert!(batch.outgoing.is_empty());
        assert_eq!(
            batch.events[0],
            ClientEvent::Negotiated {
                remote_version: MIN_PROTOCOL_VERSION,
                version: MIN_PROTOCOL_VERSION,
            }
        );
        assert!(matches!(batch.events[1], ClientEvent::Protocol(_)));
        assert_eq!(batch.events.len(), 2);

        let mut client = Client::default();
        let mut malformed = header.to_vec();
        push_frame(&mut malformed, MessageType::AckNblocks, &[1, 2, 3]);
        let error = client.receive(&malformed).unwrap_err().to_string();
        assert!(error.contains("expected 4 at protocol 0x10"), "{error}");

        let mut client = negotiated_client();
        let error = client
            .receive(&encode_frame(MessageType::AckNblocks, &7u32.to_le_bytes()).unwrap())
            .unwrap_err()
            .to_string();
        assert!(error.contains("negotiated protocol 0x11"), "{error}");
        assert!(error.contains("server advertised 0x11"), "{error}");
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

        let data = client.resources().data(rid).unwrap();
        assert_eq!(&data[..4], &[1, 2, 3, 4]);
        assert_eq!(&data[4..12], &[20, 21, 22, 23, 9, 10, 11, 12]);
        assert_eq!(&data[12..68], &[0; 56]);
        assert_eq!(&data[68..72], &[90, 91, 92, 93]);
        assert_eq!(&data[72..], &[0; 8]);
        assert_eq!(
            client.resources_mut().take_damage(rid),
            Some(vec![Range { start: 0, end: 12 }, 68..72])
        );
        assert_eq!(client.resources_mut().take_damage(rid), Some(Vec::new()));
        assert!(client.is_closed());
        assert_eq!(client.receive(&[]), Err(Error::ConnectionClosed));
    }

    #[test]
    fn encodes_outgoing_protocol_with_padding() {
        let mut client = negotiated_client();
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
        push_frame(&mut input, MessageType::OpenIrPipe, &2i32.to_le_bytes());
        input.extend_from_slice(&encode_version(WAYPIPE_PROTOCOL_VERSION));

        assert!(client.receive(&input[..12]).is_ok());
        let opened = client.receive(&input[12..20]).unwrap();
        assert!(matches!(
            &opened.events[..],
            [ClientEvent::PipeOpened { rid: Rid(2), access }]
                if access.can_write && !access.can_read
        ));
        assert!(matches!(
            client.receive(&input[20..]),
            Err(Error::InvalidFrame(_))
        ));
    }

    #[test]
    fn rejects_malformed_updates_without_panicking() {
        let mut client = negotiated_client();
        let rid = Rid(1);
        client.resources_mut().open_file(rid, 64).unwrap();

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
        let mut store = ResourceStore::new(64, 16, 128);
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
        store.inject_remote(rid).unwrap();
        assert!(matches!(
            store.inject_remote(rid),
            Err(Error::InvalidFrame(_))
        ));
        assert!(matches!(
            store.extend_file(rid, 32),
            Err(Error::InvalidFrame(_))
        ));
        assert_eq!(
            store.apply_diff(Rid(2), &[0; 8], Compression::None),
            Err(Error::UnknownRid(Rid(2)))
        );

        let mut decoder = FrameDecoder::new(16);
        decoder.push(&encode_frame(MessageType::BufferFill, &[0; 16]).unwrap());
        assert_eq!(decoder.next_frame(), Err(Error::FrameTooLarge(20)));
    }

    #[test]
    fn outgoing_files_and_pipes_round_trip_in_wire_order() {
        for compression in [Compression::None, Compression::Lz4, Compression::Zstd] {
            let config = ClientConfig {
                preferred_compression: compression,
                ..ClientConfig::default()
            };
            let mut sender = Client::new(config);
            sender.receive(&server_header(compression, true)).unwrap();
            let mut receiver = Client::new(config);
            receiver.receive(&server_header(compression, true)).unwrap();

            let data: Arc<[u8]> = (0..600_000)
                .map(|index| (index % 251) as u8)
                .collect::<Vec<_>>()
                .into();
            let file = sender.create_readonly_file(data.clone()).unwrap();
            let pipe = sender.create_pipe(PipeOpenKind::IW).unwrap();
            sender.write_pipe(pipe, b"pipe payload").unwrap();
            sender.close_pipe_write(pipe).unwrap();
            let wire = sender
                .encode_protocol_with_rids(get_registry(), &[file, pipe])
                .unwrap();

            let mut decoder = FrameDecoder::new(DEFAULT_MAX_FRAME_SIZE);
            decoder.push(&wire);
            let mut kinds = Vec::new();
            while let Some(frame) = decoder.next_frame().unwrap() {
                kinds.push(frame.kind);
            }
            assert_eq!(
                kinds,
                vec![
                    MessageType::OpenFile,
                    MessageType::BufferFill,
                    MessageType::BufferFill,
                    MessageType::BufferFill,
                    MessageType::OpenIwPipe,
                    MessageType::PipeTransfer,
                    MessageType::PipeShutdownW,
                    MessageType::InjectRids,
                    MessageType::Protocol,
                ]
            );

            let received = receiver.receive(&wire).unwrap();
            assert_eq!(receiver.resources().data(file), Some(data.as_ref()));
            let read = receiver.resources_mut().read_pipe(pipe, 4096).unwrap();
            assert_eq!(read.data, b"pipe payload");
            assert!(read.eof);
            assert!(received.events.iter().any(|event| matches!(
                event,
                ClientEvent::Protocol(messages)
                    if messages.len() == 1
                        && messages[0].has_file_descriptors()
                        && messages[0].opcode() == 1
            )));
        }
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

    #[test]
    fn reports_unknown_rid_frame_context_within_bounded_history() {
        let mut client = negotiated_client();
        for index in 10..80 {
            let rid = Rid(index);
            client.resources_mut().open_file(rid, 0).unwrap();
            assert!(client.resources_mut().close_resource(rid));
        }
        let diff = |rid: Rid| {
            let mut payload = rid.0.to_le_bytes().to_vec();
            payload.extend_from_slice(&[0; 8]);
            encode_frame(MessageType::BufferDiff, &payload).unwrap()
        };
        let evicted = client.receive(&diff(Rid(10))).unwrap_err().to_string();
        assert!(
            evicted.contains("resource id 10")
                && evicted.contains("processing BufferDiff frame")
                && evicted.contains("(not open)"),
            "{evicted}"
        );
        let recent = client.receive(&diff(Rid(79))).unwrap_err().to_string();
        assert!(
            recent.contains("resource id 79")
                && recent.contains("processing BufferDiff frame")
                && recent.contains("(recently closed by local lifecycle)"),
            "{recent}"
        );
        let unknown = client.receive(&diff(Rid(199))).unwrap_err().to_string();
        assert!(
            unknown.contains("resource id 199")
                && unknown.contains("processing BufferDiff frame")
                && unknown.contains("(not open)"),
            "{unknown}"
        );
    }
}
