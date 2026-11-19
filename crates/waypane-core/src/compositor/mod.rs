use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

use crate::wayland_backend::protocol::Interface;
use waypipe_client::{Client, ProtocolMessage, Rid};

use crate::error::{Error, Result};
use crate::protocol::wayland as wl;
use crate::protocol::xdg;
use crate::protocol::{
    decode_request, event, event_opcode, event_with_fd, Argument, DecodedRequest, Value, GLOBALS,
};
mod clipboard;
mod dispatch;
mod scale;
mod text_input;
mod xdg_shell;
use crate::state::*;
use crate::types::*;
pub use scale::MAX_WAYLAND_SCALE;

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
    regions: BTreeMap<u32, Region>,
    pools: BTreeMap<u32, Pool>,
    buffers: BTreeMap<u32, Buffer>,
    xdg_surfaces: BTreeMap<u32, XdgSurface>,
    windows: BTreeMap<WindowId, Window>,
    positioners: BTreeMap<u32, Positioner>,
    popup_positioners: BTreeMap<WindowId, Positioner>,
    window_scales: BTreeMap<WindowId, i32>,
    seats: BTreeMap<u32, Seat>,
    pointers: BTreeMap<u32, PointerState>,
    outputs: Vec<u32>,
    data_devices: BTreeMap<u32, DataDevice>,
    data_sources: BTreeMap<u32, DataSource>,
    offers: BTreeMap<u32, SelectionState>,
    selection: Option<SelectionState>,
    clipboard_reads: BTreeMap<Rid, ClipboardRead>,
    subsurface_stacks: BTreeMap<u32, Vec<u32>>,
    published_z_orders: BTreeMap<WindowId, Vec<SurfaceId>>,
    subsurface_sync: BTreeMap<u32, SubsurfaceSync>,
    next_server_id: u32,
    outbound: Vec<ProtocolMessage>,
    events: Vec<CoreEvent>,
    pending_frames: BTreeMap<FrameId, PendingFrame>,
    next_frame_id: FrameId,
    serial: u32,
    keymap: Arc<[u8]>,
    output_size: Size,
    output_scale: i32,
    keyboard_focus: Option<WindowId>,
    pointer_focus: Option<(WindowId, SurfaceId, f64, f64)>,
    pointer_window: Option<(WindowId, f64, f64)>,
    active_pointer: Option<u32>,
    popup_grab: Vec<WindowId>,
    input_serials: VecDeque<u32>,
    pressed_keys: BTreeSet<u32>,
    touches: BTreeMap<i32, (SurfaceId, f64, f64)>,
    text_inputs: BTreeMap<u32, TextInput>,
    modifiers: Modifiers,
}

impl Compositor {
    pub fn new(keymap: Arc<[u8]>) -> Self {
        Self::with_output_scale(keymap, 1)
    }

    pub fn with_output_scale(keymap: Arc<[u8]>, scale: i32) -> Self {
        let mut objects = BTreeMap::new();
        objects.insert(1, ObjectRecord::new(&wl::WL_DISPLAY_INTERFACE, 1));
        Self {
            objects,
            surfaces: BTreeMap::new(),
            regions: BTreeMap::new(),
            pools: BTreeMap::new(),
            buffers: BTreeMap::new(),
            xdg_surfaces: BTreeMap::new(),
            windows: BTreeMap::new(),
            positioners: BTreeMap::new(),
            popup_positioners: BTreeMap::new(),
            window_scales: BTreeMap::new(),
            seats: BTreeMap::new(),
            pointers: BTreeMap::new(),
            outputs: Vec::new(),
            data_devices: BTreeMap::new(),
            data_sources: BTreeMap::new(),
            offers: BTreeMap::new(),
            selection: None,
            clipboard_reads: BTreeMap::new(),
            subsurface_stacks: BTreeMap::new(),
            published_z_orders: BTreeMap::new(),
            subsurface_sync: BTreeMap::new(),
            next_server_id: 0xff00_0000,
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
            output_scale: scale.clamp(1, MAX_WAYLAND_SCALE),
            keyboard_focus: None,
            pointer_focus: None,
            pointer_window: None,
            active_pointer: None,
            popup_grab: Vec::new(),
            input_serials: VecDeque::new(),
            pressed_keys: BTreeSet::new(),
            touches: BTreeMap::new(),
            text_inputs: BTreeMap::new(),
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

    pub fn complete_frame(&mut self, frame_id: FrameId, time: u32) -> Result<()> {
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
            self.retire_orphan_buffers();
        }
        self.retire_orphan_buffers();
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
        self.pressed_keys.clear();
        self.modifiers = Modifiers::default();
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
        self.sync_text_input_focus(old_surface, new_surface)?;
        self.keyboard_focus = window_id;
        self.sync_data_device_focus()?;
        Ok(())
    }

    pub fn pointer_focus(&mut self, window_id: Option<WindowId>, x: f64, y: f64) -> Result<()> {
        let target = window_id.and_then(|id| self.hit_surface(id, x, y));
        let pointers = self
            .seats
            .values()
            .flat_map(|seat| seat.pointers.iter().copied())
            .collect::<Vec<_>>();
        let target_window = target
            .and_then(|(surface, _, _)| {
                self.surfaces
                    .get(&surface)
                    .and_then(|surface| surface.window_id)
            })
            .or(window_id);
        let target_focus =
            target.map(|(surface, _, _)| (target_window.unwrap_or_default(), surface));
        for pointer in pointers {
            let previous = self.pointers.entry(pointer).or_default().focus;
            if previous == target_focus {
                continue;
            }
            if let Some((old_window, old_surface)) = previous {
                if self.objects.contains_key(&old_surface) {
                    let serial = self.next_serial();
                    self.emit(
                        &wl::WL_POINTER_INTERFACE,
                        "leave",
                        pointer,
                        &[Argument::Uint(serial), Argument::Object(old_surface)],
                    );
                }
                if Some(old_window) != window_id
                    && !self.popup_grab.contains(&old_window)
                    && self
                        .windows
                        .get(&old_window)
                        .is_some_and(|window| matches!(window.kind, WindowKind::Popup { .. }))
                {
                    self.emit(&xdg::XDG_POPUP_INTERFACE, "popup_done", old_window, &[]);
                }
            }
            if let Some((surface, local_x, local_y)) = target {
                let serial = self.next_serial();
                self.emit(
                    &wl::WL_POINTER_INTERFACE,
                    "enter",
                    pointer,
                    &[
                        Argument::Uint(serial),
                        Argument::Object(surface),
                        Argument::Fixed(local_x),
                        Argument::Fixed(local_y),
                    ],
                );
                self.emit(&wl::WL_POINTER_INTERFACE, "frame", pointer, &[]);
                if let Some(cursor) = self.pointers.get(&pointer).and_then(|state| state.cursor) {
                    self.set_surface_mapped(cursor.surface, false);
                }
                let state = self.pointers.entry(pointer).or_default();
                state.focus = Some((target_window.unwrap_or_default(), surface));
                state.enter_serial = Some(serial);
                state.cursor = None;
                self.active_pointer = Some(pointer);
            } else {
                if let Some(state) = self.pointers.get_mut(&pointer) {
                    state.focus = None;
                }
                if self.active_pointer == Some(pointer) {
                    self.active_pointer = None;
                }
            }
        }
        self.pointer_focus = target.map(|(surface, local_x, local_y)| {
            (target_window.unwrap_or_default(), surface, local_x, local_y)
        });
        self.pointer_window = target.map(|_| (window_id.unwrap_or_default(), x, y));
        if let Some((_surface, local_x, local_y)) = target {
            self.emit_pointer_motion(local_x, local_y, 0);
        }
        Ok(())
    }

    pub fn pointer_motion(&mut self, window_id: WindowId, x: f64, y: f64, time: u32) -> Result<()> {
        let Some((surface, local_x, local_y)) = self.hit_surface(window_id, x, y) else {
            return Ok(());
        };
        let focus_window = self
            .surfaces
            .get(&surface)
            .and_then(|surface| surface.window_id)
            .unwrap_or(window_id);
        if self.pointer_focus.map(|focus| (focus.0, focus.1)) != Some((focus_window, surface)) {
            return self.pointer_focus(Some(window_id), x, y);
        }
        self.pointer_focus = Some((focus_window, surface, local_x, local_y));
        self.pointer_window = Some((window_id, x, y));
        self.emit_pointer_motion(local_x, local_y, time);
        Ok(())
    }

    pub fn cursor_placement(&self) -> Option<(WindowId, f64, f64, Point)> {
        let pointer = self.active_pointer?;
        let cursor = self.pointers.get(&pointer)?.cursor?;
        let (window, x, y) = self.pointer_window?;
        self.pointer_focus
            .is_some()
            .then_some((window, x, y, cursor.hotspot))
    }

    fn emit_pointer_motion(&mut self, x: f64, y: f64, time: u32) {
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
    }

    fn hit_surface(&self, window_id: WindowId, x: f64, y: f64) -> Option<(SurfaceId, f64, f64)> {
        let window = self.windows.get(&window_id)?;
        if !matches!(window.kind, WindowKind::Popup { .. }) {
            if let Some((popup, local_x, local_y)) = self.hit_popup(window_id, x, y) {
                let surface = self.windows[&popup].surface_id;
                return self.hit_subsurface(surface, local_x, local_y).or_else(|| {
                    self.surface_accepts_input(surface, local_x, local_y)
                        .then_some((surface, local_x, local_y))
                });
            }
        }
        let surface = window.surface_id;
        self.hit_subsurface(surface, x, y).or_else(|| {
            self.surface_accepts_input(surface, x, y)
                .then_some((surface, x, y))
        })
    }

    fn hit_popup(&self, parent: WindowId, x: f64, y: f64) -> Option<(WindowId, f64, f64)> {
        for popup in self.windows.values().rev() {
            if !matches!(popup.kind, WindowKind::Popup { parent: popup_parent } if popup_parent == parent)
            {
                continue;
            }
            let rect = popup.geometry;
            let left = f64::from(rect.x);
            let top = f64::from(rect.y);
            if rect.width <= 0
                || rect.height <= 0
                || x < left
                || y < top
                || x >= left + f64::from(rect.width)
                || y >= top + f64::from(rect.height)
            {
                continue;
            }
            let local_x = x - left;
            let local_y = y - top;
            if let Some(hit) = self.hit_popup(popup.id, local_x, local_y) {
                return Some(hit);
            }
            if self.surface_accepts_input(popup.surface_id, local_x, local_y) {
                return Some((popup.id, local_x, local_y));
            }
        }
        None
    }

    fn hit_subsurface(&self, parent: SurfaceId, x: f64, y: f64) -> Option<(SurfaceId, f64, f64)> {
        let children = self.subsurface_stacks.get(&parent)?;
        for child in children.iter().rev() {
            let Some(surface) = self.surfaces.get(child) else {
                continue;
            };
            if surface.committed.buffer.is_none()
                || surface.logical_size.width == 0
                || surface.logical_size.height == 0
            {
                continue;
            }
            let left = f64::from(surface.position.x);
            let top = f64::from(surface.position.y);
            if x < left
                || y < top
                || x >= left + f64::from(surface.logical_size.width)
                || y >= top + f64::from(surface.logical_size.height)
            {
                continue;
            }
            let local_x = x - left;
            let local_y = y - top;
            return self.hit_subsurface(*child, local_x, local_y).or_else(|| {
                self.surface_accepts_input(*child, local_x, local_y)
                    .then_some((*child, local_x, local_y))
            });
        }
        None
    }

    fn surface_accepts_input(&self, surface: SurfaceId, x: f64, y: f64) -> bool {
        self.surfaces.get(&surface).is_some_and(|surface| {
            surface
                .committed
                .input_region
                .as_ref()
                .is_none_or(|region| region.contains(x, y))
        })
    }

    pub fn pointer_button(&mut self, button: u32, pressed: bool, time: u32) -> Result<()> {
        let Some((focus_window, _, _, _)) = self.pointer_focus else {
            return Ok(());
        };
        if pressed {
            self.dismiss_outside_popups(Some(focus_window));
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
            if pressed {
                self.record_input_serial(serial);
            }
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
        if pressed && key == 1 {
            self.dismiss_popup_grab();
        }
        if self.keyboard_focus.is_none() || key == 0 {
            return Ok(());
        }
        let changed = if pressed {
            self.pressed_keys.insert(key)
        } else {
            self.pressed_keys.remove(&key)
        };
        if !changed {
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
                    Argument::Uint(key),
                    Argument::Uint(u32::from(pressed)),
                ],
            );
            if pressed {
                self.record_input_serial(serial);
            }
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

    pub fn popup_grab_active(&self) -> bool {
        !self.popup_grab.is_empty()
    }

    fn record_input_serial(&mut self, serial: u32) {
        if self.input_serials.len() == 32 {
            self.input_serials.pop_front();
        }
        self.input_serials.push_back(serial);
    }

    pub fn touch_down(
        &mut self,
        window_id: WindowId,
        id: i32,
        x: f64,
        y: f64,
        time: u32,
    ) -> Result<()> {
        let Some((surface, local_x, local_y)) = self.hit_surface(window_id, x, y) else {
            return Ok(());
        };
        let focus_window = self
            .surfaces
            .get(&surface)
            .and_then(|surface| surface.window_id)
            .unwrap_or(window_id);
        self.dismiss_outside_popups(Some(focus_window));
        self.touches.insert(id, (surface, x - local_x, y - local_y));
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
                    Argument::Fixed(local_x),
                    Argument::Fixed(local_y),
                ],
            );
            self.emit(&wl::WL_TOUCH_INTERFACE, "frame", touch, &[]);
            self.record_input_serial(serial);
        }
        Ok(())
    }

    pub fn touch_motion(&mut self, window_id: WindowId, id: i32, x: f64, y: f64, time: u32) {
        let (surface, local_x, local_y) = self
            .touches
            .get(&id)
            .map(|(surface, offset_x, offset_y)| (*surface, x - offset_x, y - offset_y))
            .or_else(|| {
                self.windows
                    .get(&window_id)
                    .map(|window| (window.surface_id, x, y))
            })
            .unwrap_or((0, x, y));
        if surface == 0 {
            return;
        }
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
                    Argument::Fixed(local_x),
                    Argument::Fixed(local_y),
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
        self.touches.remove(&id);
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
