use super::dispatch::{bad, int, object_id, optional_object_id, rect, uint};
use super::*;

pub(super) const SLIDE_X: u32 = 1;
pub(super) const SLIDE_Y: u32 = 2;
pub(super) const FLIP_X: u32 = 4;
pub(super) const FLIP_Y: u32 = 8;
pub(super) const RESIZE_X: u32 = 16;
pub(super) const RESIZE_Y: u32 = 32;
const CONSTRAINT_MASK: u32 = SLIDE_X | SLIDE_Y | FLIP_X | FLIP_Y | RESIZE_X | RESIZE_Y;
const MAX_DIRECTION: u32 = 8;
const MAX_POPUP_DEPTH: usize = 64;

#[derive(Clone, Copy)]
struct Bounds {
    width: i64,
    height: i64,
}

impl Compositor {
    pub(super) fn update_positioner(&mut self, request: &DecodedRequest) -> Result<()> {
        let mut positioner = *self
            .positioners
            .get(&request.object_id)
            .ok_or_else(|| bad(request, "unknown xdg_positioner"))?;
        match request.method.name {
            "set_size" => {
                let width = int(request, 0)?;
                let height = int(request, 1)?;
                if width <= 0 || height <= 0 {
                    return Err(bad(request, "positioner size must be positive"));
                }
                positioner.size = Some((width, height));
            }
            "set_anchor_rect" => {
                let anchor_rect = rect(request, 0)?;
                if anchor_rect.width < 0 || anchor_rect.height < 0 {
                    return Err(bad(request, "anchor rectangle size cannot be negative"));
                }
                positioner.anchor_rect = Some(anchor_rect);
            }
            "set_anchor" => {
                positioner.anchor = direction(request, 0)?;
            }
            "set_gravity" => {
                positioner.gravity = direction(request, 0)?;
            }
            "set_constraint_adjustment" => {
                let adjustment = uint(request, 0)?;
                if adjustment & !CONSTRAINT_MASK != 0 {
                    return Err(bad(request, "unknown constraint adjustment bits"));
                }
                positioner.constraint_adjustment = adjustment;
            }
            "set_offset" => {
                positioner.offset = Point {
                    x: int(request, 0)?,
                    y: int(request, 1)?,
                };
            }
            "set_reactive" => positioner.reactive = true,
            "set_parent_size" => {
                let width = int(request, 0)?;
                let height = int(request, 1)?;
                if width <= 0 || height <= 0 {
                    return Err(bad(request, "positioner parent size must be positive"));
                }
                positioner.parent_size = Some((width, height));
            }
            "set_parent_configure" => positioner.parent_serial = Some(uint(request, 0)?),
            other => {
                return Err(bad(
                    request,
                    format!("unsupported xdg_positioner request {other}"),
                ))
            }
        }
        self.positioners.insert(request.object_id, positioner);
        Ok(())
    }

    pub(super) fn create_popup(&mut self, request: &DecodedRequest) -> Result<()> {
        let parent_xdg = optional_object_id(request, 1)?
            .ok_or_else(|| bad(request, "xdg_popup requires a supported parent surface"))?;
        if self
            .xdg_surfaces
            .get(&request.object_id)
            .is_none_or(|surface| surface.window_id.is_some())
        {
            return Err(bad(request, "xdg_surface is unknown or already has a role"));
        }
        let parent = self
            .xdg_surfaces
            .get(&parent_xdg)
            .and_then(|surface| surface.window_id)
            .ok_or_else(|| bad(request, "popup parent has no role"))?;
        let positioner = self.positioner(object_id(request, 2)?, request)?;
        let geometry = self
            .popup_geometry(parent, positioner, None)
            .map_err(|error| bad(request, error))?;
        let id = self.create_new(request, 0)?;
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
        self.popup_positioners.insert(id, positioner);
        self.emit_window_preferred_scale(id);
        self.configure_popup(id, request.object_id, geometry, None);
        let info = self.window_info(id).unwrap();
        self.events.push(CoreEvent::WindowCreated(info));
        Ok(())
    }

    pub(super) fn reposition_popup(&mut self, request: &DecodedRequest) -> Result<()> {
        let Some(Window {
            xdg_surface_id,
            kind: WindowKind::Popup { parent },
            ..
        }) = self.windows.get(&request.object_id)
        else {
            return Err(bad(request, "unknown xdg_popup"));
        };
        let (xdg_surface_id, parent) = (*xdg_surface_id, *parent);
        let positioner = self.positioner(object_id(request, 0)?, request)?;
        let geometry = self
            .popup_geometry(parent, positioner, None)
            .map_err(|error| bad(request, error))?;
        let token = (request.method.name == "reposition")
            .then(|| uint(request, 1))
            .transpose()?;
        self.popup_positioners.insert(request.object_id, positioner);
        self.configure_popup(request.object_id, xdg_surface_id, geometry, token);
        Ok(())
    }

    pub(super) fn grab_popup(&mut self, request: &DecodedRequest) -> Result<()> {
        let seat = object_id(request, 0)?;
        if !self.seats.contains_key(&seat) {
            return Err(bad(request, "unknown wl_seat for popup grab"));
        }
        let serial = uint(request, 1)?;
        if !self.input_serials.contains(&serial) {
            return Err(bad(
                request,
                "xdg_popup.grab requires a serial from a recent button, key, or touch press",
            ));
        }
        let Some(Window {
            kind: WindowKind::Popup { parent },
            ..
        }) = self.windows.get(&request.object_id)
        else {
            return Err(bad(request, "unknown xdg_popup"));
        };
        let parent = *parent;
        if self.popup_grab.last().is_some_and(|top| *top != parent) {
            self.dismiss_popup_grab();
        }
        if self.popup_grab.last() != Some(&request.object_id) {
            self.popup_grab.push(request.object_id);
        }
        self.keyboard_focus(Some(request.object_id))
    }

    pub(super) fn dismiss_outside_popups(&mut self, window: Option<WindowId>) {
        let inside = self.popup_grab.last().is_some_and(|top| {
            window.is_some_and(|window| self.popup_descendant_or_self(window, *top))
        });
        if !inside {
            self.dismiss_popup_grab();
        }
    }

    pub(super) fn dismiss_popup_grab(&mut self) {
        for popup in std::mem::take(&mut self.popup_grab).into_iter().rev() {
            if self.windows.contains_key(&popup) {
                self.emit(&xdg::XDG_POPUP_INTERFACE, "popup_done", popup, &[]);
            }
        }
    }

    pub(super) fn ack_xdg_configure(&mut self, request: &DecodedRequest) -> Result<()> {
        let serial = uint(request, 0)?;
        let (window_id, geometry) = {
            let xdg = self
                .xdg_surfaces
                .get_mut(&request.object_id)
                .ok_or_else(|| bad(request, "unknown xdg_surface"))?;
            let Some(index) = xdg
                .pending_configures
                .iter()
                .position(|pending| pending.serial == serial)
            else {
                return Err(bad(
                    request,
                    "ack_configure references a serial that was never sent or was already consumed",
                ));
            };
            let geometry = xdg.pending_configures[index].geometry;
            xdg.pending_configures.drain(..=index);
            xdg.initial_ack_seen = true;
            (xdg.window_id, geometry)
        };
        if let (Some(window_id), Some(geometry)) = (window_id, geometry) {
            let window = self
                .windows
                .get_mut(&window_id)
                .ok_or_else(|| Error::protocol(format!("unknown window {window_id}")))?;
            window.geometry = geometry;
            self.events.push(CoreEvent::WindowMetadata {
                id: window_id,
                title: window.title.clone(),
                app_id: window.app_id.clone(),
                geometry,
            });
        }
        Ok(())
    }

    pub(super) fn configure_size_limits(&mut self, request: &DecodedRequest) -> Result<()> {
        let width = int(request, 0)?;
        let height = int(request, 1)?;
        if width < 0 || height < 0 {
            return Err(bad(request, "window size limits cannot be negative"));
        }
        let maximum = request.method.name == "set_max_size";
        if maximum && (width == 0) != (height == 0) {
            return Err(bad(
                request,
                "maximum width and height must both be zero or positive",
            ));
        }
        let reconfigure = {
            let xdg = self
                .xdg_surfaces
                .values_mut()
                .find(|surface| surface.window_id == Some(request.object_id))
                .ok_or_else(|| bad(request, "unknown xdg_toplevel"))?;
            let width = width as u32;
            let height = height as u32;
            if maximum {
                if width != 0 && (width < xdg.min_size.width || height < xdg.min_size.height) {
                    return Err(bad(request, "maximum size is smaller than minimum size"));
                }
                xdg.max_size = Size { width, height };
            } else {
                if xdg.max_size.width != 0
                    && (width > xdg.max_size.width || height > xdg.max_size.height)
                {
                    return Err(bad(request, "minimum size exceeds maximum size"));
                }
                xdg.min_size = Size { width, height };
            }
            xdg.configured_size
                .map(|size| (size, xdg.configured_states.clone()))
        };
        if let Some((size, states)) = reconfigure {
            self.configure_window(request.object_id, Some(size), states)?;
        }
        Ok(())
    }

    pub fn configure_window(
        &mut self,
        id: WindowId,
        size: Option<Size>,
        mut states: Vec<u32>,
    ) -> Result<()> {
        match self.windows.get(&id).map(|window| window.kind) {
            Some(WindowKind::Toplevel) => {}
            // Popup size is client-driven and already configured; host native resizes of a
            // popup carry no xdg_toplevel.configure, rather than being a protocol error.
            Some(WindowKind::Popup { .. }) => return Ok(()),
            _ => return Err(Error::protocol(format!("unknown toplevel {id}"))),
        }
        if !states.contains(&4) && self.keyboard_focus == Some(id) {
            states.push(4);
        }
        let serial = self.next_serial();
        let (xdg_surface_id, size) = {
            let xdg = self
                .xdg_surfaces
                .values_mut()
                .find(|surface| surface.window_id == Some(id))
                .ok_or_else(|| Error::protocol(format!("unknown xdg_surface for window {id}")))?;
            let size = size.map(|size| constrain_size(size, xdg.min_size, xdg.max_size));
            xdg.record_configure(serial, None);
            xdg.configured_size = size;
            xdg.configured_states = states.clone();
            (self.windows[&id].xdg_surface_id, size)
        };
        self.send_configure(id, xdg_surface_id, size, &states, serial);
        self.events
            .push(CoreEvent::WindowConfigured { id, size, states });
        self.reposition_reactive_popups(id, size, serial);
        Ok(())
    }

    fn positioner(&self, id: u32, request: &DecodedRequest) -> Result<Positioner> {
        self.positioners
            .get(&id)
            .copied()
            .ok_or_else(|| bad(request, "unknown xdg_positioner"))
    }

    fn configure_popup(
        &mut self,
        popup: WindowId,
        xdg_surface: u32,
        geometry: Rect,
        token: Option<u32>,
    ) {
        let serial = self.next_serial();
        if let Some(xdg) = self.xdg_surfaces.get_mut(&xdg_surface) {
            xdg.record_configure(serial, Some(geometry));
        }
        if let Some(token) = token {
            self.emit(
                &xdg::XDG_POPUP_INTERFACE,
                "repositioned",
                popup,
                &[Argument::Uint(token)],
            );
        }
        self.emit(
            &xdg::XDG_POPUP_INTERFACE,
            "configure",
            popup,
            &[
                Argument::Int(geometry.x),
                Argument::Int(geometry.y),
                Argument::Int(geometry.width),
                Argument::Int(geometry.height),
            ],
        );
        self.emit(
            &xdg::XDG_SURFACE_INTERFACE,
            "configure",
            xdg_surface,
            &[Argument::Uint(serial)],
        );
    }

    fn reposition_reactive_popups(
        &mut self,
        parent: WindowId,
        parent_size: Option<Size>,
        serial: u32,
    ) {
        let popups = self
            .popup_positioners
            .iter()
            .filter(|(id, positioner)| {
                positioner.reactive
                    && positioner
                        .parent_serial
                        .is_none_or(|expected| expected == serial)
                    && matches!(
                        self.windows.get(id).map(|window| window.kind),
                        Some(WindowKind::Popup { parent: popup_parent }) if popup_parent == parent
                    )
            })
            .map(|(id, positioner)| (*id, *positioner))
            .collect::<Vec<_>>();
        for (popup, positioner) in popups {
            if let Ok(geometry) = self.popup_geometry(parent, positioner, parent_size) {
                let xdg_surface = self.windows[&popup].xdg_surface_id;
                self.configure_popup(popup, xdg_surface, geometry, None);
            }
        }
    }

    fn popup_geometry(
        &self,
        parent: WindowId,
        positioner: Positioner,
        parent_size: Option<Size>,
    ) -> std::result::Result<Rect, String> {
        let (width, height) = positioner
            .size
            .filter(|(width, height)| *width > 0 && *height > 0)
            .ok_or_else(|| "positioner requires a positive size".to_string())?;
        let anchor_rect = positioner
            .anchor_rect
            .filter(|rect| rect.width > 0 && rect.height > 0)
            .ok_or_else(|| "positioner requires a non-empty anchor rectangle".to_string())?;
        if positioner.anchor > MAX_DIRECTION || positioner.gravity > MAX_DIRECTION {
            return Err("positioner anchor or gravity is invalid".to_string());
        }
        let bounds = self.parent_bounds(parent, positioner, parent_size)?;
        let right = i64::from(anchor_rect.x) + i64::from(anchor_rect.width);
        let bottom = i64::from(anchor_rect.y) + i64::from(anchor_rect.height);
        if anchor_rect.x < 0 || anchor_rect.y < 0 || right > bounds.width || bottom > bounds.height
        {
            return Err("anchor rectangle extends outside the parent geometry".to_string());
        }

        let original = unadjusted_geometry(positioner, anchor_rect, width, height)?;
        let mut geometry = original;
        if positioner.constraint_adjustment & FLIP_X != 0 && constrained_x(original, bounds) {
            let flipped = unadjusted_geometry(
                Positioner {
                    anchor: flip_x(positioner.anchor),
                    gravity: flip_x(positioner.gravity),
                    ..positioner
                },
                anchor_rect,
                width,
                height,
            )?;
            if !constrained_x(flipped, bounds) {
                geometry.x = flipped.x;
                geometry.width = flipped.width;
            }
        }
        if positioner.constraint_adjustment & FLIP_Y != 0 && constrained_y(original, bounds) {
            let flipped = unadjusted_geometry(
                Positioner {
                    anchor: flip_y(positioner.anchor),
                    gravity: flip_y(positioner.gravity),
                    ..positioner
                },
                anchor_rect,
                width,
                height,
            )?;
            if !constrained_y(flipped, bounds) {
                geometry.y = flipped.y;
                geometry.height = flipped.height;
            }
        }
        if positioner.constraint_adjustment & SLIDE_X != 0 && constrained_x(geometry, bounds) {
            geometry.x = i64::from(geometry.x)
                .clamp(0, (bounds.width - i64::from(geometry.width)).max(0))
                as i32;
        }
        if positioner.constraint_adjustment & SLIDE_Y != 0 && constrained_y(geometry, bounds) {
            geometry.y = i64::from(geometry.y)
                .clamp(0, (bounds.height - i64::from(geometry.height)).max(0))
                as i32;
        }
        if positioner.constraint_adjustment & RESIZE_X != 0 {
            resize_x(&mut geometry, bounds);
        }
        if positioner.constraint_adjustment & RESIZE_Y != 0 {
            resize_y(&mut geometry, bounds);
        }

        let right = i64::from(geometry.x) + i64::from(geometry.width);
        let bottom = i64::from(geometry.y) + i64::from(geometry.height);
        if right < 0
            || bottom < 0
            || i64::from(geometry.x) > bounds.width
            || i64::from(geometry.y) > bounds.height
        {
            return Err("popup does not intersect or adjoin its parent surface".to_string());
        }
        Ok(geometry)
    }

    fn parent_bounds(
        &self,
        parent: WindowId,
        positioner: Positioner,
        configured_size: Option<Size>,
    ) -> std::result::Result<Bounds, String> {
        if let Some((width, height)) = positioner.parent_size {
            if width <= 0 || height <= 0 {
                return Err("positioner parent size must be positive".to_string());
            }
            return Ok(Bounds {
                width: i64::from(width),
                height: i64::from(height),
            });
        }
        if let Some(size) = configured_size.filter(|size| size.width > 0 && size.height > 0) {
            return Ok(Bounds {
                width: i64::from(size.width),
                height: i64::from(size.height),
            });
        }
        let window = self
            .windows
            .get(&parent)
            .ok_or_else(|| "unknown popup parent".to_string())?;
        if window.geometry.width > 0 && window.geometry.height > 0 {
            return Ok(Bounds {
                width: i64::from(window.geometry.width),
                height: i64::from(window.geometry.height),
            });
        }
        let size = self
            .surfaces
            .get(&window.surface_id)
            .map(|surface| surface.logical_size)
            .filter(|size| size.width > 0 && size.height > 0)
            .ok_or_else(|| "popup parent has no usable geometry".to_string())?;
        Ok(Bounds {
            width: i64::from(size.width),
            height: i64::from(size.height),
        })
    }

    pub(super) fn popup_descendant_or_self(&self, window: WindowId, ancestor: WindowId) -> bool {
        let mut current = Some(window);
        for _ in 0..MAX_POPUP_DEPTH {
            match current {
                Some(id) if id == ancestor => return true,
                Some(id) => {
                    current = match self.windows.get(&id).map(|window| window.kind) {
                        Some(WindowKind::Popup { parent }) => Some(parent),
                        _ => None,
                    };
                }
                None => return false,
            }
        }
        false
    }
}

fn direction(request: &DecodedRequest, index: usize) -> Result<u32> {
    let value = uint(request, index)?;
    if value > MAX_DIRECTION {
        return Err(bad(request, "invalid positioner anchor or gravity"));
    }
    Ok(value)
}

fn anchor_point(anchor: u32, rect: Rect) -> (i64, i64) {
    let horizontal = match anchor {
        3 | 5 | 6 => 0,
        4 | 7 | 8 => 2,
        _ => 1,
    };
    let vertical = match anchor {
        1 | 5 | 7 => 0,
        2 | 6 | 8 => 2,
        _ => 1,
    };
    let x = 2 * i64::from(rect.x) + i64::from(horizontal * rect.width);
    let y = 2 * i64::from(rect.y) + i64::from(vertical * rect.height);
    (x / 2, y / 2)
}

fn unadjusted_geometry(
    positioner: Positioner,
    anchor_rect: Rect,
    width: i32,
    height: i32,
) -> std::result::Result<Rect, String> {
    let (anchor_x, anchor_y) = anchor_point(positioner.anchor, anchor_rect);
    let x = anchor_x + i64::from(positioner.offset.x);
    let y = anchor_y + i64::from(positioner.offset.y);
    let (x, y) = match positioner.gravity {
        1 => (x - i64::from(width) / 2, y - i64::from(height)),
        2 => (x - i64::from(width) / 2, y),
        3 => (x - i64::from(width), y - i64::from(height) / 2),
        4 => (x, y - i64::from(height) / 2),
        5 => (x - i64::from(width), y - i64::from(height)),
        6 => (x - i64::from(width), y),
        7 => (x, y - i64::from(height)),
        8 => (x, y),
        _ => (x - i64::from(width) / 2, y - i64::from(height) / 2),
    };
    Ok(Rect {
        x: i32::try_from(x).map_err(|_| "popup x coordinate overflows i32".to_string())?,
        y: i32::try_from(y).map_err(|_| "popup y coordinate overflows i32".to_string())?,
        width,
        height,
    })
}

fn flip_x(direction: u32) -> u32 {
    match direction {
        3 => 4,
        4 => 3,
        5 => 7,
        6 => 8,
        7 => 5,
        8 => 6,
        other => other,
    }
}

fn flip_y(direction: u32) -> u32 {
    match direction {
        1 => 2,
        2 => 1,
        5 => 6,
        6 => 5,
        7 => 8,
        8 => 7,
        other => other,
    }
}

fn constrained_x(rect: Rect, bounds: Bounds) -> bool {
    rect.x < 0 || i64::from(rect.x) + i64::from(rect.width) > bounds.width
}

fn constrained_y(rect: Rect, bounds: Bounds) -> bool {
    rect.y < 0 || i64::from(rect.y) + i64::from(rect.height) > bounds.height
}

fn resize_x(rect: &mut Rect, bounds: Bounds) {
    let right = i64::from(rect.x) + i64::from(rect.width);
    if rect.x < 0 {
        rect.x = 0;
        rect.width = right.clamp(1, bounds.width) as i32;
    }
    let right = i64::from(rect.x) + i64::from(rect.width);
    if right > bounds.width {
        rect.width = (bounds.width - i64::from(rect.x))
            .max(1)
            .clamp(1, i64::from(i32::MAX)) as i32;
    }
}

fn resize_y(rect: &mut Rect, bounds: Bounds) {
    let bottom = i64::from(rect.y) + i64::from(rect.height);
    if rect.y < 0 {
        rect.y = 0;
        rect.height = bottom.clamp(1, bounds.height) as i32;
    }
    let bottom = i64::from(rect.y) + i64::from(rect.height);
    if bottom > bounds.height {
        rect.height = (bounds.height - i64::from(rect.y))
            .max(1)
            .clamp(1, i64::from(i32::MAX)) as i32;
    }
}

fn constrain_size(size: Size, min: Size, max: Size) -> Size {
    Size {
        width: constrain_dimension(size.width, min.width, max.width),
        height: constrain_dimension(size.height, min.height, max.height),
    }
}

fn constrain_dimension(value: u32, min: u32, max: u32) -> u32 {
    let value = value.max(min);
    if max == 0 {
        value
    } else {
        value.min(max.max(min))
    }
}
