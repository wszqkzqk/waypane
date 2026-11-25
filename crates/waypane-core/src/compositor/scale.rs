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

pub const MAX_WAYLAND_SCALE: i32 = 8;
const MAX_SCALE_PARENT_DEPTH: usize = 64;

impl Compositor {
    pub fn output_scale(&self) -> i32 {
        self.output_scale
    }

    pub fn configured_size(&self, window: WindowId) -> Option<Size> {
        self.xdg_surfaces
            .values()
            .find(|surface| surface.window_id == Some(window))
            .and_then(|surface| surface.configured_size)
    }

    pub fn set_output_scale(&mut self, scale: i32) -> Result<()> {
        validate_scale(scale)?;
        if self.output_scale == scale && self.window_scales.is_empty() {
            return Ok(());
        }
        self.output_scale = scale;
        self.window_scales.clear();

        let outputs = self.outputs.clone();
        for output in outputs {
            if self
                .objects
                .get(&output)
                .is_some_and(|object| object.version >= 2)
            {
                self.emit(
                    &wl::WL_OUTPUT_INTERFACE,
                    "scale",
                    output,
                    &[Argument::Int(scale)],
                );
                self.emit(&wl::WL_OUTPUT_INTERFACE, "done", output, &[]);
            }
        }
        let surfaces = self.surfaces.keys().copied().collect::<Vec<_>>();
        for surface in surfaces {
            self.emit_preferred_scale_value(surface, scale, true);
        }
        Ok(())
    }

    pub fn set_window_scale(&mut self, window: WindowId, scale: i32) -> Result<()> {
        validate_scale(scale)?;
        if !self.windows.contains_key(&window) {
            return Err(Error::protocol(format!(
                "cannot set scale for unknown window {window}"
            )));
        }
        if self.preferred_window_scale(window) == scale {
            self.window_scales.entry(window).or_insert(scale);
            return Ok(());
        }
        self.window_scales.insert(window, scale);
        self.emit_window_preferred_scale(window);

        let descendants = self
            .windows
            .keys()
            .filter(|candidate| {
                **candidate != window
                    && !self.window_scales.contains_key(*candidate)
                    && self.popup_descendant_or_self(**candidate, window)
            })
            .copied()
            .collect::<Vec<_>>();
        for descendant in descendants {
            self.emit_window_preferred_scale(descendant);
        }
        Ok(())
    }

    pub(super) fn emit_preferred_scale(&mut self, surface: SurfaceId) {
        let scale = self
            .surfaces
            .get(&surface)
            .and_then(|surface| surface.window_id)
            .map_or(self.output_scale, |window| {
                self.preferred_window_scale(window)
            });
        self.emit_preferred_scale_value(surface, scale, false);
    }

    pub(super) fn emit_window_preferred_scale(&mut self, window: WindowId) {
        let scale = self.preferred_window_scale(window);
        let surfaces = self
            .surfaces
            .iter()
            .filter(|(_, surface)| surface.window_id == Some(window))
            .map(|(surface, _)| *surface)
            .collect::<Vec<_>>();
        for surface in surfaces {
            self.emit_preferred_scale_value(surface, scale, false);
        }
    }

    fn preferred_window_scale(&self, window: WindowId) -> i32 {
        let mut current = Some(window);
        for _ in 0..MAX_SCALE_PARENT_DEPTH {
            let Some(id) = current else {
                break;
            };
            if let Some(scale) = self.window_scales.get(&id) {
                return *scale;
            }
            current = match self.windows.get(&id).map(|window| window.kind) {
                Some(WindowKind::Popup { parent }) => Some(parent),
                _ => None,
            };
        }
        self.output_scale
    }

    fn emit_preferred_scale_value(&mut self, surface: SurfaceId, scale: i32, force: bool) {
        if (force || scale != 1)
            && self
                .objects
                .get(&surface)
                .is_some_and(|object| object.version >= 6)
        {
            self.emit(
                &wl::WL_SURFACE_INTERFACE,
                "preferred_buffer_scale",
                surface,
                &[Argument::Int(scale)],
            );
        }
    }
}

fn validate_scale(scale: i32) -> Result<()> {
    if !(1..=MAX_WAYLAND_SCALE).contains(&scale) {
        return Err(Error::protocol(format!(
            "invalid Wayland buffer scale {scale}; expected 1..={MAX_WAYLAND_SCALE}"
        )));
    }
    Ok(())
}
