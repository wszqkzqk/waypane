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

use super::*;
use waypipe_client::{
    parse_wayland_messages, ClientConfig, Compression, FrameDecoder, MessageType, WaylandMessage,
    DEFAULT_MAX_FRAME_SIZE, WAYPIPE_PROTOCOL_VERSION,
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

fn record(interface: &'static Interface) -> ObjectRecord {
    ObjectRecord::new(interface, interface.version)
}

fn method<'a>(
    interface: &'a Interface,
    name: &str,
) -> &'a crate::wayland_backend::protocol::MessageDesc {
    interface
        .requests
        .iter()
        .find(|request| request.name == name)
        .unwrap()
}

struct Fixture {
    compositor: Compositor,
    client: Client,
}

impl Fixture {
    fn new() -> Self {
        let mut compositor = Compositor::new(Arc::from(DEFAULT_KEYMAP));
        let mut client = Client::new(ClientConfig::default());
        client.receive(&header()).unwrap();

        compositor
            .insert_record(50, record(&wl::WL_SHM_POOL_INTERFACE))
            .unwrap();
        client.resources_mut().open_file(Rid(1), 4).unwrap();
        client
            .resources_mut()
            .apply_fill(
                Rid(1),
                &[0, 0, 0, 0, 4, 0, 0, 0, 0, 255, 0, 0],
                Compression::None,
            )
            .unwrap();
        compositor.pools.insert(
            50,
            Pool {
                rid: Rid(1),
                size: 4,
                buffer_refs: 3,
                destroyed: false,
            },
        );
        for id in [51, 52, 53] {
            compositor
                .insert_record(id, record(&wl::WL_BUFFER_INTERFACE))
                .unwrap();
            compositor.buffers.insert(
                id,
                Buffer {
                    pool_id: 50,
                    rid: Rid(1),
                    offset: 0,
                    width: 1,
                    height: 1,
                    stride: 4,
                    format: ShmFormat::Xrgb8888,
                    alive: true,
                },
            );
        }

        compositor.surfaces.insert(
            10,
            Surface {
                role: Role::Toplevel(1),
                window_id: Some(1),
                ..Surface::default()
            },
        );
        for (surface, proxy) in [(11, 21), (12, 22)] {
            compositor.surfaces.insert(
                surface,
                Surface {
                    role: Role::Subsurface { parent: 10, proxy },
                    window_id: Some(1),
                    ..Surface::default()
                },
            );
            compositor.subsurface_sync.insert(
                proxy,
                SubsurfaceSync {
                    surface,
                    synchronized: true,
                    cached: Default::default(),
                },
            );
        }
        compositor.subsurface_stacks.insert(10, vec![11, 12]);
        compositor.xdg_surfaces.insert(
            20,
            XdgSurface {
                surface_id: 10,
                window_id: Some(1),
                initial_ack_seen: true,
                ..XdgSurface::default()
            },
        );
        compositor.windows.insert(
            1,
            Window {
                id: 1,
                xdg_surface_id: 20,
                surface_id: 10,
                kind: WindowKind::Toplevel,
                title: "root".into(),
                app_id: "test".into(),
                geometry: Rect {
                    x: 0,
                    y: 0,
                    width: 2,
                    height: 2,
                },
            },
        );
        for callback in [44, 55] {
            compositor
                .insert_record(callback, record(&wl::WL_CALLBACK_INTERFACE))
                .unwrap();
        }
        Self { compositor, client }
    }

    fn restack(&mut self, sibling: u32) {
        self.compositor
            .restack_subsurface(&DecodedRequest {
                object_id: 21,
                interface: &wl::WL_SUBSURFACE_INTERFACE,
                method: method(&wl::WL_SUBSURFACE_INTERFACE, "place_above"),
                opcode: 1,
                version: 1,
                args: vec![Value::Object(Some(sibling))],
            })
            .unwrap();
    }
}

#[test]
fn sync_subsurface_commits_with_parent_and_preserves_z_order() {
    let mut fixture = Fixture::new();
    fixture
        .compositor
        .surfaces
        .get_mut(&11)
        .unwrap()
        .pending
        .attach = Some(Some(51));
    fixture
        .compositor
        .surfaces
        .get_mut(&11)
        .unwrap()
        .pending
        .callbacks = vec![55];
    fixture
        .compositor
        .commit_surface(&mut fixture.client, 11)
        .unwrap();
    assert!(fixture.compositor.take_events().is_empty());
    assert!(fixture.compositor.take_outgoing().is_empty());
    assert_eq!(fixture.compositor.subsurface_sync[&21].cached.len(), 1);

    fixture.restack(12);
    assert_eq!(fixture.compositor.subsurface_stacks[&10], vec![12, 11]);
    fixture
        .compositor
        .surfaces
        .get_mut(&10)
        .unwrap()
        .pending
        .attach = Some(Some(53));
    fixture
        .compositor
        .surfaces
        .get_mut(&10)
        .unwrap()
        .pending
        .callbacks = vec![44];
    fixture
        .compositor
        .commit_surface(&mut fixture.client, 10)
        .unwrap();

    let events = fixture.compositor.take_events();
    assert_eq!(events.len(), 2);
    let CoreEvent::Frame(child_frame) = &events[1] else {
        panic!("expected subsurface frame");
    };
    assert_eq!(child_frame.surface_id, 11);
    assert_eq!(child_frame.z_order, Some(vec![10, 12, 11]));
    assert!(fixture.compositor.take_outgoing().is_empty());

    fixture
        .compositor
        .complete_frame(child_frame.id, 10)
        .unwrap();
    assert_eq!(fixture.compositor.take_outgoing().len(), 3);
    assert!(fixture.compositor.subsurface_sync[&21].cached.is_empty());
}

#[test]
fn desynchronized_subsurface_commits_immediately() {
    let mut fixture = Fixture::new();
    fixture
        .compositor
        .subsurface_sync
        .get_mut(&21)
        .unwrap()
        .synchronized = false;
    fixture
        .compositor
        .surfaces
        .get_mut(&11)
        .unwrap()
        .pending
        .attach = Some(Some(52));
    fixture
        .compositor
        .commit_surface(&mut fixture.client, 11)
        .unwrap();
    let events = fixture.compositor.take_events();
    assert!(matches!(&events[..], [CoreEvent::Frame(frame)] if frame.surface_id == 11));
}

#[test]
fn hit_testing_targets_topmost_subsurface_then_popup() {
    let mut fixture = Fixture::new();
    fixture
        .compositor
        .surfaces
        .get_mut(&10)
        .unwrap()
        .pending
        .attach = Some(Some(53));
    fixture
        .compositor
        .commit_surface(&mut fixture.client, 10)
        .unwrap();
    fixture.compositor.take_events();
    fixture.restack(12);
    fixture
        .compositor
        .surfaces
        .get_mut(&11)
        .unwrap()
        .pending
        .attach = Some(Some(51));
    fixture
        .compositor
        .commit_surface(&mut fixture.client, 11)
        .unwrap();
    fixture
        .compositor
        .surfaces
        .get_mut(&12)
        .unwrap()
        .pending
        .attach = Some(Some(52));
    fixture
        .compositor
        .commit_surface(&mut fixture.client, 12)
        .unwrap();
    fixture
        .compositor
        .commit_surface(&mut fixture.client, 10)
        .unwrap();
    fixture.compositor.take_events();

    fixture.compositor.pointer_motion(1, 0.5, 0.5, 1).unwrap();
    assert_eq!(fixture.compositor.pointer_focus, Some((1, 11, 0.5, 0.5)));

    fixture.compositor.surfaces.insert(
        13,
        Surface {
            role: Role::Popup(30),
            window_id: Some(30),
            logical_size: Size {
                width: 2,
                height: 2,
            },
            ..Surface::default()
        },
    );
    fixture
        .compositor
        .surfaces
        .get_mut(&13)
        .unwrap()
        .committed
        .buffer = Some(51);
    fixture.compositor.windows.insert(
        30,
        Window {
            id: 30,
            xdg_surface_id: 31,
            surface_id: 13,
            kind: WindowKind::Popup { parent: 1 },
            title: String::new(),
            app_id: String::new(),
            geometry: Rect {
                x: 3,
                y: 3,
                width: 2,
                height: 2,
            },
        },
    );
    fixture.compositor.pointer_motion(1, 4.5, 4.5, 1).unwrap();
    assert_eq!(fixture.compositor.pointer_focus, Some((30, 13, 1.5, 1.5)));
    assert_eq!(fixture.compositor.pointer_window, Some((1, 4.5, 4.5)));
}

#[test]
fn supports_shm_backing_larger_than_logical_pool() {
    let mut compositor = Compositor::new(Arc::from(DEFAULT_KEYMAP));
    let mut client = Client::new(ClientConfig::default());
    client.receive(&header()).unwrap();
    let rid = Rid(99);
    client.resources_mut().open_file(rid, 16384).unwrap();

    let create_pool = DecodedRequest {
        object_id: 2,
        interface: &wl::WL_SHM_INTERFACE,
        method: method(&wl::WL_SHM_INTERFACE, "create_pool"),
        opcode: 0,
        version: 1,
        args: vec![
            Value::NewId {
                id: 60,
                interface: Some(&wl::WL_SHM_POOL_INTERFACE),
            },
            Value::Fd(rid),
            Value::Int(4096),
        ],
    };
    compositor
        .dispatch(&mut client, &create_pool, ObjectKind::Shm)
        .unwrap();
    assert_eq!(compositor.pools[&60].size, 4096);

    let resize = |size| DecodedRequest {
        object_id: 60,
        interface: &wl::WL_SHM_POOL_INTERFACE,
        method: method(&wl::WL_SHM_POOL_INTERFACE, "resize"),
        opcode: 1,
        version: 1,
        args: vec![Value::Int(size)],
    };
    compositor
        .dispatch(&mut client, &resize(8192), ObjectKind::ShmPool)
        .unwrap();
    assert_eq!(compositor.pools[&60].size, 8192);

    let create_buffer = |id, offset| DecodedRequest {
        object_id: 60,
        interface: &wl::WL_SHM_POOL_INTERFACE,
        method: method(&wl::WL_SHM_POOL_INTERFACE, "create_buffer"),
        opcode: 0,
        version: 1,
        args: vec![
            Value::NewId {
                id,
                interface: Some(&wl::WL_BUFFER_INTERFACE),
            },
            Value::Int(offset),
            Value::Int(1),
            Value::Int(1),
            Value::Int(4),
            Value::Uint(0),
        ],
    };
    let error = compositor
        .dispatch(&mut client, &create_buffer(61, 8192), ObjectKind::ShmPool)
        .unwrap_err()
        .to_string();
    assert!(error.contains("buffer exceeds pool bounds"), "{error}");

    compositor
        .dispatch(&mut client, &resize(16384), ObjectKind::ShmPool)
        .unwrap();
    compositor
        .dispatch(&mut client, &create_buffer(62, 8192), ObjectKind::ShmPool)
        .unwrap();
    assert_eq!(compositor.buffers[&62].pool_id, 60);

    let error = compositor
        .dispatch(&mut client, &resize(8192), ObjectKind::ShmPool)
        .unwrap_err()
        .to_string();
    assert!(error.contains("16384 to 8192"), "{error}");
    assert_eq!(compositor.pools[&60].size, 16384);
}

#[test]
fn allows_empty_initial_commit_before_xdg_ack_configure() {
    let mut fixture = Fixture::new();
    {
        let xdg = fixture.compositor.xdg_surfaces.get_mut(&20).unwrap();
        xdg.initial_ack_seen = false;
        xdg.record_configure(777, None);
    }

    fixture
        .compositor
        .commit_surface(&mut fixture.client, 10)
        .unwrap();
    assert!(!fixture.compositor.xdg_surfaces[&20].initial_ack_seen);

    fixture
        .compositor
        .surfaces
        .get_mut(&10)
        .unwrap()
        .pending
        .attach = Some(None);
    fixture
        .compositor
        .commit_surface(&mut fixture.client, 10)
        .unwrap();
    assert!(!fixture.compositor.xdg_surfaces[&20].initial_ack_seen);

    fixture
        .compositor
        .surfaces
        .get_mut(&10)
        .unwrap()
        .pending
        .attach = Some(Some(51));
    let error = fixture
        .compositor
        .commit_surface(&mut fixture.client, 10)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("attached a buffer before initial ack_configure"),
        "{error}"
    );
    assert!(!fixture.compositor.xdg_surfaces[&20].initial_ack_seen);
}

fn latest_configure_serial(xdg: &XdgSurface) -> Option<u32> {
    xdg.pending_configures.back().map(|pending| pending.serial)
}

fn ack_configure(fixture: &mut Fixture, surface: u32, serial: u32) -> Result<()> {
    fixture.compositor.dispatch(
        &mut fixture.client,
        &request(
            surface,
            &xdg::XDG_SURFACE_INTERFACE,
            "ack_configure",
            6,
            vec![Value::Uint(serial)],
        ),
        ObjectKind::XdgSurface,
    )
}

#[test]
fn initial_ack_after_newer_configure_allows_first_buffer() {
    let mut fixture = Fixture::new();
    fixture
        .compositor
        .xdg_surfaces
        .get_mut(&20)
        .unwrap()
        .initial_ack_seen = false;
    fixture
        .compositor
        .configure_window(1, None, vec![4])
        .unwrap();
    let initial = latest_configure_serial(&fixture.compositor.xdg_surfaces[&20]).unwrap();
    fixture
        .compositor
        .configure_window(1, None, vec![4])
        .unwrap();
    let newer = latest_configure_serial(&fixture.compositor.xdg_surfaces[&20]).unwrap();
    assert_ne!(initial, newer);

    ack_configure(&mut fixture, 20, initial).unwrap();
    assert!(fixture.compositor.xdg_surfaces[&20].initial_ack_seen);
    assert_eq!(
        latest_configure_serial(&fixture.compositor.xdg_surfaces[&20]),
        Some(newer)
    );
    fixture
        .compositor
        .surfaces
        .get_mut(&10)
        .unwrap()
        .pending
        .attach = Some(Some(51));
    fixture
        .compositor
        .commit_surface(&mut fixture.client, 10)
        .unwrap();
    let events = fixture.compositor.take_events();
    assert!(events
        .iter()
        .any(|event| matches!(event, CoreEvent::Frame(_))));
}

#[test]
fn configure_acks_consume_earlier_serials_and_reject_invalid_ones() {
    let mut fixture = Fixture::new();
    fixture
        .compositor
        .configure_window(1, None, vec![])
        .unwrap();
    let first = latest_configure_serial(&fixture.compositor.xdg_surfaces[&20]).unwrap();
    let other_object_serial = fixture.compositor.next_serial();
    fixture
        .compositor
        .configure_window(1, None, vec![4])
        .unwrap();
    let second = latest_configure_serial(&fixture.compositor.xdg_surfaces[&20]).unwrap();
    fixture
        .compositor
        .configure_window(1, None, vec![])
        .unwrap();
    let third = latest_configure_serial(&fixture.compositor.xdg_surfaces[&20]).unwrap();

    for serial in [u32::MAX, other_object_serial] {
        let error = ack_configure(&mut fixture, 20, serial)
            .unwrap_err()
            .to_string();
        assert!(error.contains("ack_configure"), "{error}");
    }
    ack_configure(&mut fixture, 20, second).unwrap();
    assert_eq!(
        latest_configure_serial(&fixture.compositor.xdg_surfaces[&20]),
        Some(third)
    );
    for serial in [second, first] {
        let error = ack_configure(&mut fixture, 20, serial)
            .unwrap_err()
            .to_string();
        assert!(error.contains("already consumed"), "{error}");
    }
    ack_configure(&mut fixture, 20, third).unwrap();
    assert_eq!(
        latest_configure_serial(&fixture.compositor.xdg_surfaces[&20]),
        None
    );
}

#[test]
fn popup_ack_applies_geometry_of_the_acknowledged_configure() {
    let mut fixture = Fixture::new();
    fixture.compositor.windows.get_mut(&1).unwrap().geometry = Rect {
        x: 0,
        y: 0,
        width: 500,
        height: 300,
    };
    fixture
        .compositor
        .surfaces
        .get_mut(&10)
        .unwrap()
        .logical_size = Size {
        width: 500,
        height: 300,
    };
    replace_record(&mut fixture, 30, &xdg::XDG_SURFACE_INTERFACE);
    replace_record(&mut fixture, 40, &wl::WL_SURFACE_INTERFACE);
    replace_record(&mut fixture, 41, &xdg::XDG_POSITIONER_INTERFACE);
    replace_record(&mut fixture, 90, &xdg::XDG_POPUP_INTERFACE);
    fixture.compositor.surfaces.insert(
        40,
        Surface {
            role: Role::Popup(90),
            window_id: Some(90),
            ..Surface::default()
        },
    );
    fixture.compositor.xdg_surfaces.insert(
        30,
        XdgSurface {
            surface_id: 40,
            window_id: Some(90),
            ..XdgSurface::default()
        },
    );
    fixture.compositor.windows.insert(
        90,
        Window {
            id: 90,
            xdg_surface_id: 30,
            surface_id: 40,
            kind: WindowKind::Popup { parent: 1 },
            title: String::new(),
            app_id: String::new(),
            geometry: Rect::default(),
        },
    );

    let geometries = [
        Rect {
            x: 10,
            y: 20,
            width: 100,
            height: 50,
        },
        Rect {
            x: 15,
            y: 25,
            width: 80,
            height: 40,
        },
    ];
    let mut serials = Vec::new();
    for (token, geometry) in (7_u32..).zip(geometries) {
        fixture.compositor.positioners.insert(
            41,
            Positioner {
                size: Some((geometry.width, geometry.height)),
                anchor_rect: Some(Rect {
                    x: geometry.x,
                    y: geometry.y,
                    width: geometry.width,
                    height: geometry.height,
                }),
                ..Positioner::default()
            },
        );
        fixture
            .compositor
            .dispatch(
                &mut fixture.client,
                &request(
                    90,
                    &xdg::XDG_POPUP_INTERFACE,
                    "reposition",
                    3,
                    vec![Value::Object(Some(41)), Value::Uint(token)],
                ),
                ObjectKind::XdgPopup,
            )
            .unwrap();
        serials.push(latest_configure_serial(&fixture.compositor.xdg_surfaces[&30]).unwrap());
    }

    ack_configure(&mut fixture, 30, serials[0]).unwrap();
    assert_eq!(fixture.compositor.windows[&90].geometry, geometries[0]);
    ack_configure(&mut fixture, 30, serials[1]).unwrap();
    assert_eq!(fixture.compositor.windows[&90].geometry, geometries[1]);
    assert!(fixture
        .compositor
        .take_events()
        .iter()
        .any(|event| matches!(event, CoreEvent::WindowMetadata { id: 90, geometry, .. } if *geometry == geometries[1])));
}

#[test]
fn configure_history_is_bounded_and_always_accepts_latest_serial() {
    let mut fixture = Fixture::new();
    for _ in 0..100 {
        fixture
            .compositor
            .configure_window(1, None, vec![])
            .unwrap();
    }
    let xdg = &mut fixture.compositor.xdg_surfaces.get_mut(&20).unwrap();
    assert_eq!(xdg.pending_configures.len(), 64);
    let latest = latest_configure_serial(xdg).unwrap();
    ack_configure(&mut fixture, 20, latest).unwrap();
    assert_eq!(
        latest_configure_serial(&fixture.compositor.xdg_surfaces[&20]),
        None
    );
}

#[test]
fn popup_native_resize_and_focus_do_not_reconfigure_popup() {
    let mut fixture = Fixture::new();
    replace_record(&mut fixture, 30, &xdg::XDG_SURFACE_INTERFACE);
    replace_record(&mut fixture, 90, &xdg::XDG_POPUP_INTERFACE);
    fixture.compositor.surfaces.insert(
        40,
        Surface {
            role: Role::Popup(90),
            window_id: Some(90),
            ..Surface::default()
        },
    );
    fixture.compositor.xdg_surfaces.insert(
        30,
        XdgSurface {
            surface_id: 40,
            window_id: Some(90),
            ..XdgSurface::default()
        },
    );
    let geometry = Rect {
        x: 10,
        y: 20,
        width: 180,
        height: 120,
    };
    fixture.compositor.windows.insert(
        90,
        Window {
            id: 90,
            xdg_surface_id: 30,
            surface_id: 40,
            kind: WindowKind::Popup { parent: 1 },
            title: String::new(),
            app_id: String::new(),
            geometry,
        },
    );
    fixture.compositor.keyboard_focus(Some(1)).unwrap();
    fixture.compositor.take_outgoing();

    let resized = configure_window_result(&mut fixture);
    assert!(resized.is_ok(), "popup resize was rejected: {resized:?}");
    for focused in [90, 1, 90] {
        fixture.compositor.keyboard_focus(Some(focused)).unwrap();
    }

    let messages = outbound_messages(&mut fixture.compositor, &mut fixture.client);
    assert!(
        messages.iter().all(|message| {
            message.object_id() != 90
                || ![
                    event_opcode(&xdg::XDG_POPUP_INTERFACE, "configure"),
                    event_opcode(&xdg::XDG_TOPLEVEL_INTERFACE, "configure"),
                ]
                .contains(&message.opcode())
        }),
        "unexpected popup reconfiguration: {messages:?}"
    );
    assert!(fixture.compositor.xdg_surfaces[&30]
        .pending_configures
        .is_empty());
    assert_eq!(fixture.compositor.windows[&90].geometry, geometry);
    assert!(fixture
        .compositor
        .configure_window(999, None, vec![])
        .is_err());
}

fn configure_window_result(fixture: &mut Fixture) -> Result<()> {
    fixture.compositor.configure_window(
        90,
        Some(Size {
            width: 640,
            height: 480,
        }),
        vec![4],
    )
}

#[test]
fn queued_frame_keeps_its_shm_snapshot_after_diff_and_pool_close() {
    let mut fixture = Fixture::new();
    fixture
        .compositor
        .surfaces
        .get_mut(&10)
        .unwrap()
        .pending
        .attach = Some(Some(51));
    fixture
        .compositor
        .commit_surface(&mut fixture.client, 10)
        .unwrap();
    let events = fixture.compositor.take_events();
    let CoreEvent::Frame(frame) = &events[0] else {
        panic!("expected a root frame");
    };
    let snapshot = frame.buffer.as_ref().unwrap().data.clone();
    assert_eq!(&*snapshot, &[0, 255, 0, 0]);

    fixture
        .client
        .resources_mut()
        .apply_fill(
            Rid(1),
            &[0, 0, 0, 0, 4, 0, 0, 0, 255, 0, 0, 0],
            Compression::None,
        )
        .unwrap();
    assert_eq!(&*snapshot, &[0, 255, 0, 0]);
    assert_eq!(
        fixture.client.resources().data(Rid(1)),
        Some(&[255, 0, 0, 0][..])
    );

    assert!(fixture.client.resources_mut().close_transport_file(Rid(1)));
    assert_eq!(fixture.client.resources().data(Rid(1)), None);
    assert_eq!(&*snapshot, &[0, 255, 0, 0]);
}

#[test]
fn tracks_cursor_serial_role_hotspot_and_surface_lifetime() {
    let mut fixture = Fixture::new();
    fixture
        .compositor
        .insert_record(60, record(&wl::WL_POINTER_INTERFACE))
        .unwrap();
    fixture.compositor.seats.insert(
        1,
        Seat {
            pointers: vec![60],
            ..Seat::default()
        },
    );
    for id in [61, 62] {
        fixture
            .compositor
            .insert_record(id, record(&wl::WL_SURFACE_INTERFACE))
            .unwrap();
        fixture.compositor.surfaces.insert(id, Surface::default());
    }
    fixture
        .compositor
        .pointer_focus(Some(1), 10.0, 20.0)
        .unwrap();
    let serial = fixture.compositor.pointers[&60].enter_serial.unwrap();
    let set_cursor = |serial, surface| DecodedRequest {
        object_id: 60,
        interface: &wl::WL_POINTER_INTERFACE,
        method: method(&wl::WL_POINTER_INTERFACE, "set_cursor"),
        opcode: 0,
        version: 1,
        args: vec![
            Value::Uint(serial),
            Value::Object(surface),
            Value::Int(2),
            Value::Int(3),
        ],
    };

    fixture
        .compositor
        .dispatch(
            &mut fixture.client,
            &set_cursor(serial.wrapping_add(1), Some(62)),
            ObjectKind::Pointer,
        )
        .unwrap();
    assert_eq!(fixture.compositor.surfaces[&62].role, Role::None);
    let error = fixture
        .compositor
        .dispatch(
            &mut fixture.client,
            &set_cursor(serial, Some(10)),
            ObjectKind::Pointer,
        )
        .unwrap_err()
        .to_string();
    assert!(error.contains("different role"), "{error}");

    fixture.compositor.take_events();
    fixture
        .compositor
        .dispatch(
            &mut fixture.client,
            &set_cursor(serial, Some(61)),
            ObjectKind::Pointer,
        )
        .unwrap();
    assert_eq!(
        fixture.compositor.surfaces[&61].role,
        Role::Cursor { pointer: 60 }
    );
    assert_eq!(
        fixture.compositor.cursor_placement(),
        Some((1, 10.0, 20.0, Point { x: 2, y: 3 }))
    );
    let events = fixture.compositor.take_events();
    assert!(matches!(
        events.as_slice(),
        [CoreEvent::CursorChanged {
            window_id: Some(1),
            buffer: None,
            hotspot: Point { x: 2, y: 3 },
        }]
    ));

    let surface = fixture.compositor.surfaces.get_mut(&61).unwrap();
    surface.pending.attach = Some(Some(51));
    surface.pending.offset = Some(Point { x: 4, y: 1 });
    fixture
        .compositor
        .commit_surface_now(&mut fixture.client, 61)
        .unwrap();
    assert_eq!(
        fixture.compositor.surfaces[&61].committed.offset,
        Point { x: 4, y: 1 }
    );
    let events = fixture.compositor.take_events();
    match events.as_slice() {
        [CoreEvent::CursorChanged {
            window_id: Some(1),
            buffer: Some(view),
            hotspot: Point { x: -2, y: 2 },
        }] => assert_eq!(&*view.data, &[0, 255, 0, 0]),
        other => panic!("unexpected cursor events: {other:?}"),
    }

    fixture
        .compositor
        .dispatch(
            &mut fixture.client,
            &set_cursor(serial, None),
            ObjectKind::Pointer,
        )
        .unwrap();
    assert_eq!(fixture.compositor.cursor_placement(), None);
    assert!(matches!(
        fixture.compositor.take_events().as_slice(),
        [CoreEvent::CursorChanged {
            window_id: None,
            buffer: None,
            ..
        }]
    ));

    fixture
        .compositor
        .dispatch(
            &mut fixture.client,
            &set_cursor(serial, Some(61)),
            ObjectKind::Pointer,
        )
        .unwrap();
    fixture.compositor.take_events();
    let destroy_surface = DecodedRequest {
        object_id: 61,
        interface: &wl::WL_SURFACE_INTERFACE,
        method: method(&wl::WL_SURFACE_INTERFACE, "destroy"),
        opcode: 0,
        version: 1,
        args: Vec::new(),
    };
    fixture
        .compositor
        .dispatch(&mut fixture.client, &destroy_surface, ObjectKind::Surface)
        .unwrap();
    fixture
        .compositor
        .finish_destructor(&mut fixture.client, 61, ObjectKind::Surface)
        .unwrap();
    assert_eq!(fixture.compositor.cursor_placement(), None);
    assert!(matches!(
        fixture.compositor.take_events().as_slice(),
        [CoreEvent::CursorChanged {
            window_id: None,
            buffer: None,
            ..
        }]
    ));
}

#[test]
fn applies_committed_input_regions_to_hit_testing() {
    let mut fixture = Fixture::new();
    fixture
        .compositor
        .insert_record(70, record(&wl::WL_REGION_INTERFACE))
        .unwrap();
    fixture.compositor.regions.insert(70, Region::default());
    let region_request = |name: &str, rect: Rect| DecodedRequest {
        object_id: 70,
        interface: &wl::WL_REGION_INTERFACE,
        method: method(&wl::WL_REGION_INTERFACE, name),
        opcode: 0,
        version: 1,
        args: vec![
            Value::Int(rect.x),
            Value::Int(rect.y),
            Value::Int(rect.width),
            Value::Int(rect.height),
        ],
    };
    let outer = Rect {
        x: 0,
        y: 0,
        width: 100,
        height: 100,
    };
    let corner = Rect {
        x: 0,
        y: 0,
        width: 20,
        height: 20,
    };
    let hole = Rect {
        x: 20,
        y: 20,
        width: 60,
        height: 60,
    };
    for (name, rect) in [("add", outer), ("add", corner), ("subtract", hole)] {
        fixture
            .compositor
            .dispatch(
                &mut fixture.client,
                &region_request(name, rect),
                ObjectKind::Region,
            )
            .unwrap();
    }

    let set_input_region = |region| DecodedRequest {
        object_id: 10,
        interface: &wl::WL_SURFACE_INTERFACE,
        method: method(&wl::WL_SURFACE_INTERFACE, "set_input_region"),
        opcode: 0,
        version: 1,
        args: vec![Value::Object(region)],
    };
    fixture
        .compositor
        .dispatch(
            &mut fixture.client,
            &set_input_region(Some(70)),
            ObjectKind::Surface,
        )
        .unwrap();
    fixture
        .compositor
        .dispatch(
            &mut fixture.client,
            &region_request(
                "add",
                Rect {
                    x: 40,
                    y: 40,
                    width: 20,
                    height: 20,
                },
            ),
            ObjectKind::Region,
        )
        .unwrap();

    let original = fixture.compositor.regions[&70].clone();
    let error = fixture
        .compositor
        .dispatch(
            &mut fixture.client,
            &region_request(
                "subtract",
                Rect {
                    x: 0,
                    y: 0,
                    width: -1,
                    height: 10,
                },
            ),
            ObjectKind::Region,
        )
        .unwrap_err()
        .to_string();
    assert!(error.contains("invalid or too complex"), "{error}");
    assert_eq!(fixture.compositor.regions[&70], original);

    fixture
        .compositor
        .commit_surface_now(&mut fixture.client, 10)
        .unwrap();
    for (x, y, expected) in [
        (10.0, 10.0, true),
        (19.999, 50.0, true),
        (20.0, 50.0, false),
        (50.0, 50.0, false),
        (79.999, 50.0, false),
        (80.0, 50.0, true),
        (50.0, 19.999, true),
        (50.0, 20.0, false),
        (50.0, 80.0, true),
        (-0.001, 50.0, false),
        (100.0, 50.0, false),
    ] {
        assert_eq!(
            fixture.compositor.hit_surface(1, x, y).is_some(),
            expected,
            "unexpected hit-testing result at ({x}, {y})"
        );
    }

    fixture
        .compositor
        .dispatch(
            &mut fixture.client,
            &set_input_region(None),
            ObjectKind::Surface,
        )
        .unwrap();
    fixture
        .compositor
        .commit_surface_now(&mut fixture.client, 10)
        .unwrap();
    assert!(fixture.compositor.hit_surface(1, 50.0, 50.0).is_some());
}

fn request(
    object_id: u32,
    interface: &'static Interface,
    name: &str,
    version: u32,
    args: Vec<Value>,
) -> DecodedRequest {
    let (opcode, method) = interface
        .requests
        .iter()
        .enumerate()
        .find(|(_, request)| request.name == name)
        .unwrap_or_else(|| panic!("unknown request {}.{name}", interface.name));
    DecodedRequest {
        object_id,
        interface,
        method,
        opcode: opcode as u16,
        version,
        args,
    }
}

fn outbound_messages(compositor: &mut Compositor, client: &mut Client) -> Vec<WaylandMessage> {
    let messages = compositor.take_outgoing();
    if messages.is_empty() {
        return Vec::new();
    }
    let encoded = client.encode_messages(&messages).unwrap();
    let mut decoder = FrameDecoder::new(DEFAULT_MAX_FRAME_SIZE);
    decoder.push(&encoded);
    let mut decoded = Vec::new();
    while let Some(frame) = decoder.next_frame().unwrap() {
        if frame.kind == MessageType::Protocol {
            decoded.extend(parse_wayland_messages(frame.payload).unwrap());
        }
    }
    decoded
}

#[test]
fn configures_repositions_and_grabs_popups() {
    let mut fixture = Fixture::new();
    fixture.compositor.windows.get_mut(&1).unwrap().geometry = Rect {
        x: 0,
        y: 0,
        width: 500,
        height: 300,
    };
    fixture
        .compositor
        .surfaces
        .get_mut(&10)
        .unwrap()
        .logical_size = Size {
        width: 500,
        height: 300,
    };
    for id in [30, 40] {
        fixture
            .compositor
            .insert_record(id, record(&wl::WL_SURFACE_INTERFACE))
            .unwrap();
    }
    fixture.compositor.surfaces.insert(40, Surface::default());
    replace_record(&mut fixture, 30, &xdg::XDG_SURFACE_INTERFACE);
    fixture.compositor.xdg_surfaces.insert(
        30,
        XdgSurface {
            surface_id: 40,
            ..XdgSurface::default()
        },
    );
    fixture.compositor.surfaces.insert(30, Surface::default());
    replace_record(&mut fixture, 41, &xdg::XDG_POSITIONER_INTERFACE);
    fixture.compositor.positioners.insert(
        41,
        Positioner {
            size: Some((100, 50)),
            anchor_rect: Some(Rect {
                x: 450,
                y: 280,
                width: 20,
                height: 20,
            }),
            anchor: 8,
            gravity: 8,
            constraint_adjustment: xdg_shell::FLIP_X | xdg_shell::FLIP_Y,
            ..Positioner::default()
        },
    );
    fixture
        .compositor
        .dispatch(
            &mut fixture.client,
            &request(
                30,
                &xdg::XDG_SURFACE_INTERFACE,
                "get_popup",
                6,
                vec![
                    Value::NewId {
                        id: 90,
                        interface: Some(&xdg::XDG_POPUP_INTERFACE),
                    },
                    Value::Object(Some(20)),
                    Value::Object(Some(41)),
                ],
            ),
            ObjectKind::XdgSurface,
        )
        .unwrap();
    let flipped = Rect {
        x: 350,
        y: 230,
        width: 100,
        height: 50,
    };
    assert_eq!(fixture.compositor.windows[&90].geometry, flipped);
    assert_eq!(
        fixture.compositor.xdg_surfaces[&30]
            .pending_configures
            .back()
            .and_then(|pending| pending.geometry),
        Some(flipped)
    );
    let serial = latest_configure_serial(&fixture.compositor.xdg_surfaces[&30]).unwrap();
    fixture
        .compositor
        .dispatch(
            &mut fixture.client,
            &request(
                30,
                &xdg::XDG_SURFACE_INTERFACE,
                "ack_configure",
                6,
                vec![Value::Uint(serial)],
            ),
            ObjectKind::XdgSurface,
        )
        .unwrap();
    assert_eq!(
        latest_configure_serial(&fixture.compositor.xdg_surfaces[&30]),
        None
    );
    assert!(fixture
        .compositor
        .dispatch(
            &mut fixture.client,
            &request(
                30,
                &xdg::XDG_SURFACE_INTERFACE,
                "ack_configure",
                6,
                vec![Value::Uint(serial)],
            ),
            ObjectKind::XdgSurface,
        )
        .is_err());
    fixture.compositor.take_events();
    fixture.compositor.take_outgoing();

    fixture.compositor.positioners.insert(
        41,
        Positioner {
            size: Some((600, 400)),
            anchor_rect: Some(Rect {
                x: 450,
                y: 250,
                width: 50,
                height: 50,
            }),
            constraint_adjustment: xdg_shell::RESIZE_X | xdg_shell::RESIZE_Y,
            ..Positioner::default()
        },
    );
    fixture
        .compositor
        .dispatch(
            &mut fixture.client,
            &request(
                90,
                &xdg::XDG_POPUP_INTERFACE,
                "reposition",
                3,
                vec![Value::Object(Some(41)), Value::Uint(77)],
            ),
            ObjectKind::XdgPopup,
        )
        .unwrap();
    let resized = Rect {
        x: 175,
        y: 75,
        width: 325,
        height: 225,
    };
    assert_eq!(fixture.compositor.windows[&90].geometry, flipped);
    assert_eq!(
        fixture.compositor.xdg_surfaces[&30]
            .pending_configures
            .back()
            .and_then(|pending| pending.geometry),
        Some(resized)
    );
    let messages = outbound_messages(&mut fixture.compositor, &mut fixture.client);
    assert_eq!(messages.len(), 3, "unexpected messages: {messages:?}");
    assert_eq!(messages[0].object_id(), 90);
    assert_eq!(
        messages[0].opcode(),
        event_opcode(&xdg::XDG_POPUP_INTERFACE, "repositioned")
    );
    assert_eq!(
        u32::from_le_bytes(messages[0].payload()[..4].try_into().unwrap()),
        77
    );
    assert_eq!(messages[1].object_id(), 90);
    assert_eq!(
        messages[1].opcode(),
        event_opcode(&xdg::XDG_POPUP_INTERFACE, "configure")
    );
    let configure = messages[1]
        .payload()
        .as_chunks::<4>()
        .0
        .iter()
        .map(|bytes| i32::from_le_bytes(*bytes))
        .collect::<Vec<_>>();
    assert_eq!(configure, [175, 75, 325, 225]);
    assert_eq!(messages[2].object_id(), 30);
    assert_eq!(
        messages[2].opcode(),
        event_opcode(&xdg::XDG_SURFACE_INTERFACE, "configure")
    );
    let serial = u32::from_le_bytes(messages[2].payload()[..4].try_into().unwrap());
    fixture
        .compositor
        .dispatch(
            &mut fixture.client,
            &request(
                30,
                &xdg::XDG_SURFACE_INTERFACE,
                "ack_configure",
                6,
                vec![Value::Uint(serial)],
            ),
            ObjectKind::XdgSurface,
        )
        .unwrap();
    assert_eq!(fixture.compositor.windows[&90].geometry, resized);

    fixture.compositor.popup_positioners.insert(
        90,
        Positioner {
            size: Some((100, 50)),
            anchor_rect: Some(Rect {
                x: 350,
                y: 150,
                width: 20,
                height: 20,
            }),
            anchor: 6,
            gravity: 8,
            constraint_adjustment: xdg_shell::SLIDE_X | xdg_shell::SLIDE_Y,
            reactive: true,
            ..Positioner::default()
        },
    );
    fixture
        .compositor
        .configure_window(
            1,
            Some(Size {
                width: 400,
                height: 200,
            }),
            Vec::new(),
        )
        .unwrap();
    let reactive = Rect {
        x: 300,
        y: 150,
        width: 100,
        height: 50,
    };
    assert_eq!(
        fixture.compositor.xdg_surfaces[&30]
            .pending_configures
            .back()
            .and_then(|pending| pending.geometry),
        Some(reactive)
    );
    let serial = latest_configure_serial(&fixture.compositor.xdg_surfaces[&30]).unwrap();
    fixture
        .compositor
        .dispatch(
            &mut fixture.client,
            &request(
                30,
                &xdg::XDG_SURFACE_INTERFACE,
                "ack_configure",
                6,
                vec![Value::Uint(serial)],
            ),
            ObjectKind::XdgSurface,
        )
        .unwrap();
    assert_eq!(fixture.compositor.windows[&90].geometry, reactive);

    replace_record(&mut fixture, 70, &wl::WL_SEAT_INTERFACE);
    replace_record(&mut fixture, 60, &wl::WL_POINTER_INTERFACE);
    fixture.compositor.seats.insert(
        70,
        Seat {
            pointers: vec![60],
            ..Seat::default()
        },
    );
    fixture
        .compositor
        .pointer_motion(1, 320.0, 175.0, 1)
        .unwrap();
    assert_eq!(
        fixture.compositor.pointer_focus.map(|focus| focus.0),
        Some(90)
    );
    fixture.compositor.pointer_button(0x110, true, 1).unwrap();
    let serial = *fixture.compositor.input_serials.back().unwrap();
    fixture
        .compositor
        .dispatch(
            &mut fixture.client,
            &request(
                90,
                &xdg::XDG_POPUP_INTERFACE,
                "grab",
                1,
                vec![Value::Object(Some(70)), Value::Uint(serial)],
            ),
            ObjectKind::XdgPopup,
        )
        .unwrap();
    assert_eq!(fixture.compositor.popup_grab, [90]);
    assert_eq!(fixture.compositor.keyboard_focus, Some(90));
    fixture.compositor.take_outgoing();
    fixture.compositor.pointer_motion(1, 10.0, 10.0, 2).unwrap();
    assert_eq!(fixture.compositor.popup_grab, [90]);
    assert!(
        !outbound_messages(&mut fixture.compositor, &mut fixture.client)
            .iter()
            .any(|message| message.object_id() == 90
                && message.opcode() == event_opcode(&xdg::XDG_POPUP_INTERFACE, "popup_done"))
    );
    fixture.compositor.pointer_button(0x110, true, 3).unwrap();
    assert!(fixture.compositor.popup_grab.is_empty());
    assert!(
        outbound_messages(&mut fixture.compositor, &mut fixture.client)
            .iter()
            .any(|message| message.object_id() == 90
                && message.opcode() == event_opcode(&xdg::XDG_POPUP_INTERFACE, "popup_done"))
    );
}

fn replace_record(fixture: &mut Fixture, id: u32, interface: &'static Interface) {
    if fixture.compositor.objects.contains_key(&id) {
        fixture.compositor.objects.remove(&id);
    }
    fixture
        .compositor
        .insert_record(id, record(interface))
        .unwrap();
}

#[test]
fn enforces_toplevel_size_limits_and_unacknowledged_serials() {
    let mut fixture = Fixture::new();
    for (name, width, height) in [("set_min_size", 200, 100), ("set_max_size", 800, 600)] {
        fixture
            .compositor
            .dispatch(
                &mut fixture.client,
                &request(
                    1,
                    &xdg::XDG_TOPLEVEL_INTERFACE,
                    name,
                    1,
                    vec![Value::Int(width), Value::Int(height)],
                ),
                ObjectKind::XdgToplevel,
            )
            .unwrap();
    }
    fixture
        .compositor
        .configure_window(
            1,
            Some(Size {
                width: 50,
                height: 2000,
            }),
            Vec::new(),
        )
        .unwrap();
    assert_eq!(
        fixture.compositor.xdg_surfaces[&20].configured_size,
        Some(Size {
            width: 200,
            height: 600
        })
    );
    let serial = latest_configure_serial(&fixture.compositor.xdg_surfaces[&20]).unwrap();
    fixture
        .compositor
        .dispatch(
            &mut fixture.client,
            &request(
                20,
                &xdg::XDG_SURFACE_INTERFACE,
                "ack_configure",
                1,
                vec![Value::Uint(serial)],
            ),
            ObjectKind::XdgSurface,
        )
        .unwrap();
    assert!(fixture
        .compositor
        .dispatch(
            &mut fixture.client,
            &request(
                20,
                &xdg::XDG_SURFACE_INTERFACE,
                "ack_configure",
                1,
                vec![Value::Uint(serial)],
            ),
            ObjectKind::XdgSurface,
        )
        .is_err());

    assert!(fixture
        .compositor
        .dispatch(
            &mut fixture.client,
            &request(
                1,
                &xdg::XDG_TOPLEVEL_INTERFACE,
                "set_min_size",
                1,
                vec![Value::Int(900), Value::Int(100)],
            ),
            ObjectKind::XdgToplevel,
        )
        .is_err());
    assert_eq!(
        fixture.compositor.xdg_surfaces[&20].min_size,
        Size {
            width: 200,
            height: 100
        }
    );
    assert!(fixture
        .compositor
        .dispatch(
            &mut fixture.client,
            &request(
                1,
                &xdg::XDG_TOPLEVEL_INTERFACE,
                "set_max_size",
                1,
                vec![Value::Int(0), Value::Int(500)],
            ),
            ObjectKind::XdgToplevel,
        )
        .is_err());

    fixture
        .compositor
        .configure_window(
            1,
            Some(Size {
                width: 50,
                height: 2000,
            }),
            Vec::new(),
        )
        .unwrap();
    fixture
        .compositor
        .dispatch(
            &mut fixture.client,
            &request(
                1,
                &xdg::XDG_TOPLEVEL_INTERFACE,
                "set_max_size",
                1,
                vec![Value::Int(400), Value::Int(300)],
            ),
            ObjectKind::XdgToplevel,
        )
        .unwrap();
    assert_eq!(
        fixture.compositor.xdg_surfaces[&20].configured_size,
        Some(Size {
            width: 200,
            height: 300
        })
    );
}

#[test]
fn forwards_evdev_keycodes_once_per_logical_transition() {
    let mut fixture = Fixture::new();
    replace_record(&mut fixture, 70, &wl::WL_SEAT_INTERFACE);
    replace_record(&mut fixture, 61, &wl::WL_KEYBOARD_INTERFACE);
    fixture.compositor.seats.insert(
        70,
        Seat {
            keyboards: vec![61],
            ..Seat::default()
        },
    );
    fixture.compositor.keyboard_focus(Some(1)).unwrap();
    fixture.compositor.take_outgoing();
    fixture.compositor.key(1, true, 10).unwrap();
    fixture.compositor.key(42, true, 20).unwrap();
    fixture.compositor.key(42, true, 21).unwrap();
    fixture.compositor.key(42, false, 30).unwrap();
    fixture.compositor.key(42, false, 31).unwrap();
    fixture.compositor.key(2, true, 40).unwrap();
    let messages = outbound_messages(&mut fixture.compositor, &mut fixture.client);
    let key_opcode = event_opcode(&wl::WL_KEYBOARD_INTERFACE, "key");
    let keys = messages
        .iter()
        .filter(|message| message.object_id() == 61 && message.opcode() == key_opcode)
        .map(|message| {
            let words = message
                .payload()
                .as_chunks::<4>()
                .0
                .iter()
                .map(|bytes| u32::from_le_bytes(*bytes))
                .collect::<Vec<_>>();
            (words[1], words[2], words[3])
        })
        .collect::<Vec<_>>();
    assert_eq!(keys, [(10, 1, 1), (20, 42, 1), (30, 42, 0), (40, 2, 1)]);

    fixture.compositor.keyboard_focus(None).unwrap();
    fixture.compositor.take_outgoing();
    fixture.compositor.key(2, false, 50).unwrap();
    assert!(
        !outbound_messages(&mut fixture.compositor, &mut fixture.client)
            .iter()
            .any(|message| message.opcode() == key_opcode)
    );
}

#[test]
fn every_supported_request_has_an_explicit_routing_policy() {
    let objects: &[(&Interface, u32)] = &[
        (&wl::WL_DISPLAY_INTERFACE, 1),
        (&wl::WL_REGISTRY_INTERFACE, 1),
        (&wl::WL_CALLBACK_INTERFACE, 1),
        (&wl::WL_COMPOSITOR_INTERFACE, 6),
        (&wl::WL_SUBCOMPOSITOR_INTERFACE, 1),
        (&wl::WL_SHM_INTERFACE, 2),
        (&wl::WL_SHM_POOL_INTERFACE, 1),
        (&wl::WL_BUFFER_INTERFACE, 1),
        (&wl::WL_SURFACE_INTERFACE, 6),
        (&wl::WL_SUBSURFACE_INTERFACE, 1),
        (&wl::WL_REGION_INTERFACE, 1),
        (&xdg::XDG_WM_BASE_INTERFACE, 6),
        (&xdg::XDG_SURFACE_INTERFACE, 6),
        (&xdg::XDG_TOPLEVEL_INTERFACE, 6),
        (&xdg::XDG_POPUP_INTERFACE, 6),
        (&xdg::XDG_POSITIONER_INTERFACE, 6),
        (&wl::WL_SEAT_INTERFACE, 9),
        (&wl::WL_KEYBOARD_INTERFACE, 9),
        (&wl::WL_POINTER_INTERFACE, 9),
        (&wl::WL_TOUCH_INTERFACE, 9),
        (&wl::WL_OUTPUT_INTERFACE, 4),
        (&wl::WL_DATA_DEVICE_MANAGER_INTERFACE, 3),
        (&wl::WL_DATA_DEVICE_INTERFACE, 3),
        (&wl::WL_DATA_SOURCE_INTERFACE, 3),
        (&wl::WL_DATA_OFFER_INTERFACE, 3),
        (
            &crate::protocol::text_input::ZWP_TEXT_INPUT_MANAGER_V3_INTERFACE,
            1,
        ),
        (&crate::protocol::text_input::ZWP_TEXT_INPUT_V3_INTERFACE, 1),
    ];
    for (interface, version) in objects {
        let kind = ObjectRecord::new(interface, *version).kind;
        let routed = routed_requests(kind);
        for request in interface
            .requests
            .iter()
            .filter(|request| request.since <= *version)
        {
            if !request.is_destructor {
                assert!(
                    routed.contains(&request.name),
                    "{}.{} since {} has no explicit routing policy",
                    interface.name,
                    request.name,
                    request.since
                );
            }
        }
        for name in routed {
            assert!(
                interface
                    .requests
                    .iter()
                    .any(|request| request.name == *name),
                "stale route {}.{name}",
                interface.name
            );
        }
    }
}

fn routed_requests(kind: ObjectKind) -> &'static [&'static str] {
    match kind {
        ObjectKind::Display => &["sync", "get_registry"],
        ObjectKind::Registry => &["bind"],
        ObjectKind::Callback
        | ObjectKind::Buffer
        | ObjectKind::Keyboard
        | ObjectKind::Touch
        | ObjectKind::Output
        | ObjectKind::Other => &[],
        ObjectKind::Compositor => &["create_surface", "create_region"],
        ObjectKind::Subcompositor => &["get_subsurface"],
        ObjectKind::Shm => &["create_pool"],
        ObjectKind::ShmPool => &["create_buffer", "resize"],
        ObjectKind::Surface => &[
            "attach",
            "damage",
            "frame",
            "set_opaque_region",
            "set_input_region",
            "commit",
            "set_buffer_transform",
            "set_buffer_scale",
            "damage_buffer",
            "offset",
        ],
        ObjectKind::Subsurface => &[
            "set_position",
            "place_above",
            "place_below",
            "set_sync",
            "set_desync",
        ],
        ObjectKind::Region => &["add", "subtract"],
        ObjectKind::XdgWmBase => &["create_positioner", "get_xdg_surface", "pong"],
        ObjectKind::XdgSurface => &[
            "get_toplevel",
            "get_popup",
            "set_window_geometry",
            "ack_configure",
        ],
        ObjectKind::XdgToplevel => &[
            "set_parent",
            "set_title",
            "set_app_id",
            "show_window_menu",
            "move",
            "resize",
            "set_max_size",
            "set_min_size",
            "set_maximized",
            "unset_maximized",
            "set_fullscreen",
            "unset_fullscreen",
            "set_minimized",
        ],
        ObjectKind::XdgPopup => &["grab", "reposition"],
        ObjectKind::XdgPositioner => &[
            "set_size",
            "set_anchor_rect",
            "set_anchor",
            "set_gravity",
            "set_constraint_adjustment",
            "set_offset",
            "set_reactive",
            "set_parent_size",
            "set_parent_configure",
        ],
        ObjectKind::Seat => &["get_pointer", "get_keyboard", "get_touch"],
        ObjectKind::Pointer => &["set_cursor"],
        ObjectKind::DataDeviceManager => &["create_data_source", "get_data_device"],
        ObjectKind::DataDevice => &["start_drag", "set_selection"],
        ObjectKind::DataSource => &["offer", "set_actions"],
        ObjectKind::DataOffer => &["accept", "receive", "set_actions", "finish"],
        ObjectKind::TextInputManager => &["get_text_input"],
        ObjectKind::TextInput => &[
            "enable",
            "disable",
            "set_surrounding_text",
            "set_text_change_cause",
            "set_content_type",
            "set_cursor_rectangle",
            "commit",
        ],
    }
}

#[test]
fn tracks_surface_output_membership_across_mapping_and_release() {
    let mut fixture = Fixture::new();
    for id in [80, 81] {
        replace_record(&mut fixture, id, &wl::WL_OUTPUT_INTERFACE);
    }
    fixture.compositor.outputs.push(80);
    fixture
        .compositor
        .surfaces
        .get_mut(&10)
        .unwrap()
        .pending
        .attach = Some(Some(51));
    fixture
        .compositor
        .commit_surface_now(&mut fixture.client, 10)
        .unwrap();
    assert!(fixture.compositor.surfaces[&10].mapped);
    assert_eq!(fixture.compositor.surfaces[&10].outputs, [80]);
    assert_eq!(
        surface_output_events(
            &outbound_messages(&mut fixture.compositor, &mut fixture.client),
            "enter"
        ),
        [(10, 80)]
    );
    fixture.compositor.take_events();

    fixture
        .compositor
        .commit_surface_now(&mut fixture.client, 10)
        .unwrap();
    assert!(fixture.compositor.take_events().is_empty());
    assert!(outbound_messages(&mut fixture.compositor, &mut fixture.client).is_empty());

    fixture.compositor.outputs.push(81);
    fixture.compositor.set_surface_mapped(10, true);
    assert_eq!(fixture.compositor.surfaces[&10].outputs, [80, 81]);
    assert_eq!(
        surface_output_events(
            &outbound_messages(&mut fixture.compositor, &mut fixture.client),
            "enter"
        ),
        [(10, 81)]
    );

    assert!(fixture
        .compositor
        .dispatch(
            &mut fixture.client,
            &request(
                5,
                &xdg::XDG_WM_BASE_INTERFACE,
                "get_xdg_surface",
                6,
                vec![
                    Value::NewId {
                        id: 90,
                        interface: Some(&xdg::XDG_SURFACE_INTERFACE),
                    },
                    Value::Object(Some(10)),
                ],
            ),
            ObjectKind::XdgWmBase,
        )
        .is_err());

    fixture
        .compositor
        .surfaces
        .get_mut(&10)
        .unwrap()
        .pending
        .attach = Some(None);
    fixture
        .compositor
        .commit_surface_now(&mut fixture.client, 10)
        .unwrap();
    assert!(!fixture.compositor.surfaces[&10].mapped);
    assert!(fixture.compositor.surfaces[&10].outputs.is_empty());
    assert_eq!(
        surface_output_events(
            &outbound_messages(&mut fixture.compositor, &mut fixture.client),
            "leave"
        ),
        [(10, 80), (10, 81)]
    );
    fixture.compositor.take_events();

    fixture
        .compositor
        .surfaces
        .get_mut(&10)
        .unwrap()
        .pending
        .attach = Some(Some(52));
    fixture
        .compositor
        .commit_surface_now(&mut fixture.client, 10)
        .unwrap();
    assert_eq!(
        surface_output_events(
            &outbound_messages(&mut fixture.compositor, &mut fixture.client),
            "enter"
        ),
        [(10, 80), (10, 81)]
    );
    fixture
        .compositor
        .finish_destructor(&mut fixture.client, 80, ObjectKind::Output)
        .unwrap();
    assert_eq!(fixture.compositor.outputs, [81]);
    assert_eq!(fixture.compositor.surfaces[&10].outputs, [81]);
    assert_eq!(
        surface_output_events(
            &outbound_messages(&mut fixture.compositor, &mut fixture.client),
            "leave"
        ),
        [(10, 80)]
    );

    fixture
        .compositor
        .surfaces
        .get_mut(&10)
        .unwrap()
        .pending
        .attach = Some(None);
    fixture
        .compositor
        .commit_surface_now(&mut fixture.client, 10)
        .unwrap();
    assert_eq!(
        surface_output_events(
            &outbound_messages(&mut fixture.compositor, &mut fixture.client),
            "leave"
        ),
        [(10, 81)]
    );
}

fn surface_output_events(messages: &[WaylandMessage], name: &str) -> Vec<(u32, u32)> {
    let opcode = event_opcode(&wl::WL_SURFACE_INTERFACE, name);
    messages
        .iter()
        .filter(|message| message.object_id() != 1 && message.opcode() == opcode)
        .map(|message| {
            (
                message.object_id(),
                u32::from_le_bytes(message.payload()[..4].try_into().unwrap()),
            )
        })
        .collect()
}

#[test]
fn encodes_keyboard_keymap_and_input_events_with_full_arguments() {
    let mut fixture = Fixture::new();
    replace_record(&mut fixture, 70, &wl::WL_SEAT_INTERFACE);
    fixture.compositor.seats.insert(70, Seat::default());
    fixture
        .compositor
        .dispatch(
            &mut fixture.client,
            &request(
                70,
                &wl::WL_SEAT_INTERFACE,
                "get_keyboard",
                7,
                vec![Value::NewId {
                    id: 61,
                    interface: Some(&wl::WL_KEYBOARD_INTERFACE),
                }],
            ),
            ObjectKind::Seat,
        )
        .unwrap();
    fixture.compositor.keyboard_focus(Some(1)).unwrap();
    fixture.compositor.key(42, true, 102).unwrap();
    let messages = outbound_messages(&mut fixture.compositor, &mut fixture.client);
    let words = |opcode: u16| {
        messages
            .iter()
            .find(|message| message.object_id() == 61 && message.opcode() == opcode)
            .unwrap_or_else(|| panic!("missing keyboard event {opcode} in {messages:?}"))
            .payload()
            .as_chunks::<4>()
            .0
            .iter()
            .map(|bytes| u32::from_le_bytes(*bytes))
            .collect::<Vec<_>>()
    };

    let keymap = words(event_opcode(&wl::WL_KEYBOARD_INTERFACE, "keymap"));
    assert_eq!(keymap[0], 1);
    assert_eq!(keymap[1] as usize, DEFAULT_KEYMAP.len());
    assert_eq!(
        words(event_opcode(&wl::WL_KEYBOARD_INTERFACE, "repeat_info")),
        [25, 600]
    );
    let enter = words(event_opcode(&wl::WL_KEYBOARD_INTERFACE, "enter"));
    assert_eq!(enter[1], 10);
    assert_eq!(enter[2], 0);
    assert_eq!(enter.len(), 3);
    assert_eq!(
        words(event_opcode(&wl::WL_KEYBOARD_INTERFACE, "modifiers")).len(),
        5
    );
    let key = words(event_opcode(&wl::WL_KEYBOARD_INTERFACE, "key"));
    assert_eq!(key.len(), 4);
    assert_eq!(key[1], 102);
    assert_eq!(key[2..], [42, 1]);
}

#[test]
fn retired_shm_pool_keeps_wire_mapping_for_late_diffs() {
    let mut fixture = Fixture::new();
    for id in [51, 52, 53] {
        fixture.compositor.buffers.get_mut(&id).unwrap().alive = false;
    }
    fixture
        .compositor
        .finish_destructor(&mut fixture.client, 50, ObjectKind::ShmPool)
        .unwrap();
    assert!(fixture.compositor.pools[&50].destroyed);
    fixture.compositor.retire_orphan_buffers();
    assert!(!fixture.compositor.pools.contains_key(&50));
    assert!(fixture.compositor.buffers.is_empty());
    assert_eq!(
        fixture.client.resources().data(Rid(1)),
        Some(&[0, 255, 0, 0][..])
    );

    let mut payload = Vec::new();
    payload.extend_from_slice(&Rid(1).0.to_le_bytes());
    payload.extend_from_slice(&12u32.to_le_bytes());
    payload.extend_from_slice(&0u32.to_le_bytes());
    payload.extend_from_slice(&0u32.to_le_bytes());
    payload.extend_from_slice(&1u32.to_le_bytes());
    payload.extend_from_slice(&[255, 0, 0, 0]);
    let frame = waypipe_client::encode_frame(MessageType::BufferDiff, &payload).unwrap();
    fixture.client.receive(&frame).unwrap();
    assert_eq!(
        fixture.client.resources().data(Rid(1)),
        Some(&[255, 0, 0, 0][..])
    );
    assert_eq!(fixture.client.resources().allocated_file_size(), 4);
}

#[test]
fn emits_and_inherits_wayland_buffer_scales() {
    let mut fixture = Fixture::new();
    for id in [10, 11, 12] {
        replace_record(&mut fixture, id, &wl::WL_SURFACE_INTERFACE);
        fixture.compositor.surfaces.get_mut(&id).unwrap().window_id = Some(1);
    }
    fixture.compositor.output_scale = 2;
    fixture
        .compositor
        .dispatch(
            &mut fixture.client,
            &request(
                5,
                &wl::WL_COMPOSITOR_INTERFACE,
                "create_surface",
                6,
                vec![Value::NewId {
                    id: 70,
                    interface: Some(&wl::WL_SURFACE_INTERFACE),
                }],
            ),
            ObjectKind::Compositor,
        )
        .unwrap();
    let messages = outbound_messages(&mut fixture.compositor, &mut fixture.client);
    assert_eq!(
        scale_events(
            &messages,
            &wl::WL_SURFACE_INTERFACE,
            "preferred_buffer_scale",
            &[10, 11, 12, 13, 70]
        ),
        [(70, 2)]
    );

    fixture.compositor.set_window_scale(1, 3).unwrap();
    let messages = outbound_messages(&mut fixture.compositor, &mut fixture.client);
    assert_eq!(
        scale_events(
            &messages,
            &wl::WL_SURFACE_INTERFACE,
            "preferred_buffer_scale",
            &[10, 11, 12, 13, 70]
        ),
        [(10, 3), (11, 3), (12, 3)]
    );
    fixture.compositor.set_window_scale(1, 3).unwrap();
    assert!(outbound_messages(&mut fixture.compositor, &mut fixture.client).is_empty());

    replace_record(&mut fixture, 13, &wl::WL_SURFACE_INTERFACE);
    fixture.compositor.surfaces.insert(
        13,
        Surface {
            window_id: Some(90),
            ..Surface::default()
        },
    );
    fixture.compositor.windows.insert(
        90,
        Window {
            id: 90,
            xdg_surface_id: 31,
            surface_id: 13,
            kind: WindowKind::Popup { parent: 1 },
            title: String::new(),
            app_id: String::new(),
            geometry: Rect::default(),
        },
    );
    fixture.compositor.emit_window_preferred_scale(90);
    assert_eq!(
        scale_events(
            &outbound_messages(&mut fixture.compositor, &mut fixture.client),
            &wl::WL_SURFACE_INTERFACE,
            "preferred_buffer_scale",
            &[10, 11, 12, 13, 70]
        ),
        [(13, 3)]
    );
    fixture.compositor.set_window_scale(90, 2).unwrap();
    assert_eq!(
        scale_events(
            &outbound_messages(&mut fixture.compositor, &mut fixture.client),
            &wl::WL_SURFACE_INTERFACE,
            "preferred_buffer_scale",
            &[10, 11, 12, 13, 70]
        ),
        [(13, 2)]
    );
    fixture.compositor.set_window_scale(1, 5).unwrap();
    assert_eq!(
        scale_events(
            &outbound_messages(&mut fixture.compositor, &mut fixture.client),
            &wl::WL_SURFACE_INTERFACE,
            "preferred_buffer_scale",
            &[10, 11, 12, 13, 70]
        ),
        [(10, 5), (11, 5), (12, 5)]
    );

    replace_record(&mut fixture, 80, &wl::WL_OUTPUT_INTERFACE);
    fixture.compositor.outputs.push(80);
    fixture.compositor.set_output_scale(2).unwrap();
    let messages = outbound_messages(&mut fixture.compositor, &mut fixture.client);
    assert_eq!(
        scale_events(&messages, &wl::WL_OUTPUT_INTERFACE, "scale", &[80]),
        [(80, 2)]
    );
    assert!(messages.iter().any(|message| message.object_id() == 80
        && message.opcode() == event_opcode(&wl::WL_OUTPUT_INTERFACE, "done")));
    assert_eq!(messages.len(), 7);
    assert_eq!(
        scale_events(
            &messages,
            &wl::WL_SURFACE_INTERFACE,
            "preferred_buffer_scale",
            &[10, 11, 12, 13, 70]
        ),
        [(10, 2), (11, 2), (12, 2), (13, 2), (70, 2)]
    );
    assert_eq!(fixture.compositor.output_scale(), 2);
    assert!(fixture.compositor.window_scales.is_empty());

    fixture.compositor.set_output_scale(1).unwrap();
    let messages = outbound_messages(&mut fixture.compositor, &mut fixture.client);
    assert_eq!(
        scale_events(&messages, &wl::WL_OUTPUT_INTERFACE, "scale", &[80]),
        [(80, 1)]
    );
    assert_eq!(
        scale_events(
            &messages,
            &wl::WL_SURFACE_INTERFACE,
            "preferred_buffer_scale",
            &[10, 11, 12, 13, 70]
        ),
        [(10, 1), (11, 1), (12, 1), (13, 1), (70, 1)]
    );
    assert!(fixture.compositor.set_output_scale(0).is_err());
    assert_eq!(fixture.compositor.output_scale(), 1);
}

fn scale_events(
    messages: &[WaylandMessage],
    interface: &'static Interface,
    name: &str,
    objects: &[u32],
) -> Vec<(u32, i32)> {
    let opcode = event_opcode(interface, name);
    messages
        .iter()
        .filter(|message| {
            objects.contains(&message.object_id())
                && message.opcode() == opcode
                && message.payload().len() >= 4
        })
        .map(|message| {
            (
                message.object_id(),
                i32::from_le_bytes(message.payload()[..4].try_into().unwrap()),
            )
        })
        .collect()
}
