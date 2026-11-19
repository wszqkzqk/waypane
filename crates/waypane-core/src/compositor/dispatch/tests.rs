use super::*;
use waypipe_client::{ClientConfig, Compression, WAYPIPE_PROTOCOL_VERSION};

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
                geometry: None,
                configure_serial: Some(1),
                initial_ack_seen: true,
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
        .complete_frame(&mut fixture.client, child_frame.id, 10)
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
    assert_eq!(fixture.compositor.pointer_focus, Some((1, 13, 1.5, 1.5)));
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
        xdg.configure_serial = Some(777);
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
