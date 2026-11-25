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

#[cfg(feature = "desktop")]
mod desktop;
mod image;
mod present;
mod viewport;

#[cfg(feature = "desktop")]
pub mod keys;

#[cfg(feature = "desktop")]
pub use desktop::{DesktopError, DesktopRenderer, PresentedFrame};
pub use image::{decode_buffer, output_size, Canvas, DecodedBuffer, ImageError, UploadRegion};
pub use present::{canvas_texture_format, preferred_surface_format};
pub use viewport::{Viewport, ViewportTransform, MAX_VIEWPORT_ZOOM, MIN_VIEWPORT_ZOOM};
