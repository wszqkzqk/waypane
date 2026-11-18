use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use wayland_backend::protocol::Interface;
use waypipe_client::{Client, ProtocolMessage, Rid};

use crate::error::{Error, Result};
use crate::protocol::wayland as wl;
use crate::protocol::xdg;
use crate::protocol::{
    decode_request, event, event_opcode, event_with_fd, Argument, DecodedRequest, Value, GLOBALS,
};
mod dispatch;
use crate::state::*;
use crate::types::*;

#[derive(Debug, Default, Clone, Copy)]
struct Modifiers {
    shift: bool,
    ctrl: bool,
    alt: bool,
    logo: bool,
}

impl Modifiers {
    fn masks(self) -> (u32, u32) {
        let mut depressed = 0;
        if self.shift {
            depressed |= 1;
        }
        if self.ctrl {
            depressed |= 4;
        }
        if self.alt {
            depressed |= 8;
        }
        if self.logo {
            depressed |= 64;
        }
        (depressed, 0)
    }

    fn update(&mut self, key: u32, pressed: bool) {
        let target = match key {
            42 | 54 => &mut self.shift,
            29 | 97 => &mut self.ctrl,
            56 | 100 => &mut self.alt,
            125 | 126 => &mut self.logo,
            _ => return,
        };
        *target = pressed;
    }
}

#[derive(Debug)]
struct PendingFrame {
    surface_id: u32,
    buffer_id: Option<u32>,
    callbacks: Vec<u32>,
}

pub struct Compositor {
    objects: BTreeMap<u32, ObjectRecord>,
    surfaces: BTreeMap<u32, Surface>,
    pools: BTreeMap<u32, Pool>,
    buffers: BTreeMap<u32, Buffer>,
    xdg_surfaces: BTreeMap<u32, XdgSurface>,
    windows: BTreeMap<WindowId, Window>,
    positioners: BTreeMap<u32, Positioner>,
    seats: BTreeMap<u32, Seat>,
    outputs: Vec<u32>,
    outbound: Vec<ProtocolMessage>,
    events: Vec<CoreEvent>,
    pending_frames: BTreeMap<FrameId, PendingFrame>,
    next_frame_id: FrameId,
    serial: u32,
    keymap: Arc<[u8]>,
    output_size: Size,
    output_scale: i32,
    keyboard_focus: Option<WindowId>,
    pointer_focus: Option<(WindowId, f64, f64)>,
    modifiers: Modifiers,
}

impl Compositor {
    pub fn new(keymap: Arc<[u8]>) -> Self {
        let mut objects = BTreeMap::new();
        objects.insert(1, ObjectRecord::new(&wl::WL_DISPLAY_INTERFACE, 1));
        Self {
            objects,
            surfaces: BTreeMap::new(),
            pools: BTreeMap::new(),
            buffers: BTreeMap::new(),
            xdg_surfaces: BTreeMap::new(),
            windows: BTreeMap::new(),
            positioners: BTreeMap::new(),
            seats: BTreeMap::new(),
            outputs: Vec::new(),
            outbound: Vec::new(),
            events: Vec::new(),
            pending_frames: BTreeMap::new(),
            next_frame_id: 1,
            serial: 1,
            keymap,
            output_size: Size {
                width: 1920,
                height: 1080,
            },
            output_scale: 1,
            keyboard_focus: None,
            pointer_focus: None,
            modifiers: Modifiers::default(),
        }
    }

    pub fn process(
        &mut self,
        client: &mut Client,
        messages: &[waypipe_client::WaylandMessage],
        rids: &mut VecDeque<Rid>,
    ) -> Result<()> {
        for message in messages {
            let object_id = message.object_id();
            let record = self
                .objects
                .get(&object_id)
                .cloned()
                .ok_or(Error::UnknownObject {
                    object_id,
                    interface: "request",
                })?;
            let request = decode_request(message, record.interface, record.version, rids)?;
            let kind = record.kind;
            self.dispatch(client, &request, kind)?;
            if request.method.is_destructor && self.objects.contains_key(&object_id) {
                self.finish_destructor(client, object_id, kind)?;
            }
        }
        Ok(())
    }

    pub fn take_outgoing(&mut self) -> Vec<ProtocolMessage> {
        std::mem::take(&mut self.outbound)
    }

    pub fn take_events(&mut self) -> Vec<CoreEvent> {
        std::mem::take(&mut self.events)
    }

    pub fn window_info(&self, id: WindowId) -> Option<WindowInfo> {
        self.windows.get(&id).map(|window| WindowInfo {
            id: window.id,
            kind: window.kind,
            title: window.title.clone(),
            app_id: window.app_id.clone(),
            geometry: window.geometry,
        })
    }

    pub fn complete_frame(
        &mut self,
        client: &mut Client,
        frame_id: FrameId,
        time: u32,
    ) -> Result<()> {
        let frame = self
            .pending_frames
            .remove(&frame_id)
            .ok_or_else(|| Error::protocol(format!("unknown frame id {frame_id}")))?;
        if let Some(buffer_id) = frame.buffer_id {
            if self.objects.contains_key(&buffer_id) {
                self.emit(&wl::WL_BUFFER_INTERFACE, "release", buffer_id, &[]);
            }
        }
        for callback in frame.callbacks {
            if self.objects.remove(&callback).is_some() {
                self.emit_time(&wl::WL_CALLBACK_INTERFACE, "done", callback, time);
                self.emit(
                    &wl::WL_DISPLAY_INTERFACE,
                    "delete_id",
                    1,
                    &[Argument::Uint(callback)],
                );
            }
        }
        if !self.surfaces.contains_key(&frame.surface_id) {
            self.retire_orphan_buffers(client);
        }
        self.retire_orphan_buffers(client);
        Ok(())
    }

    pub fn configure_window(
        &mut self,
        id: WindowId,
        size: Option<Size>,
        mut states: Vec<u32>,
    ) -> Result<()> {
        if !states.contains(&4) {
            let activated = self.keyboard_focus == Some(id);
            if activated {
                states.push(4);
            }
        }
        let serial = self.next_serial();
        let xdg_surface_id = {
            let xdg_surface = self
                .xdg_surfaces
                .values_mut()
                .find(|surface| surface.window_id == Some(id))
                .ok_or_else(|| Error::protocol(format!("unknown window {id}")))?;
            if !matches!(
                self.windows.get(&id).map(|w| w.kind),
                Some(WindowKind::Toplevel)
            ) {
                return Err(Error::protocol(
                    "popup windows cannot receive toplevel configure",
                ));
            }
            xdg_surface.configure_serial = Some(serial);
            self.windows[&id].xdg_surface_id
        };
        self.send_configure(id, xdg_surface_id, size, &states, serial);
        self.events
            .push(CoreEvent::WindowConfigured { id, size, states });
        Ok(())
    }

    pub fn close_window(&mut self, id: WindowId) -> Result<()> {
        match self.windows.get(&id).map(|window| window.kind) {
            Some(WindowKind::Toplevel) => {
                self.emit(&xdg::XDG_TOPLEVEL_INTERFACE, "close", id, &[]);
                Ok(())
            }
            Some(WindowKind::Popup { .. }) => {
                self.emit(&xdg::XDG_POPUP_INTERFACE, "popup_done", id, &[]);
                Ok(())
            }
            _ => Err(Error::protocol(format!("cannot close window {id}"))),
        }
    }

    pub fn keyboard_focus(&mut self, window_id: Option<WindowId>) -> Result<()> {
        if self.keyboard_focus == window_id {
            return Ok(());
        }
        let old_surface = self
            .keyboard_focus
            .and_then(|id| self.windows.get(&id))
            .map(|window| window.surface_id);
        let new_surface = window_id
            .and_then(|id| self.windows.get(&id))
            .map(|window| window.surface_id);
        let keyboards = self
            .seats
            .values()
            .flat_map(|seat| seat.keyboards.iter().copied())
            .collect::<Vec<_>>();
        for keyboard in keyboards {
            if let Some(surface) = old_surface {
                let serial = self.next_serial();
                self.emit(
                    &wl::WL_KEYBOARD_INTERFACE,
                    "leave",
                    keyboard,
                    &[Argument::Uint(serial), Argument::Object(surface)],
                );
            }
            if let Some(surface) = new_surface {
                let serial = self.next_serial();
                self.emit(
                    &wl::WL_KEYBOARD_INTERFACE,
                    "enter",
                    keyboard,
                    &[
                        Argument::Uint(serial),
                        Argument::Object(surface),
                        Argument::Array(&[]),
                    ],
                );
                self.emit_modifiers(keyboard);
            }
        }
        if let (Some(old), Some(new)) = (self.keyboard_focus, window_id) {
            if old != new {
                let size = self.windows.get(&new).and_then(|window| {
                    (window.geometry.width > 0 && window.geometry.height > 0).then_some(Size {
                        width: window.geometry.width as u32,
                        height: window.geometry.height as u32,
                    })
                });
                if self.windows.contains_key(&new) {
                    let _ = self.configure_window(new, size, vec![4]);
                }
            }
        }
        self.keyboard_focus = window_id;
        Ok(())
    }

    pub fn pointer_focus(&mut self, window_id: Option<WindowId>, x: f64, y: f64) -> Result<()> {
        let surface = window_id
            .and_then(|id| self.windows.get(&id))
            .map(|window| window.surface_id);
        let previous = self.pointer_focus.map(|focus| focus.0);
        let pointers = self
            .seats
            .values()
            .flat_map(|seat| seat.pointers.iter().copied())
            .collect::<Vec<_>>();
        for pointer in pointers {
            if let Some(previous) = previous {
                if Some(previous) != window_id {
                    if let Some(old_surface) = self.windows.get(&previous).map(|w| w.surface_id) {
                        let serial = self.next_serial();
                        self.emit(
                            &wl::WL_POINTER_INTERFACE,
                            "leave",
                            pointer,
                            &[Argument::Uint(serial), Argument::Object(old_surface)],
                        );
                        if self
                            .windows
                            .get(&previous)
                            .map(|w| w.kind)
                            .is_some_and(|kind| matches!(kind, WindowKind::Popup { .. }))
                        {
                            self.emit(&xdg::XDG_POPUP_INTERFACE, "popup_done", previous, &[]);
                        }
                    }
                }
            }
            if previous != window_id {
                if let Some(surface) = surface {
                    let serial = self.next_serial();
                    self.emit(
                        &wl::WL_POINTER_INTERFACE,
                        "enter",
                        pointer,
                        &[
                            Argument::Uint(serial),
                            Argument::Object(surface),
                            Argument::Fixed(x),
                            Argument::Fixed(y),
                        ],
                    );
                    self.emit(&wl::WL_POINTER_INTERFACE, "frame", pointer, &[]);
                }
            }
        }
        self.pointer_focus = window_id.map(|id| (id, x, y));
        if let Some(window_id) = window_id {
            self.pointer_motion(window_id, x, y, 0)?;
        }
        Ok(())
    }

    pub fn pointer_motion(&mut self, window_id: WindowId, x: f64, y: f64, time: u32) -> Result<()> {
        if self.pointer_focus.map(|focus| focus.0) != Some(window_id) {
            return self.pointer_focus(Some(window_id), x, y);
        }
        self.pointer_focus = Some((window_id, x, y));
        let pointers = self
            .seats
            .values()
            .flat_map(|seat| seat.pointers.iter().copied())
            .collect::<Vec<_>>();
        for pointer in pointers {
            self.emit(
                &wl::WL_POINTER_INTERFACE,
                "motion",
                pointer,
                &[Argument::Uint(time), Argument::Fixed(x), Argument::Fixed(y)],
            );
            self.emit(&wl::WL_POINTER_INTERFACE, "frame", pointer, &[]);
        }
        Ok(())
    }

    pub fn pointer_button(&mut self, button: u32, pressed: bool, time: u32) -> Result<()> {
        if self.pointer_focus.is_none() {
            return Ok(());
        }
        let pointers = self
            .seats
            .values()
            .flat_map(|seat| seat.pointers.iter().copied())
            .collect::<Vec<_>>();
        for pointer in pointers {
            let serial = self.next_serial();
            self.emit(
                &wl::WL_POINTER_INTERFACE,
                "button",
                pointer,
                &[
                    Argument::Uint(serial),
                    Argument::Uint(time),
                    Argument::Uint(button),
                    Argument::Uint(u32::from(pressed)),
                ],
            );
            self.emit(&wl::WL_POINTER_INTERFACE, "frame", pointer, &[]);
        }
        Ok(())
    }

    pub fn pointer_axis(
        &mut self,
        vertical: bool,
        value: f64,
        discrete120: Option<i32>,
        time: u32,
    ) -> Result<()> {
        let axis = u32::from(!vertical);
        let pointers = self
            .seats
            .values()
            .flat_map(|seat| seat.pointers.iter().copied())
            .collect::<Vec<_>>();
        for pointer in pointers {
            if self
                .objects
                .get(&pointer)
                .is_some_and(|object| object.version >= 5)
            {
                self.emit(
                    &wl::WL_POINTER_INTERFACE,
                    "axis_source",
                    pointer,
                    &[Argument::Uint(0)],
                );
            }
            self.emit(
                &wl::WL_POINTER_INTERFACE,
                "axis",
                pointer,
                &[
                    Argument::Uint(time),
                    Argument::Uint(axis),
                    Argument::Fixed(value),
                ],
            );
            if let Some(value120) = discrete120 {
                if self
                    .objects
                    .get(&pointer)
                    .is_some_and(|object| object.version >= 8)
                {
                    self.emit(
                        &wl::WL_POINTER_INTERFACE,
                        "axis_value120",
                        pointer,
                        &[Argument::Uint(axis), Argument::Int(value120)],
                    );
                } else if self
                    .objects
                    .get(&pointer)
                    .is_some_and(|object| object.version >= 5)
                {
                    self.emit(
                        &wl::WL_POINTER_INTERFACE,
                        "axis_discrete",
                        pointer,
                        &[Argument::Uint(axis), Argument::Int(value120 / 120)],
                    );
                }
            }
            if self
                .objects
                .get(&pointer)
                .is_some_and(|object| object.version >= 5)
            {
                self.emit(&wl::WL_POINTER_INTERFACE, "frame", pointer, &[]);
            }
        }
        Ok(())
    }

    pub fn key(&mut self, key: u32, pressed: bool, time: u32) -> Result<()> {
        if self.keyboard_focus.is_none() || key < 8 {
            return Ok(());
        }
        let keyboards = self
            .seats
            .values()
            .flat_map(|seat| seat.keyboards.iter().copied())
            .collect::<Vec<_>>();
        for keyboard in keyboards {
            let serial = self.next_serial();
            self.emit(
                &wl::WL_KEYBOARD_INTERFACE,
                "key",
                keyboard,
                &[
                    Argument::Uint(serial),
                    Argument::Uint(time),
                    Argument::Uint(key - 8),
                    Argument::Uint(u32::from(pressed)),
                ],
            );
            let changed = self.modifiers;
            self.modifiers.update(key, pressed);
            if changed.shift != self.modifiers.shift
                || changed.ctrl != self.modifiers.ctrl
                || changed.alt != self.modifiers.alt
                || changed.logo != self.modifiers.logo
            {
                self.emit_modifiers(keyboard);
            }
        }
        Ok(())
    }

    pub fn touch_down(
        &mut self,
        window_id: WindowId,
        id: i32,
        x: f64,
        y: f64,
        time: u32,
    ) -> Result<()> {
        let Some(surface) = self.windows.get(&window_id).map(|window| window.surface_id) else {
            return Ok(());
        };
        let touches = self
            .seats
            .values()
            .flat_map(|seat| seat.touches.iter().copied())
            .collect::<Vec<_>>();
        for touch in touches {
            let serial = self.next_serial();
            self.emit(
                &wl::WL_TOUCH_INTERFACE,
                "down",
                touch,
                &[
                    Argument::Uint(serial),
                    Argument::Uint(time),
                    Argument::Object(surface),
                    Argument::Int(id),
                    Argument::Fixed(x),
                    Argument::Fixed(y),
                ],
            );
            self.emit(&wl::WL_TOUCH_INTERFACE, "frame", touch, &[]);
        }
        Ok(())
    }

    pub fn touch_motion(&mut self, id: i32, x: f64, y: f64, time: u32) {
        let touches = self
            .seats
            .values()
            .flat_map(|seat| seat.touches.iter().copied())
            .collect::<Vec<_>>();
        for touch in touches {
            self.emit(
                &wl::WL_TOUCH_INTERFACE,
                "motion",
                touch,
                &[
                    Argument::Uint(time),
                    Argument::Int(id),
                    Argument::Fixed(x),
                    Argument::Fixed(y),
                ],
            );
            self.emit(&wl::WL_TOUCH_INTERFACE, "frame", touch, &[]);
        }
    }

    pub fn touch_cancel(&mut self) {
        let touches = self
            .seats
            .values()
            .flat_map(|seat| seat.touches.iter().copied())
            .collect::<Vec<_>>();
        for touch in touches {
            self.emit(&wl::WL_TOUCH_INTERFACE, "cancel", touch, &[]);
            self.emit(&wl::WL_TOUCH_INTERFACE, "frame", touch, &[]);
        }
    }

    pub fn touch_up(&mut self, id: i32, time: u32) {
        let touches = self
            .seats
            .values()
            .flat_map(|seat| seat.touches.iter().copied())
            .collect::<Vec<_>>();
        for touch in touches {
            let serial = self.next_serial();
            self.emit(
                &wl::WL_TOUCH_INTERFACE,
                "up",
                touch,
                &[
                    Argument::Uint(serial),
                    Argument::Uint(time),
                    Argument::Int(id),
                ],
            );
            self.emit(&wl::WL_TOUCH_INTERFACE, "frame", touch, &[]);
        }
    }
}
