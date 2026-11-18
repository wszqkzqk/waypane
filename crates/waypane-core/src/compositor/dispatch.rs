use super::*;

impl Compositor {
    pub(super) fn emit(
        &mut self,
        interface: &Interface,
        name: &str,
        object_id: u32,
        args: &[Argument<'_>],
    ) {
        let message = event(object_id, event_opcode(interface, name), args);
        self.outbound.push(message);
    }

    pub(super) fn emit_time(
        &mut self,
        interface: &Interface,
        name: &str,
        object_id: u32,
        time: u32,
    ) {
        self.emit(interface, name, object_id, &[Argument::Uint(time)]);
    }

    pub(super) fn emit_modifiers(&mut self, keyboard: u32) {
        let serial = self.next_serial();
        let (depressed, locked) = self.modifiers.masks();
        self.emit(
            &wl::WL_KEYBOARD_INTERFACE,
            "modifiers",
            keyboard,
            &[
                Argument::Uint(serial),
                Argument::Uint(depressed),
                Argument::Uint(0),
                Argument::Uint(locked),
                Argument::Uint(0),
            ],
        );
    }

    pub(super) fn next_serial(&mut self) -> u32 {
        let serial = self.serial;
        self.serial = self.serial.wrapping_add(1);
        if self.serial == 0 {
            self.serial = 1;
        }
        serial
    }

    pub(super) fn send_configure(
        &mut self,
        toplevel: WindowId,
        xdg_surface: u32,
        size: Option<Size>,
        states: &[u32],
        serial: u32,
    ) {
        let (width, height) = size
            .map(|size| (size.width as i32, size.height as i32))
            .unwrap_or((0, 0));
        let states = states
            .iter()
            .flat_map(|state| state.to_le_bytes())
            .collect::<Vec<_>>();
        self.emit(
            &xdg::XDG_TOPLEVEL_INTERFACE,
            "configure",
            toplevel,
            &[
                Argument::Int(width),
                Argument::Int(height),
                Argument::Array(&states),
            ],
        );
        self.emit(
            &xdg::XDG_SURFACE_INTERFACE,
            "configure",
            xdg_surface,
            &[Argument::Uint(serial)],
        );
    }

    pub(super) fn retire_orphan_buffers(&mut self, client: &mut Client) {
        let attached = self
            .surfaces
            .values()
            .filter_map(|surface| surface.committed.buffer)
            .collect::<std::collections::BTreeSet<_>>();
        let retired = self
            .buffers
            .iter()
            .filter(|(id, buffer)| !buffer.alive && !attached.contains(id))
            .map(|(id, buffer)| (*id, buffer.pool_id))
            .collect::<Vec<_>>();
        for (id, pool_id) in retired {
            self.buffers.remove(&id);
            self.unref_buffer(client, pool_id);
        }
    }

    pub(super) fn dispatch(
        &mut self,
        client: &mut Client,
        request: &DecodedRequest,
        kind: ObjectKind,
    ) -> Result<()> {
        match (kind, request.method.name) {
            (ObjectKind::Display, "sync") => {
                let callback = self.create_new(request, 0)?;
                self.objects.remove(&callback);
                self.emit_time(&wl::WL_CALLBACK_INTERFACE, "done", callback, 0);
                self.emit(
                    &wl::WL_DISPLAY_INTERFACE,
                    "delete_id",
                    1,
                    &[Argument::Uint(callback)],
                );
            }
            (ObjectKind::Display, "get_registry") => {
                let registry = self.create_new(request, 0)?;
                for (index, (interface, version)) in GLOBALS.iter().enumerate() {
                    self.emit(
                        &wl::WL_REGISTRY_INTERFACE,
                        "global",
                        registry,
                        &[
                            Argument::Uint((index + 1) as u32),
                            Argument::Str(Some(interface)),
                            Argument::Uint(*version),
                        ],
                    );
                }
            }
            (ObjectKind::Registry, "bind") => self.bind(request)?,
            (ObjectKind::Compositor, "create_surface") => {
                let id = self.create_new(request, 0)?;
                self.surfaces.insert(id, Surface::default());
            }
            (ObjectKind::Compositor, "create_region") => {
                self.create_new(request, 0)?;
            }
            (ObjectKind::Subcompositor, "get_subsurface") => {
                let id = self.create_new(request, 0)?;
                let surface = object_id(request, 1)?;
                let parent = object_id(request, 2)?;
                if !self.surfaces.contains_key(&surface) || !self.surfaces.contains_key(&parent) {
                    return Err(bad(request, "unknown surface or parent surface"));
                }
                if surface == parent {
                    return Err(bad(request, "surface cannot be its own parent"));
                }
                if self.surfaces[&surface].role != Role::None {
                    return Err(bad(request, "surface already has a role"));
                }
                let mut ancestor = Some(parent);
                for _ in 0..64 {
                    let Some(id) = ancestor else { break };
                    if id == surface {
                        return Err(bad(request, "subsurface parent loop"));
                    }
                    ancestor = match self.surfaces[&id].role {
                        Role::Subsurface { parent, .. } => Some(parent),
                        _ => None,
                    };
                }
                if ancestor.is_some() {
                    return Err(bad(request, "subsurface hierarchy exceeds 64 levels"));
                }
                let window_id = self.surfaces[&parent].window_id;
                let entry = self.surfaces.get_mut(&surface).unwrap();
                entry.role = Role::Subsurface { parent, proxy: id };
                entry.window_id = window_id;
                self.subsurface_sync.insert(
                    id,
                    SubsurfaceSync {
                        surface,
                        synchronized: true,
                        cached: Default::default(),
                    },
                );
                self.subsurface_stacks
                    .entry(parent)
                    .or_default()
                    .push(surface);
            }
            (ObjectKind::Subsurface, "set_position") => {
                let surface = self.subsurface_surface(request.object_id)?;
                self.surfaces.get_mut(&surface).unwrap().pending.position = Some(Point {
                    x: int(request, 0)?,
                    y: int(request, 1)?,
                });
            }
            (ObjectKind::Subsurface, "place_above" | "place_below") => {
                self.restack_subsurface(request)?;
            }
            (ObjectKind::Subsurface, "set_sync" | "set_desync") => {
                let synchronized = request.method.name == "set_sync";
                self.subsurface_sync
                    .get_mut(&request.object_id)
                    .ok_or_else(|| bad(request, "unknown subsurface"))?
                    .synchronized = synchronized;
            }
            (ObjectKind::Subsurface, "destroy") => {
                let surface = self.subsurface_surface(request.object_id)?;
                if let Some(surface) = self.surfaces.get_mut(&surface) {
                    surface.role = Role::None;
                    surface.window_id = None;
                }
                self.subsurface_sync.remove(&request.object_id);
                for stack in self.subsurface_stacks.values_mut() {
                    stack.retain(|id| *id != surface);
                }
            }
            (ObjectKind::Surface, "destroy") => {
                if self.surfaces[&request.object_id].role != Role::None {
                    return Err(bad(request, "surface role object must be destroyed first"));
                }
            }
            (ObjectKind::Surface, "attach") => {
                let buffer = optional_object_id(request, 0)?;
                if let Some(buffer) = buffer {
                    if !self.buffers.get(&buffer).is_some_and(|buffer| buffer.alive) {
                        return Err(bad(request, "attaching unknown or destroyed buffer"));
                    }
                }
                let offset = Point {
                    x: int(request, 1)?,
                    y: int(request, 2)?,
                };
                let pending = &mut self.surfaces.get_mut(&request.object_id).unwrap().pending;
                pending.attach = Some(buffer);
                pending.offset = Some(offset);
            }
            (ObjectKind::Surface, "damage" | "damage_buffer") => {
                let rect = rect(request, 0)?;
                self.surfaces
                    .get_mut(&request.object_id)
                    .unwrap()
                    .pending
                    .damage
                    .push((rect, request.method.name == "damage_buffer"));
            }
            (ObjectKind::Surface, "frame") => {
                let callback = self.create_new(request, 0)?;
                self.surfaces
                    .get_mut(&request.object_id)
                    .unwrap()
                    .pending
                    .callbacks
                    .push(callback);
            }
            (ObjectKind::Surface, "commit") => self.commit_surface(client, request.object_id)?,
            (ObjectKind::Surface, "set_buffer_transform") => {
                let transform = int(request, 0)?;
                if !(0..=7).contains(&transform) {
                    return Err(bad(request, "invalid buffer transform"));
                }
                self.surfaces
                    .get_mut(&request.object_id)
                    .unwrap()
                    .pending
                    .transform = Some(transform as u32);
            }
            (ObjectKind::Surface, "set_buffer_scale") => {
                let scale = int(request, 0)?;
                if scale <= 0 {
                    return Err(bad(request, "buffer scale must be positive"));
                }
                self.surfaces
                    .get_mut(&request.object_id)
                    .unwrap()
                    .pending
                    .scale = Some(scale);
            }
            (ObjectKind::Surface, "offset") => {
                self.surfaces
                    .get_mut(&request.object_id)
                    .unwrap()
                    .pending
                    .offset = Some(Point {
                    x: int(request, 0)?,
                    y: int(request, 1)?,
                });
            }
            (ObjectKind::Surface, "set_opaque_region" | "set_input_region") => {}
            (ObjectKind::Shm, "create_pool") => {
                let id = self.create_new(request, 0)?;
                let rid = fd(request, 1)?;
                let size = int(request, 2)?;
                let data = client
                    .resources()
                    .data(rid)
                    .ok_or_else(|| bad(request, "shm pool has no backing resource"))?;
                if size <= 0 || usize::try_from(size).ok() != Some(data.len()) {
                    return Err(bad(
                        request,
                        "shm pool size does not match backing resource",
                    ));
                }
                self.pools.insert(
                    id,
                    Pool {
                        rid,
                        size: data.len(),
                        buffer_refs: 0,
                        destroyed: false,
                    },
                );
            }
            (ObjectKind::ShmPool, "create_buffer") => self.create_buffer(client, request)?,
            (ObjectKind::ShmPool, "resize") => {
                let size = int(request, 0)?;
                let pool = self
                    .pools
                    .get(&request.object_id)
                    .ok_or_else(|| bad(request, "unknown pool"))?;
                let available = client.resources().data(pool.rid).map_or(0, <[u8]>::len);
                if size <= 0
                    || usize::try_from(size).ok() != Some(available)
                    || (size as usize) < pool.size
                {
                    return Err(bad(request, "invalid shm pool resize"));
                }
                self.pools.get_mut(&request.object_id).unwrap().size = size as usize;
            }
            (ObjectKind::ShmPool, "destroy") => {}
            (ObjectKind::Buffer, "destroy") => {
                self.buffers.get_mut(&request.object_id).unwrap().alive = false;
            }
            (ObjectKind::XdgWmBase, "create_positioner") => {
                let id = self.create_new(request, 0)?;
                self.positioners.insert(id, Positioner::default());
            }
            (ObjectKind::XdgWmBase, "get_xdg_surface") => {
                let id = self.create_new(request, 0)?;
                let surface = object_id(request, 1)?;
                let Some(entry) = self.surfaces.get(&surface) else {
                    return Err(bad(request, "unknown wl_surface"));
                };
                if entry.role != Role::None
                    || self
                        .xdg_surfaces
                        .values()
                        .any(|xdg| xdg.surface_id == surface)
                {
                    return Err(bad(request, "surface already has a role"));
                }
                self.xdg_surfaces.insert(
                    id,
                    XdgSurface {
                        surface_id: surface,
                        window_id: None,
                        geometry: None,
                        configure_serial: None,
                        initial_ack_seen: false,
                    },
                );
            }
            (ObjectKind::XdgWmBase, "pong") => {}
            (ObjectKind::XdgWmBase, "destroy") => {}
            (ObjectKind::XdgSurface, "get_toplevel") => {
                let id = self.create_new(request, 0)?;
                let xdg_surface = self
                    .xdg_surfaces
                    .get_mut(&request.object_id)
                    .ok_or_else(|| bad(request, "unknown xdg_surface"))?;
                if xdg_surface.window_id.is_some() {
                    return Err(bad(request, "xdg_surface already has a role"));
                }
                let surface_id = xdg_surface.surface_id;
                xdg_surface.window_id = Some(id);
                {
                    let surface = self.surfaces.get_mut(&surface_id).unwrap();
                    surface.role = Role::Toplevel(id);
                    surface.window_id = Some(id);
                }
                let window = Window {
                    id,
                    xdg_surface_id: request.object_id,
                    surface_id,
                    kind: WindowKind::Toplevel,
                    title: String::new(),
                    app_id: String::new(),
                    geometry: Rect::default(),
                };
                self.windows.insert(id, window);
                let serial = self.next_serial();
                self.xdg_surfaces
                    .get_mut(&request.object_id)
                    .unwrap()
                    .configure_serial = Some(serial);
                self.send_configure(id, request.object_id, None, &[], serial);
                let info = self.window_info(id).unwrap();
                self.events.push(CoreEvent::WindowCreated(info));
            }
            (ObjectKind::XdgSurface, "get_popup") => self.create_popup(request)?,
            (ObjectKind::XdgSurface, "set_window_geometry") => {
                let geometry = rect(request, 0)?;
                let window_id = self
                    .xdg_surfaces
                    .get_mut(&request.object_id)
                    .ok_or_else(|| bad(request, "unknown xdg_surface"))?
                    .window_id;
                if let Some(window_id) = window_id {
                    let window = self.windows.get_mut(&window_id).unwrap();
                    window.geometry = geometry;
                    self.events.push(CoreEvent::WindowMetadata {
                        id: window_id,
                        title: window.title.clone(),
                        app_id: window.app_id.clone(),
                        geometry,
                    });
                }
            }
            (ObjectKind::XdgSurface, "ack_configure") => {
                let serial = uint(request, 0)?;
                let xdg_surface = self
                    .xdg_surfaces
                    .get_mut(&request.object_id)
                    .ok_or_else(|| bad(request, "unknown xdg_surface"))?;
                if xdg_surface.configure_serial != Some(serial) {
                    return Err(bad(request, "ack_configure does not match latest serial"));
                }
                xdg_surface.initial_ack_seen = true;
            }
            (ObjectKind::XdgSurface, "destroy") => {
                if self.xdg_surfaces[&request.object_id].window_id.is_some() {
                    return Err(bad(request, "toplevel or popup must be destroyed first"));
                }
            }
            (ObjectKind::XdgToplevel, "set_title" | "set_app_id") => {
                let value = string(request, 0)?.to_owned();
                let window = self.windows.get_mut(&request.object_id).unwrap();
                if request.method.name == "set_title" {
                    window.title = value;
                } else {
                    window.app_id = value;
                }
                self.events.push(CoreEvent::WindowMetadata {
                    id: request.object_id,
                    title: window.title.clone(),
                    app_id: window.app_id.clone(),
                    geometry: window.geometry,
                });
            }
            (
                ObjectKind::XdgToplevel,
                "set_maximized" | "unset_maximized" | "set_fullscreen" | "unset_fullscreen"
                | "set_minimized",
            ) => {
                let request_kind = match request.method.name {
                    "set_maximized" => WindowRequest::Maximized,
                    "unset_maximized" => WindowRequest::Unmaximized,
                    "set_fullscreen" => WindowRequest::Fullscreen(optional_object_id(request, 0)?),
                    "unset_fullscreen" => WindowRequest::Unfullscreen,
                    _ => WindowRequest::Minimized,
                };
                let states = match request_kind {
                    WindowRequest::Maximized => vec![1, 4],
                    WindowRequest::Fullscreen(_) => vec![2, 4],
                    WindowRequest::Unmaximized | WindowRequest::Unfullscreen => vec![4],
                    WindowRequest::Minimized => Vec::new(),
                    _ => unreachable!(),
                };
                if !states.is_empty() {
                    let xdg_surface = self.windows[&request.object_id].xdg_surface_id;
                    let serial = self.next_serial();
                    let config = self.xdg_surfaces.get_mut(&xdg_surface).unwrap();
                    config.configure_serial = Some(serial);
                    self.send_configure(request.object_id, xdg_surface, None, &states, serial);
                }
                self.events.push(CoreEvent::WindowRequest {
                    id: request.object_id,
                    request: request_kind,
                });
            }
            (ObjectKind::XdgToplevel, "move" | "resize") => {
                let request_kind = if request.method.name == "move" {
                    WindowRequest::Move
                } else {
                    WindowRequest::Resize {
                        edges: uint(request, 2)?,
                    }
                };
                self.events.push(CoreEvent::WindowRequest {
                    id: request.object_id,
                    request: request_kind,
                });
            }
            (
                ObjectKind::XdgToplevel,
                "set_parent" | "set_max_size" | "set_min_size" | "show_window_menu" | "destroy",
            ) => {}
            (ObjectKind::XdgPopup, "set_position" | "reposition" | "destroy") => {}
            (ObjectKind::XdgPositioner, "set_size") => {
                self.positioners.get_mut(&request.object_id).unwrap().size =
                    Some((int(request, 0)?, int(request, 1)?));
            }
            (ObjectKind::XdgPositioner, "set_anchor_rect") => {
                self.positioners
                    .get_mut(&request.object_id)
                    .unwrap()
                    .anchor_rect = Some(rect(request, 0)?);
            }
            (ObjectKind::XdgPositioner, "set_offset") => {
                self.positioners.get_mut(&request.object_id).unwrap().offset = Point {
                    x: int(request, 0)?,
                    y: int(request, 1)?,
                };
            }
            (
                ObjectKind::XdgPositioner,
                "destroy"
                | "set_anchor"
                | "set_gravity"
                | "set_constraint_adjustment"
                | "set_reactive"
                | "set_parent_size"
                | "set_parent_configure",
            ) => {}
            (ObjectKind::Seat, "get_pointer") => {
                let id = self.create_new(request, 0)?;
                self.seats
                    .get_mut(&request.object_id)
                    .unwrap()
                    .pointers
                    .push(id);
            }
            (ObjectKind::Seat, "get_keyboard") => {
                let id = self.create_new(request, 0)?;
                self.seats
                    .get_mut(&request.object_id)
                    .unwrap()
                    .keyboards
                    .push(id);
                let rid = client.create_readonly_file(self.keymap.clone())?;
                self.outbound.push(event_with_fd(
                    id,
                    event_opcode(&wl::WL_KEYBOARD_INTERFACE, "keymap"),
                    &[Argument::Uint(1), Argument::Uint(self.keymap.len() as u32)],
                    rid,
                ));
                if self.objects[&id].version >= 4 {
                    self.emit(
                        &wl::WL_KEYBOARD_INTERFACE,
                        "repeat_info",
                        id,
                        &[Argument::Int(25), Argument::Int(600)],
                    );
                }
                if let Some(surface) = self
                    .keyboard_focus
                    .and_then(|window| self.windows.get(&window))
                    .map(|window| window.surface_id)
                {
                    let serial = self.next_serial();
                    self.emit(
                        &wl::WL_KEYBOARD_INTERFACE,
                        "enter",
                        id,
                        &[
                            Argument::Uint(serial),
                            Argument::Object(surface),
                            Argument::Array(&[]),
                        ],
                    );
                    self.emit_modifiers(id);
                }
            }
            (ObjectKind::Seat, "get_touch") => {
                let id = self.create_new(request, 0)?;
                self.seats
                    .get_mut(&request.object_id)
                    .unwrap()
                    .touches
                    .push(id);
            }
            (ObjectKind::Seat, "release") => {}
            (ObjectKind::Region, "add" | "subtract" | "destroy") => {}
            (ObjectKind::Output, "release") => {}
            (ObjectKind::DataDeviceManager, "create_data_source") => {
                self.create_data_source(request)?;
            }
            (ObjectKind::DataDeviceManager, "get_data_device") => {
                self.create_data_device(request)?;
            }
            (ObjectKind::DataSource, "offer") => self.offer_source_mime(request)?,
            (ObjectKind::DataSource, "set_actions" | "destroy") => {}
            (ObjectKind::DataDevice, "set_selection") => self.set_remote_selection(request)?,
            (ObjectKind::DataDevice, "start_drag" | "release") => {}
            (ObjectKind::DataOffer, "receive") => self.receive_offer(client, request)?,
            (ObjectKind::DataOffer, "set_actions" | "finish" | "destroy") => {}
            (ObjectKind::TextInputManager, "get_text_input") => {
                self.create_text_input(request)?;
            }
            (ObjectKind::TextInput, _) => self.handle_text_input_request(request)?,
            (_, "destroy" | "release") => {}
            _ => {
                return Err(Error::protocol(format!(
                    "unsupported request {}.{}",
                    request.interface.name, request.method.name
                )));
            }
        }
        Ok(())
    }

    fn bind(&mut self, request: &DecodedRequest) -> Result<()> {
        let name = uint(request, 0)?;
        let interface = string(request, 1)?;
        let version = uint(request, 2)?;
        let (id, _) = request
            .args
            .get(3)
            .and_then(Value::new_id)
            .ok_or_else(|| bad(request, "missing new_id"))?;
        let Some((expected, max_version)) = name
            .checked_sub(1)
            .and_then(|index| GLOBALS.get(index as usize))
        else {
            return Err(bad(request, "unknown global name"));
        };
        if *expected != interface || version == 0 || version > *max_version {
            return Err(bad(request, "global interface or version mismatch"));
        }
        let record = global_interface(interface, version)
            .ok_or_else(|| bad(request, "unsupported interface"))?;
        self.insert_record(id, record)?;

        match interface {
            "wl_shm" => {
                self.emit(&wl::WL_SHM_INTERFACE, "format", id, &[Argument::Uint(0)]);
                self.emit(&wl::WL_SHM_INTERFACE, "format", id, &[Argument::Uint(1)]);
            }
            "wl_seat" => {
                self.seats.insert(id, Seat::default());
                self.emit(
                    &wl::WL_SEAT_INTERFACE,
                    "capabilities",
                    id,
                    &[Argument::Uint(7)],
                );
                if version >= 2 {
                    self.emit(
                        &wl::WL_SEAT_INTERFACE,
                        "name",
                        id,
                        &[Argument::Str(Some("waypane"))],
                    );
                }
            }
            "wl_output" => {
                self.outputs.push(id);
                self.emit(
                    &wl::WL_OUTPUT_INTERFACE,
                    "geometry",
                    id,
                    &[
                        Argument::Int(0),
                        Argument::Int(0),
                        Argument::Int(530),
                        Argument::Int(300),
                        Argument::Int(0),
                        Argument::Str(Some("Waypane")),
                        Argument::Str(Some("Remote Display")),
                        Argument::Int(0),
                    ],
                );
                self.emit(
                    &wl::WL_OUTPUT_INTERFACE,
                    "mode",
                    id,
                    &[
                        Argument::Uint(3),
                        Argument::Int(self.output_size.width as i32),
                        Argument::Int(self.output_size.height as i32),
                        Argument::Int(60_000),
                    ],
                );
                if version >= 2 {
                    self.emit(
                        &wl::WL_OUTPUT_INTERFACE,
                        "scale",
                        id,
                        &[Argument::Int(self.output_scale)],
                    );
                }
                if version >= 4 {
                    self.emit(
                        &wl::WL_OUTPUT_INTERFACE,
                        "name",
                        id,
                        &[Argument::Str(Some("Waypane-1"))],
                    );
                    self.emit(
                        &wl::WL_OUTPUT_INTERFACE,
                        "description",
                        id,
                        &[Argument::Str(Some("Waypane remote output"))],
                    );
                }
                if version >= 2 {
                    self.emit(&wl::WL_OUTPUT_INTERFACE, "done", id, &[]);
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn create_buffer(&mut self, client: &Client, request: &DecodedRequest) -> Result<()> {
        let id = self.create_new(request, 0)?;
        let pool = self
            .pools
            .get_mut(&request.object_id)
            .ok_or_else(|| bad(request, "unknown pool"))?;
        let buffer = Buffer {
            pool_id: request.object_id,
            rid: pool.rid,
            offset: int(request, 1)?,
            width: int(request, 2)?,
            height: int(request, 3)?,
            stride: int(request, 4)?,
            format: match uint(request, 5)? {
                0 => ShmFormat::Argb8888,
                1 => ShmFormat::Xrgb8888,
                _ => return Err(bad(request, "unsupported wl_shm format")),
            },
            alive: true,
        };
        let Some(required) = buffer.required_bytes() else {
            return Err(bad(request, "invalid buffer dimensions or stride"));
        };
        let available = client
            .resources()
            .data(pool.rid)
            .map_or(0, <[u8]>::len)
            .min(pool.size);
        if required > available {
            return Err(bad(request, "buffer exceeds pool bounds"));
        }
        pool.buffer_refs += 1;
        self.buffers.insert(id, buffer);
        Ok(())
    }

    fn create_popup(&mut self, request: &DecodedRequest) -> Result<()> {
        let id = self.create_new(request, 0)?;
        let parent_xdg = object_id(request, 1)?;
        let positioner = object_id(request, 2)?;
        let parent = self
            .xdg_surfaces
            .get(&parent_xdg)
            .and_then(|surface| surface.window_id)
            .ok_or_else(|| bad(request, "popup parent has no role"))?;
        let positioner = *self
            .positioners
            .get(&positioner)
            .ok_or_else(|| bad(request, "unknown xdg_positioner"))?;
        let (width, height) = positioner
            .size
            .ok_or_else(|| bad(request, "positioner size is not set"))?;
        if width <= 0 || height <= 0 {
            return Err(bad(request, "invalid positioner size"));
        }
        let anchor = positioner.anchor_rect.unwrap_or_default();
        let geometry = Rect {
            x: anchor.x + positioner.offset.x,
            y: anchor.y + positioner.offset.y,
            width,
            height,
        };
        let surface_id = self.xdg_surfaces[&request.object_id].surface_id;
        {
            let surface = self.surfaces.get_mut(&surface_id).unwrap();
            surface.role = Role::Popup(id);
            surface.window_id = Some(id);
        }
        self.xdg_surfaces
            .get_mut(&request.object_id)
            .unwrap()
            .window_id = Some(id);
        self.xdg_surfaces
            .get_mut(&request.object_id)
            .unwrap()
            .geometry = Some(geometry);
        self.windows.insert(
            id,
            Window {
                id,
                xdg_surface_id: request.object_id,
                surface_id,
                kind: WindowKind::Popup { parent },
                title: String::new(),
                app_id: String::new(),
                geometry,
            },
        );
        self.emit(
            &xdg::XDG_POPUP_INTERFACE,
            "configure",
            id,
            &[
                Argument::Int(geometry.x),
                Argument::Int(geometry.y),
                Argument::Int(width),
                Argument::Int(height),
            ],
        );
        let serial = self.next_serial();
        self.xdg_surfaces
            .get_mut(&request.object_id)
            .unwrap()
            .configure_serial = Some(serial);
        self.emit(
            &xdg::XDG_SURFACE_INTERFACE,
            "configure",
            request.object_id,
            &[Argument::Uint(serial)],
        );
        let info = self.window_info(id).unwrap();
        self.events.push(CoreEvent::WindowCreated(info));
        Ok(())
    }

    fn commit_surface_now(&mut self, client: &mut Client, surface_id: u32) -> Result<()> {
        if let Some(xdg_surface) = self
            .xdg_surfaces
            .values()
            .find(|xdg| xdg.surface_id == surface_id && xdg.window_id.is_some())
        {
            if xdg_surface.configure_serial.is_some() && !xdg_surface.initial_ack_seen {
                return Err(Error::protocol(
                    "xdg_surface committed before initial ack_configure",
                ));
            }
        }

        let (buffer_id, callbacks, scale, transform, damage, attach, window_id, role, position) = {
            let surface = self
                .surfaces
                .get_mut(&surface_id)
                .ok_or_else(|| Error::protocol("committing unknown surface"))?;
            let attach = surface.pending.attach.take();
            if let Some(buffer) = attach {
                surface.committed.buffer = buffer;
            }
            if let Some(offset) = surface.pending.offset.take() {
                surface.committed.offset = offset;
            }
            if let Some(scale) = surface.pending.scale.take() {
                surface.committed.scale = scale;
            }
            if let Some(transform) = surface.pending.transform.take() {
                surface.committed.transform = transform;
            }
            if let Some(position) = surface.pending.position.take() {
                surface.position = position;
            }
            (
                surface.committed.buffer,
                std::mem::take(&mut surface.pending.callbacks),
                surface.committed.scale,
                surface.committed.transform,
                std::mem::take(&mut surface.pending.damage),
                attach,
                surface.window_id,
                surface.role,
                surface.position,
            )
        };

        let buffer = match buffer_id {
            Some(buffer_id) => {
                Some(self.buffer_view(client, buffer_id, scale, transform, &damage)?)
            }
            None => None,
        };
        let parent = match role {
            Role::Subsurface { parent, .. } => Some((parent, position)),
            _ => None,
        };
        let z_order = self.subsurface_z_order(window_id, &role);
        if attach == Some(None) {
            if let Some(surface) = self.surfaces.get_mut(&surface_id) {
                surface.logical_size = Size::default();
            }
        } else if let Some(view) = &buffer {
            let (width, height) = if view.transform % 2 == 1 {
                (view.height, view.width)
            } else {
                (view.width, view.height)
            };
            if let Some(surface) = self.surfaces.get_mut(&surface_id) {
                surface.logical_size = Size {
                    width: (width / view.scale.max(1)).max(1) as u32,
                    height: (height / view.scale.max(1)).max(1) as u32,
                };
            }
        }

        if let Some(window_id) = window_id {
            if buffer.is_some() || attach == Some(None) || !callbacks.is_empty() {
                let id = self.next_frame_id;
                self.next_frame_id += 1;
                self.pending_frames.insert(
                    id,
                    PendingFrame {
                        surface_id,
                        buffer_id,
                        callbacks,
                    },
                );
                self.events.push(CoreEvent::Frame(Frame {
                    id,
                    window_id,
                    surface_id,
                    parent,
                    buffer,
                    z_order,
                }));
            }
        } else {
            if let Some(buffer_id) = buffer_id {
                if self.objects.contains_key(&buffer_id) {
                    self.emit(&wl::WL_BUFFER_INTERFACE, "release", buffer_id, &[]);
                }
            }
            for callback in callbacks {
                self.objects.remove(&callback);
                self.emit_time(&wl::WL_CALLBACK_INTERFACE, "done", callback, 0);
                self.emit(
                    &wl::WL_DISPLAY_INTERFACE,
                    "delete_id",
                    1,
                    &[Argument::Uint(callback)],
                );
            }
        }
        self.retire_orphan_buffers(client);
        Ok(())
    }

    fn commit_surface(&mut self, client: &mut Client, surface_id: u32) -> Result<()> {
        let role = self
            .surfaces
            .get(&surface_id)
            .map(|surface| surface.role)
            .ok_or_else(|| Error::protocol("committing unknown surface"))?;
        if let Role::Subsurface { proxy, .. } = role {
            let synchronized = self
                .subsurface_sync
                .get(&proxy)
                .ok_or_else(|| Error::protocol("subsurface has no synchronization state"))?
                .synchronized;
            let current = std::mem::take(
                &mut self
                    .surfaces
                    .get_mut(&surface_id)
                    .ok_or_else(|| Error::protocol("committing unknown surface"))?
                    .pending,
            );
            if synchronized {
                let cache = self.subsurface_sync.get_mut(&proxy).unwrap();
                if cache.cached.len() >= 64 {
                    return Err(Error::protocol("subsurface commit cache exceeds 64 frames"));
                }
                cache.cached.push_back(current);
                return Ok(());
            }
            let cached = std::mem::take(&mut self.subsurface_sync.get_mut(&proxy).unwrap().cached);
            for pending in cached {
                self.surfaces.get_mut(&surface_id).unwrap().pending = pending;
                self.commit_surface_now(client, surface_id)?;
                self.flush_synchronized_children(client, surface_id)?;
            }
            self.surfaces.get_mut(&surface_id).unwrap().pending = current;
        }
        self.commit_surface_now(client, surface_id)?;
        self.flush_synchronized_children(client, surface_id)
    }

    fn flush_synchronized_children(&mut self, client: &mut Client, parent_id: u32) -> Result<()> {
        let children = self
            .subsurface_sync
            .iter()
            .filter(|(_, sync)| {
                sync.synchronized
                    && self.surfaces.get(&sync.surface).is_some_and(|surface| {
                        matches!(surface.role, Role::Subsurface { parent, .. } if parent == parent_id)
                    })
            })
            .map(|(proxy, sync)| (*proxy, sync.surface))
            .collect::<Vec<_>>();
        for (proxy, surface_id) in children {
            while let Some(pending) = self
                .subsurface_sync
                .get_mut(&proxy)
                .unwrap()
                .cached
                .pop_front()
            {
                self.surfaces.get_mut(&surface_id).unwrap().pending = pending;
                self.commit_surface_now(client, surface_id)?;
                self.flush_synchronized_children(client, surface_id)?;
            }
        }
        Ok(())
    }

    fn restack_subsurface(&mut self, request: &DecodedRequest) -> Result<()> {
        let surface = self.subsurface_surface(request.object_id)?;
        let sibling = object_id(request, 0)?;
        if surface == sibling {
            return Err(bad(request, "cannot restack relative to itself"));
        }
        let Role::Subsurface { parent, .. } = self.surfaces[&surface].role else {
            return Err(bad(request, "not a subsurface"));
        };
        if sibling != parent {
            match self.surfaces.get(&sibling).map(|surface| surface.role) {
                Some(Role::Subsurface {
                    parent: sibling_parent,
                    ..
                }) if sibling_parent == parent => {}
                _ => return Err(bad(request, "sibling is not in the same parent surface")),
            }
        }
        let above = request.method.name == "place_above";
        let stack = self.subsurface_stacks.entry(parent).or_default();
        stack.retain(|id| *id != surface);
        let index = if sibling == parent {
            if above {
                0
            } else {
                stack.len()
            }
        } else {
            let position = stack
                .iter()
                .position(|id| *id == sibling)
                .ok_or_else(|| bad(request, "sibling is not in parent stack"))?;
            if above {
                position + 1
            } else {
                position
            }
        };
        stack.insert(index.min(stack.len()), surface);
        Ok(())
    }

    fn subsurface_z_order(&self, window_id: Option<WindowId>, role: &Role) -> Option<Vec<u32>> {
        let Role::Subsurface { parent, .. } = *role else {
            return None;
        };
        let root = self.windows.get(&window_id?)?.surface_id;
        let mut order = vec![root];
        self.collect_subsurface_order(parent, &mut order, 0);
        Some(order)
    }

    fn collect_subsurface_order(&self, parent: u32, order: &mut Vec<u32>, depth: usize) {
        if depth >= 64 {
            return;
        }
        if let Some(children) = self.subsurface_stacks.get(&parent) {
            for child in children.iter().copied() {
                if self.surfaces.contains_key(&child) {
                    order.push(child);
                    self.collect_subsurface_order(child, order, depth + 1);
                }
            }
        }
    }

    fn buffer_view(
        &self,
        client: &Client,
        buffer_id: u32,
        scale: i32,
        transform: u32,
        surface_damage: &[(Rect, bool)],
    ) -> Result<BufferView> {
        let buffer = self
            .buffers
            .get(&buffer_id)
            .ok_or_else(|| Error::protocol("committed buffer is unknown"))?
            .clone();
        let Some(required) = buffer.required_bytes() else {
            return Err(Error::protocol("invalid committed buffer"));
        };
        let data = client
            .resources()
            .data(buffer.rid)
            .ok_or_else(|| Error::protocol("buffer backing resource has disappeared"))?;
        if required > data.len() {
            return Err(Error::protocol("buffer exceeds shared-memory pool"));
        }

        let mut damage = Vec::new();
        for (rect, buffer_coords) in surface_damage {
            let scaled = if *buffer_coords {
                *rect
            } else {
                Rect {
                    x: rect.x.saturating_mul(scale),
                    y: rect.y.saturating_mul(scale),
                    width: rect.width.saturating_mul(scale),
                    height: rect.height.saturating_mul(scale),
                }
            };
            if let Some(rect) = clip_rect(scaled, buffer.width, buffer.height) {
                push_damage(&mut damage, rect);
            }
        }
        if damage.is_empty() {
            if let Some(pool_damage) = client.resources().damage(buffer.rid) {
                for range in pool_damage {
                    byte_range_damage(&buffer, range, &mut damage);
                }
            }
        }
        if damage.is_empty() {
            damage.push(Rect {
                x: 0,
                y: 0,
                width: buffer.width,
                height: buffer.height,
            });
        }

        Ok(BufferView {
            rid: buffer.rid,
            offset: buffer.offset,
            width: buffer.width,
            height: buffer.height,
            stride: buffer.stride,
            format: buffer.format,
            scale,
            transform,
            damage,
        })
    }

    fn subsurface_surface(&self, id: u32) -> Result<u32> {
        self.surfaces
            .iter()
            .find_map(|(surface, state)| match state.role {
                Role::Subsurface { proxy, .. } if proxy == id => Some(*surface),
                _ => None,
            })
            .ok_or_else(|| Error::protocol(format!("subsurface {id} has no wl_surface mapping")))
    }

    pub(super) fn create_new(&mut self, request: &DecodedRequest, index: usize) -> Result<u32> {
        let (id, interface) = request
            .args
            .get(index)
            .and_then(Value::new_id)
            .ok_or_else(|| bad(request, "missing new_id"))?;
        let interface = interface.ok_or_else(|| bad(request, "new_id has no static interface"))?;
        self.insert_record(
            id,
            ObjectRecord::new(interface, request.version.min(interface.version)),
        )?;
        Ok(id)
    }

    fn insert_record(&mut self, id: u32, record: ObjectRecord) -> Result<()> {
        if id == 0 || id >= 0xff00_0000 {
            return Err(Error::protocol(format!("invalid client object id {id}")));
        }
        if self.objects.contains_key(&id) {
            return Err(Error::protocol(format!("duplicate client object id {id}")));
        }
        self.objects.insert(id, record);
        Ok(())
    }

    pub(super) fn finish_destructor(
        &mut self,
        client: &mut Client,
        id: u32,
        kind: ObjectKind,
    ) -> Result<()> {
        match kind {
            ObjectKind::Surface => {
                self.surfaces.remove(&id);
            }
            ObjectKind::ShmPool => {
                if self.pools[&id].buffer_refs == 0 {
                    if let Some(pool) = self.pools.remove(&id) {
                        client.resources_mut().close_resource(pool.rid);
                    }
                } else {
                    self.pools.get_mut(&id).unwrap().destroyed = true;
                }
            }
            ObjectKind::Buffer => {
                self.retire_orphan_buffers(client);
            }
            ObjectKind::XdgSurface => {
                self.xdg_surfaces.remove(&id);
            }
            ObjectKind::XdgToplevel | ObjectKind::XdgPopup => {
                if let Some(window) = self.windows.remove(&id) {
                    if let Some(xdg) = self.xdg_surfaces.get_mut(&window.xdg_surface_id) {
                        xdg.window_id = None;
                    }
                    if let Some(surface) = self.surfaces.get_mut(&window.surface_id) {
                        surface.window_id = None;
                        surface.role = Role::None;
                        surface.committed.buffer = None;
                    }
                    if self.keyboard_focus == Some(id) {
                        self.keyboard_focus = None;
                    }
                    if self.pointer_focus.map(|focus| focus.0) == Some(id) {
                        self.pointer_focus = None;
                    }
                    self.events.push(CoreEvent::WindowClosed { id });
                }
            }
            ObjectKind::XdgPositioner => {
                self.positioners.remove(&id);
            }
            ObjectKind::Seat => {
                self.seats.remove(&id);
            }
            ObjectKind::Keyboard => {
                for seat in self.seats.values_mut() {
                    seat.keyboards.retain(|keyboard| *keyboard != id);
                }
            }
            ObjectKind::Pointer => {
                for seat in self.seats.values_mut() {
                    seat.pointers.retain(|pointer| *pointer != id);
                }
            }
            ObjectKind::Touch => {
                for seat in self.seats.values_mut() {
                    seat.touches.retain(|touch| *touch != id);
                }
            }
            ObjectKind::DataDevice | ObjectKind::DataSource | ObjectKind::DataOffer => {
                self.finish_clipboard_destructor(client, id, kind);
            }
            ObjectKind::TextInput => self.finish_text_input_destructor(id),
            _ => {}
        }
        self.objects.remove(&id);
        self.emit(
            &wl::WL_DISPLAY_INTERFACE,
            "delete_id",
            1,
            &[Argument::Uint(id)],
        );
        Ok(())
    }

    fn unref_buffer(&mut self, client: &mut Client, pool_id: u32) {
        let should_close = if let Some(pool) = self.pools.get_mut(&pool_id) {
            pool.buffer_refs = pool.buffer_refs.saturating_sub(1);
            pool.destroyed && pool.buffer_refs == 0
        } else {
            false
        };
        if should_close {
            if let Some(pool) = self.pools.remove(&pool_id) {
                client.resources_mut().close_resource(pool.rid);
            }
        }
    }
}

pub(super) fn bad(request: &DecodedRequest, reason: impl Into<String>) -> Error {
    Error::MalformedMessage {
        interface: request.interface.name,
        message: request.method.name,
        reason: reason.into(),
    }
}

pub(super) fn uint(request: &DecodedRequest, index: usize) -> Result<u32> {
    request
        .args
        .get(index)
        .and_then(Value::uint)
        .ok_or_else(|| bad(request, "expected uint argument"))
}

pub(super) fn int(request: &DecodedRequest, index: usize) -> Result<i32> {
    request
        .args
        .get(index)
        .and_then(Value::int)
        .or_else(|| {
            request
                .args
                .get(index)
                .and_then(Value::uint)
                .map(|v| v as i32)
        })
        .ok_or_else(|| bad(request, "expected int argument"))
}

pub(super) fn object_id(request: &DecodedRequest, index: usize) -> Result<u32> {
    optional_object_id(request, index)?.ok_or_else(|| bad(request, "unexpected null object"))
}

pub(super) fn optional_object_id(request: &DecodedRequest, index: usize) -> Result<Option<u32>> {
    request
        .args
        .get(index)
        .and_then(Value::object)
        .or_else(|| request.args.get(index).and_then(Value::uint).map(Some))
        .ok_or_else(|| bad(request, "expected object argument"))
}

pub(super) fn string(request: &DecodedRequest, index: usize) -> Result<&str> {
    request
        .args
        .get(index)
        .and_then(Value::string)
        .ok_or_else(|| bad(request, "expected string argument"))
}

pub(super) fn fd(request: &DecodedRequest, index: usize) -> Result<Rid> {
    request
        .args
        .get(index)
        .and_then(Value::fd)
        .ok_or_else(|| bad(request, "expected fd argument"))
}

pub(super) fn rect(request: &DecodedRequest, index: usize) -> Result<Rect> {
    Ok(Rect {
        x: int(request, index)?,
        y: int(request, index + 1)?,
        width: int(request, index + 2)?,
        height: int(request, index + 3)?,
    })
}

fn clip_rect(rect: Rect, width: i32, height: i32) -> Option<Rect> {
    let x1 = rect.x.clamp(0, width);
    let y1 = rect.y.clamp(0, height);
    let x2 = rect.x.saturating_add(rect.width).clamp(0, width);
    let y2 = rect.y.saturating_add(rect.height).clamp(0, height);
    (x2 > x1 && y2 > y1).then_some(Rect {
        x: x1,
        y: y1,
        width: x2 - x1,
        height: y2 - y1,
    })
}

fn push_damage(damage: &mut Vec<Rect>, rect: Rect) {
    if let Some(last) = damage.last_mut() {
        if last.x == rect.x && last.width == rect.width && last.y + last.height == rect.y {
            last.height += rect.height;
            return;
        }
    }
    damage.push(rect);
}

fn byte_range_damage(buffer: &Buffer, range: &std::ops::Range<usize>, damage: &mut Vec<Rect>) {
    let offset = buffer.offset as usize;
    let stride = buffer.stride as usize;
    let width_bytes = buffer.width as usize * 4;
    let Some(end_bounds) = offset.checked_add(stride.saturating_mul(buffer.height as usize)) else {
        return;
    };
    let start = range.start.max(offset).min(end_bounds);
    let end = range.end.max(offset).min(end_bounds);
    if start >= end {
        return;
    }
    let first_row = (start - offset) / stride;
    let last_row = (end - 1 - offset) / stride;
    for row in first_row..=last_row {
        let row_start = offset + row * stride;
        let row_end = row_start + width_bytes;
        let segment_start = start.max(row_start);
        let segment_end = end.min(row_end);
        if segment_start >= segment_end {
            continue;
        }
        let x1 = ((segment_start - row_start) / 4) as i32;
        let x2 = (segment_end - row_start).div_ceil(4) as i32;
        if let Some(rect) = clip_rect(
            Rect {
                x: x1,
                y: row as i32,
                width: x2 - x1,
                height: 1,
            },
            buffer.width,
            buffer.height,
        ) {
            push_damage(damage, rect);
        }
    }
}

#[cfg(test)]
mod tests;
