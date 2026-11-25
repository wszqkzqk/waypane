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

use std::collections::VecDeque;

use crate::protocol::interface_by_name;
use crate::types::{Point, Rect, ShmFormat, SurfaceId, WindowId, WindowKind};
use crate::wayland_backend::protocol::Interface;
use waypipe_client::Rid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ObjectKind {
    Display,
    Registry,
    Callback,
    Compositor,
    Subcompositor,
    Shm,
    ShmPool,
    Buffer,
    Surface,
    Subsurface,
    Region,
    XdgWmBase,
    XdgSurface,
    XdgToplevel,
    XdgPopup,
    XdgPositioner,
    Seat,
    Keyboard,
    Pointer,
    Touch,
    Output,
    DataDeviceManager,
    DataDevice,
    DataSource,
    DataOffer,
    TextInputManager,
    TextInput,
    Other,
}

#[derive(Debug, Clone)]
pub(crate) struct ObjectRecord {
    pub interface: &'static Interface,
    pub version: u32,
    pub kind: ObjectKind,
}

impl ObjectRecord {
    pub fn new(interface: &'static Interface, version: u32) -> Self {
        Self {
            interface,
            version,
            kind: kind_for_name(interface.name),
        }
    }
}

pub(crate) fn kind_for_name(name: &str) -> ObjectKind {
    match name {
        "wl_display" => ObjectKind::Display,
        "wl_registry" => ObjectKind::Registry,
        "wl_callback" => ObjectKind::Callback,
        "wl_compositor" => ObjectKind::Compositor,
        "wl_subcompositor" => ObjectKind::Subcompositor,
        "wl_shm" => ObjectKind::Shm,
        "wl_shm_pool" => ObjectKind::ShmPool,
        "wl_buffer" => ObjectKind::Buffer,
        "wl_surface" => ObjectKind::Surface,
        "wl_subsurface" => ObjectKind::Subsurface,
        "wl_region" => ObjectKind::Region,
        "xdg_wm_base" => ObjectKind::XdgWmBase,
        "xdg_surface" => ObjectKind::XdgSurface,
        "xdg_toplevel" => ObjectKind::XdgToplevel,
        "xdg_popup" => ObjectKind::XdgPopup,
        "xdg_positioner" => ObjectKind::XdgPositioner,
        "wl_seat" => ObjectKind::Seat,
        "wl_keyboard" => ObjectKind::Keyboard,
        "wl_pointer" => ObjectKind::Pointer,
        "wl_touch" => ObjectKind::Touch,
        "wl_output" => ObjectKind::Output,
        "wl_data_device_manager" => ObjectKind::DataDeviceManager,
        "wl_data_device" => ObjectKind::DataDevice,
        "wl_data_source" => ObjectKind::DataSource,
        "wl_data_offer" => ObjectKind::DataOffer,
        "zwp_text_input_manager_v3" => ObjectKind::TextInputManager,
        "zwp_text_input_v3" => ObjectKind::TextInput,
        _ => ObjectKind::Other,
    }
}

pub(crate) fn global_interface(name: &str, version: u32) -> Option<ObjectRecord> {
    interface_by_name(name).map(|interface| ObjectRecord::new(interface, version))
}

const MAX_REGION_RECTS: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RegionRect {
    left: i64,
    top: i64,
    right: i64,
    bottom: i64,
}

impl RegionRect {
    fn empty(rect: Rect) -> Result<Option<Self>, ()> {
        if rect.width < 0 || rect.height < 0 {
            return Err(());
        }
        let left = i64::from(rect.x);
        let top = i64::from(rect.y);
        let rect = Self {
            left,
            top,
            right: left + i64::from(rect.width),
            bottom: top + i64::from(rect.height),
        };
        Ok((rect.left < rect.right && rect.top < rect.bottom).then_some(rect))
    }

    fn contains(self, other: Self) -> bool {
        self.left <= other.left
            && self.top <= other.top
            && self.right >= other.right
            && self.bottom >= other.bottom
    }

    fn contains_point(self, x: f64, y: f64) -> bool {
        x >= self.left as f64
            && x < self.right as f64
            && y >= self.top as f64
            && y < self.bottom as f64
    }

    fn difference(self, cut: Self) -> Vec<Self> {
        let left = self.left.max(cut.left);
        let top = self.top.max(cut.top);
        let right = self.right.min(cut.right);
        let bottom = self.bottom.min(cut.bottom);
        if left >= right || top >= bottom {
            return vec![self];
        }
        let mut parts = Vec::with_capacity(4);
        let middle = Self {
            left: self.left,
            top,
            right: self.right,
            bottom,
        };
        if self.top < top {
            parts.push(Self {
                top: self.top,
                bottom: top,
                ..middle
            });
        }
        if self.left < left {
            parts.push(Self {
                right: left,
                ..middle
            });
        }
        if right < self.right {
            parts.push(Self {
                left: right,
                ..middle
            });
        }
        if bottom < self.bottom {
            parts.push(Self {
                top: bottom,
                bottom: self.bottom,
                ..middle
            });
        }
        parts
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Region {
    rects: Vec<RegionRect>,
}

impl Region {
    pub fn add(&mut self, rect: Rect) -> Result<(), ()> {
        let Some(added) = RegionRect::empty(rect)? else {
            return Ok(());
        };
        let mut rects = self.rects.clone();
        rects.retain(|existing| !added.contains(*existing));
        let mut fragments = vec![added];
        for existing in &rects {
            fragments = fragments
                .into_iter()
                .flat_map(|fragment| fragment.difference(*existing))
                .collect::<Vec<_>>();
            if fragments.is_empty() {
                break;
            }
            if rects.len() + fragments.len() > MAX_REGION_RECTS {
                return Err(());
            }
        }
        rects.extend(fragments);
        self.rects = rects;
        Ok(())
    }

    pub fn subtract(&mut self, rect: Rect) -> Result<(), ()> {
        let Some(cut) = RegionRect::empty(rect)? else {
            return Ok(());
        };
        let mut rects = Vec::new();
        for existing in &self.rects {
            for fragment in existing.difference(cut) {
                if rects.len() == MAX_REGION_RECTS {
                    return Err(());
                }
                rects.push(fragment);
            }
        }
        self.rects = rects;
        Ok(())
    }

    pub fn contains(&self, x: f64, y: f64) -> bool {
        self.rects.iter().any(|rect| rect.contains_point(x, y))
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SurfaceCommit {
    pub buffer: Option<u32>,
    pub offset: Point,
    pub scale: i32,
    pub transform: u32,
    pub input_region: Option<Region>,
}

impl Default for SurfaceCommit {
    fn default() -> Self {
        Self {
            buffer: None,
            offset: Point::default(),
            scale: 1,
            transform: 0,
            input_region: None,
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct PendingSurface {
    pub attach: Option<Option<u32>>,
    pub offset: Option<Point>,
    pub scale: Option<i32>,
    pub transform: Option<u32>,
    pub input_region: Option<Option<Region>>,
    pub damage: Vec<(Rect, bool)>,
    pub callbacks: Vec<u32>,
    pub position: Option<Point>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Role {
    #[default]
    None,
    Toplevel(WindowId),
    Popup(WindowId),
    Subsurface {
        parent: u32,
        proxy: u32,
    },
    Cursor {
        pointer: u32,
    },
}

#[derive(Debug, Default)]
pub(crate) struct Surface {
    pub committed: SurfaceCommit,
    pub pending: PendingSurface,
    pub role: Role,
    pub window_id: Option<WindowId>,
    pub position: Point,
    pub logical_size: crate::types::Size,
    pub mapped: bool,
    pub outputs: Vec<u32>,
}

#[derive(Debug, Default)]
pub(crate) struct SubsurfaceSync {
    pub surface: u32,
    pub synchronized: bool,
    pub cached: std::collections::VecDeque<PendingSurface>,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct PendingTextInput {
    pub enabled: Option<bool>,
    pub content_hint: Option<u32>,
    pub content_purpose: Option<u32>,
    pub surrounding_text: Option<String>,
    pub cursor_rect: Option<crate::types::Rect>,
}

#[derive(Debug, Default)]
pub(crate) struct TextInput {
    pub seat_id: u32,
    pub surface_id: Option<u32>,
    pub focused: bool,
    pub enabled: bool,
    pub commit_count: u32,
    pub pending: PendingTextInput,
    pub content_hint: u32,
    pub content_purpose: u32,
    pub surrounding_text: Option<String>,
    pub cursor_rect: crate::types::Rect,
}

#[derive(Debug)]
pub(crate) struct DataDevice {
    pub focused: bool,
    pub offered: std::collections::BTreeSet<u32>,
}

#[derive(Debug, Default)]
pub(crate) struct DataSource {
    pub mime_types: Vec<String>,
    pub cached_text: Option<std::sync::Arc<[u8]>>,
}

#[derive(Debug, Clone)]
pub(crate) enum SelectionState {
    Host {
        text: std::sync::Arc<[u8]>,
        mime_types: Vec<String>,
    },
    Remote {
        source_id: u32,
        mime_types: Vec<String>,
    },
}

impl SelectionState {
    pub fn mime_types(&self) -> &[String] {
        match self {
            Self::Host { mime_types, .. } | Self::Remote { mime_types, .. } => mime_types,
        }
    }
}

#[derive(Debug)]
pub(crate) struct ClipboardRead {
    pub mime_type: String,
    pub data: Vec<u8>,
    pub forwards: Vec<waypipe_client::Rid>,
    pub source_id: Option<u32>,
}

#[derive(Debug)]
pub(crate) struct Pool {
    pub rid: Rid,
    pub size: usize,
    pub buffer_refs: usize,
    pub destroyed: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct Buffer {
    pub pool_id: u32,
    pub rid: Rid,
    pub offset: i32,
    pub width: i32,
    pub height: i32,
    pub stride: i32,
    pub format: ShmFormat,
    pub alive: bool,
}

impl Buffer {
    pub fn required_bytes(&self) -> Option<usize> {
        if self.width <= 0 || self.height <= 0 || self.stride < self.width.saturating_mul(4) {
            return None;
        }
        let last_row = usize::try_from(self.height - 1).ok()?;
        let stride = usize::try_from(self.stride).ok()?;
        let row_bytes = usize::try_from(self.width).ok()?.checked_mul(4)?;
        let offset = usize::try_from(self.offset).ok()?;
        last_row
            .checked_mul(stride)?
            .checked_add(row_bytes)?
            .checked_add(offset)
    }
}

#[derive(Debug, Default)]
pub(crate) struct XdgSurface {
    pub surface_id: u32,
    pub window_id: Option<WindowId>,
    pub pending_configures: VecDeque<PendingConfigure>,
    pub initial_ack_seen: bool,
    pub configured_size: Option<crate::types::Size>,
    pub configured_states: Vec<u32>,
    pub min_size: crate::types::Size,
    pub max_size: crate::types::Size,
}

impl XdgSurface {
    pub(crate) fn record_configure(&mut self, serial: u32, geometry: Option<Rect>) {
        const MAX_PENDING_CONFIGURES: usize = 64;

        if self.pending_configures.len() == MAX_PENDING_CONFIGURES {
            self.pending_configures.pop_front();
        }
        self.pending_configures
            .push_back(PendingConfigure { serial, geometry });
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PendingConfigure {
    pub serial: u32,
    pub geometry: Option<Rect>,
}

#[derive(Debug)]
pub(crate) struct Window {
    pub id: WindowId,
    pub xdg_surface_id: u32,
    pub surface_id: u32,
    pub kind: WindowKind,
    pub title: String,
    pub app_id: String,
    pub geometry: Rect,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Positioner {
    pub size: Option<(i32, i32)>,
    pub anchor_rect: Option<Rect>,
    pub anchor: u32,
    pub gravity: u32,
    pub constraint_adjustment: u32,
    pub offset: Point,
    pub reactive: bool,
    pub parent_size: Option<(i32, i32)>,
    pub parent_serial: Option<u32>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct CursorState {
    pub surface: u32,
    pub hotspot: Point,
}

#[derive(Debug, Default)]
pub(crate) struct PointerState {
    pub focus: Option<(WindowId, SurfaceId)>,
    pub enter_serial: Option<u32>,
    pub cursor: Option<CursorState>,
}

#[derive(Debug, Default)]
pub(crate) struct Seat {
    pub keyboards: Vec<u32>,
    pub pointers: Vec<u32>,
    pub touches: Vec<u32>,
    pub data_devices: Vec<u32>,
}
