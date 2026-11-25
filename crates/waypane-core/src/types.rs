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

use waypipe_client::{Rid, ShmData};

pub type WindowId = u32;
pub type SurfaceId = u32;
pub type FrameId = u64;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShmFormat {
    Argb8888,
    Xrgb8888,
}

#[derive(Debug, Clone)]
pub struct BufferView {
    pub rid: Rid,
    pub data: ShmData,
    pub offset: i32,
    pub width: i32,
    pub height: i32,
    pub stride: i32,
    pub format: ShmFormat,
    pub scale: i32,
    pub transform: u32,
    pub damage: Vec<Rect>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowKind {
    Toplevel,
    Popup { parent: WindowId },
    Subsurface { parent: WindowId },
}

#[derive(Debug, Clone)]
pub struct WindowInfo {
    pub id: WindowId,
    pub kind: WindowKind,
    pub title: String,
    pub app_id: String,
    pub geometry: Rect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowRequest {
    Maximized,
    Unmaximized,
    Fullscreen(Option<u32>),
    Unfullscreen,
    Minimized,
    Move,
    Resize { edges: u32 },
}

#[derive(Debug, Clone)]
pub struct Frame {
    pub id: FrameId,
    pub window_id: WindowId,
    pub surface_id: SurfaceId,
    pub parent: Option<(SurfaceId, Point)>,
    pub buffer: Option<BufferView>,
    pub z_order: Option<Vec<SurfaceId>>,
}

#[derive(Debug, Clone)]
pub enum CoreEvent {
    Negotiated {
        remote_version: u32,
        version: u32,
    },
    WindowCreated(WindowInfo),
    WindowMetadata {
        id: WindowId,
        title: String,
        app_id: String,
        geometry: Rect,
    },
    WindowClosed {
        id: WindowId,
    },
    WindowRequest {
        id: WindowId,
        request: WindowRequest,
    },
    WindowConfigured {
        id: WindowId,
        size: Option<Size>,
        states: Vec<u32>,
    },
    Frame(Frame),
    CursorChanged {
        window_id: Option<WindowId>,
        buffer: Option<BufferView>,
        hotspot: Point,
    },
    PipeAvailable {
        rid: Rid,
    },
    ClipboardChanged {
        source: Option<u32>,
        mime_types: Vec<String>,
    },
    ClipboardData {
        mime_type: String,
        data: Vec<u8>,
    },
    TextInputState {
        surface_id: SurfaceId,
        enabled: bool,
        content_hint: u32,
        content_purpose: u32,
        cursor_rect: Rect,
    },
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputState {
    Pressed = 1,
    Released = 0,
}

pub const DEFAULT_KEYMAP: &[u8] = br#"xkb_keymap {
    xkb_keycodes { include "evdev+aliases(qwerty)" };
    xkb_types { include "complete" };
    xkb_compatibility { include "complete" };
    xkb_symbols { include "pc+us+inet(evdev)" };
};
"#;
