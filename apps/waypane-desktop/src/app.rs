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

use std::collections::HashMap;
use std::time::{Duration, Instant};

use waypane_core::{Connection, CoreEvent, WindowId, MAX_WAYLAND_SCALE};
use waypane_render::{keys, DesktopRenderer};
use waypipe_client::ClientConfig;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow};
use winit::keyboard::PhysicalKey;
use winit::window::WindowId as NativeWindowId;

use crate::session::{ExitWhenIdle, SessionEvent, SessionId, SessionManager};

pub struct App {
    connections: HashMap<SessionId, Connection>,
    renderer: DesktopRenderer,
    manager: SessionManager,
    config: ClientConfig,
    clock: Instant,
    presented_frames: usize,
    exit_after_frames: Option<usize>,
    exit_when_idle: ExitWhenIdle,
    finish_when_idle: bool,
    saw_session: bool,
    exit_code: u8,
    system_clipboard: Option<arboard::Clipboard>,
    clipboard_text: String,
    last_clipboard_check: Instant,
}

impl App {
    pub fn new(
        manager: SessionManager,
        config: ClientConfig,
        exit_after_frames: Option<usize>,
        exit_when_idle: ExitWhenIdle,
    ) -> Self {
        Self {
            connections: HashMap::new(),
            renderer: DesktopRenderer::new(),
            manager,
            config,
            clock: Instant::now(),
            presented_frames: 0,
            exit_after_frames,
            exit_when_idle,
            finish_when_idle: false,
            saw_session: false,
            exit_code: 0,
            system_clipboard: arboard::Clipboard::new().ok(),
            clipboard_text: String::new(),
            last_clipboard_check: Instant::now(),
        }
    }

    pub fn exit_code(&self) -> u8 {
        self.exit_code
    }

    fn millis(&self) -> u32 {
        self.clock.elapsed().as_millis() as u32
    }

    fn flush(&mut self, session: SessionId) {
        let Some(connection) = self.connections.get_mut(&session) else {
            return;
        };
        match connection.flush() {
            Ok(data) => {
                if !self.manager.send(session, data) {
                    log::warn!("dropping session {session}: outbound queue is closed or full");
                    self.disconnect(session);
                }
            }
            Err(error) => {
                log::error!("failed to encode session {session}: {error}");
                self.disconnect(session);
            }
        }
    }

    fn disconnect(&mut self, session: SessionId) {
        if let Some(mut connection) = self.connections.remove(&session) {
            if !connection.client().is_closed() {
                if let Ok(data) = connection.close() {
                    self.manager.send(session, data);
                }
            }
        }
        self.manager.close(session);
        self.renderer.close_session(session);
    }

    fn disconnect_all(&mut self) {
        for session in self.connections.keys().copied().collect::<Vec<_>>() {
            self.disconnect(session);
        }
    }

    fn handle_data(
        &mut self,
        event_loop: &ActiveEventLoop,
        session: SessionId,
        data: &[u8],
    ) -> Result<(), String> {
        let Some(connection) = self.connections.get_mut(&session) else {
            return Ok(());
        };
        let batch = connection
            .receive(data)
            .map_err(|error| error.to_string())?;
        if !batch.outgoing.is_empty() && !self.manager.send(session, batch.outgoing) {
            return Err("outbound queue is closed or full".to_string());
        }

        let (connections, renderer) = (&mut self.connections, &mut self.renderer);
        let connection = connections.get_mut(&session).unwrap();
        let mut closed = false;
        let mut clipboard_data = None;
        let mut clipboard_cleared = false;
        for event in batch.events {
            match event {
                CoreEvent::Negotiated {
                    remote_version,
                    version,
                } => log::info!(
                    "session {session} negotiated Waypipe protocol {version:#x} (server {remote_version:#x})"
                ),
                CoreEvent::WindowCreated(info) => renderer.window_created(session, info),
                CoreEvent::WindowMetadata {
                    id,
                    title,
                    app_id,
                    geometry,
                } => renderer.window_metadata(session, id, &title, &app_id, geometry),
                CoreEvent::WindowClosed { id } => renderer.close_core_window(session, id),
                CoreEvent::WindowRequest { id, request } => {
                    renderer.window_request(session, id, request);
                }
                CoreEvent::WindowConfigured { .. } => {}
                CoreEvent::Frame(frame) => {
                    if let Some(scale_factor) = renderer
                        .apply_frame(event_loop, session, &frame)
                        .map_err(|error| error.to_string())?
                    {
                        connection
                            .compositor_mut()
                            .set_window_scale(
                                frame.window_id,
                                wayland_scale_from_factor(scale_factor),
                            )
                            .map_err(|error| error.to_string())?;
                    }
                }
                CoreEvent::ClipboardChanged { source, mime_types } => {
                    if source.is_some() {
                        let outgoing = connection
                            .request_clipboard_text()
                            .map_err(|error| error.to_string())?;
                        if !outgoing.is_empty() && !self.manager.send(session, outgoing) {
                            return Err("outbound queue is closed or full".to_string());
                        }
                    } else if mime_types.is_empty() {
                        clipboard_cleared = true;
                    }
                }
                CoreEvent::ClipboardData { data, .. } => {
                    clipboard_data = Some(String::from_utf8_lossy(&data).into_owned());
                }
                CoreEvent::TextInputState { .. } | CoreEvent::CursorChanged { .. } => {}
                CoreEvent::PipeAvailable { rid } => {
                    log::debug!("clipboard pipe available for RID {}", rid.0);
                }
                CoreEvent::Closed => {
                    closed = true;
                    break;
                }
            }
        }
        let trailing = connection.flush().map_err(|error| error.to_string())?;
        if !trailing.is_empty() && !self.manager.send(session, trailing) {
            return Err("outbound queue is closed or full".to_string());
        }
        if closed {
            self.disconnect(session);
            return Ok(());
        }
        if clipboard_cleared {
            self.clipboard_text.clear();
            if let Some(clipboard) = self.system_clipboard.as_mut() {
                let _ = clipboard.set_text(String::new());
            }
        }
        if let Some(text) = clipboard_data {
            self.publish_clipboard(text, true);
        }
        Ok(())
    }

    fn publish_clipboard(&mut self, text: String, update_system: bool) {
        if text.len() > 16 * 1024 * 1024 {
            log::warn!("ignoring clipboard payload larger than 16 MiB");
            return;
        }
        self.clipboard_text = text.clone();
        if update_system {
            if let Some(clipboard) = self.system_clipboard.as_mut() {
                if let Err(error) = clipboard.set_text(text.clone()) {
                    log::debug!("failed to update system clipboard: {error}");
                }
            }
        }
        let manager = &self.manager;
        for (id, connection) in &mut self.connections {
            let _ = connection.compositor_mut().set_clipboard_text(&text);
            let _ = connection.compositor_mut().take_events();
            if let Ok(data) = connection.flush() {
                manager.send(*id, data);
            }
        }
    }

    fn poll_system_clipboard(&mut self) {
        if self.last_clipboard_check.elapsed() < Duration::from_millis(500) {
            return;
        }
        self.last_clipboard_check = Instant::now();
        let text = self
            .system_clipboard
            .as_mut()
            .and_then(|clipboard| clipboard.get_text().ok());
        if let Some(text) = text {
            if text != self.clipboard_text {
                self.publish_clipboard(text, false);
            }
        }
    }

    fn complete_presented(&mut self) {
        let time = self.millis();
        let manager = &self.manager;
        let connections = &mut self.connections;
        let mut completed = 0;
        for frame in self.renderer.take_presented() {
            if let Some(connection) = connections.get_mut(&frame.session) {
                match connection.complete_frame(frame.frame_id, time) {
                    Ok(data) => {
                        if manager.send(frame.session, data) {
                            completed += 1;
                        }
                    }
                    Err(error) => {
                        log::warn!("failed to complete frame {}: {error}", frame.frame_id)
                    }
                }
            }
        }
        self.presented_frames += completed;
        if self
            .exit_after_frames
            .is_some_and(|limit| self.presented_frames >= limit)
        {
            self.finish_when_idle = true;
        }
    }

    fn maybe_exit(&mut self, event_loop: &ActiveEventLoop) {
        if !self.connections.is_empty() || !self.renderer.is_empty() {
            return;
        }
        if let Some(code) = idle_exit_code(
            self.exit_when_idle,
            self.saw_session && self.exit_after_frames.is_some() && !self.finish_when_idle,
        ) {
            self.exit_code = code;
            event_loop.exit();
        }
    }

    fn core_window(&self, id: NativeWindowId) -> Option<(SessionId, WindowId)> {
        self.renderer.core_window(id)
    }
}

impl ApplicationHandler<SessionEvent> for App {
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {}

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: SessionEvent) {
        match event {
            SessionEvent::Connected(session) => {
                self.saw_session = true;
                self.connections
                    .insert(session, Connection::new(self.config));
            }
            SessionEvent::Data(session, data) => {
                if let Err(error) = self.handle_data(event_loop, session, &data) {
                    self.exit_code = 1;
                    log::error!("session {session} failed: {error}");
                    self.disconnect(session);
                    self.maybe_exit(event_loop);
                } else {
                    self.maybe_exit(event_loop);
                }
            }
            SessionEvent::Disconnected(session) => {
                self.disconnect(session);
                self.maybe_exit(event_loop);
            }
            SessionEvent::Error { session, message } => {
                self.exit_code = 1;
                if let Some(session) = session {
                    log::error!("session {session} transport error: {message}");
                    self.disconnect(session);
                    self.maybe_exit(event_loop);
                } else {
                    log::error!("transport error: {message}");
                    if self.connections.is_empty() && self.renderer.is_empty() {
                        event_loop.exit();
                    }
                }
            }
            SessionEvent::Output(message) => {
                for line in message.lines() {
                    log::info!("remote: {line}");
                }
            }
            SessionEvent::TransportClosed { message, failed } => {
                if failed {
                    self.exit_code = 1;
                    log::error!("{message}");
                } else {
                    log::debug!("{message}");
                }
                // The transport may die while forwarded windows are still open; its session
                // disconnects then arrive after this notification. Tear them down together.
                self.disconnect_all();
                event_loop.exit();
            }
        }
    }

    fn window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        id: NativeWindowId,
        event: WindowEvent,
    ) {
        let Some((session, core_id)) = self.core_window(id) else {
            return;
        };
        if matches!(
            event,
            WindowEvent::Occluded(false) | WindowEvent::Focused(true)
        ) {
            self.renderer.request_redraw(id);
        }
        let time = self.millis();
        let scale = self.renderer.scale_factor(id).unwrap_or(1.0);
        let Some(connection) = self.connections.get_mut(&session) else {
            return;
        };
        let result = match event {
            WindowEvent::CloseRequested => connection.compositor_mut().close_window(core_id),
            WindowEvent::Resized(size) => {
                let Some((_, core_id, logical)) = self.renderer.resize(id, size.width, size.height)
                else {
                    return;
                };
                let compositor = connection.compositor_mut();
                compositor
                    .set_window_scale(core_id, wayland_scale_from_factor(scale))
                    .and_then(|()| compositor.configure_window(core_id, Some(logical), vec![4]))
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => connection
                .compositor_mut()
                .set_window_scale(core_id, wayland_scale_from_factor(scale_factor)),
            WindowEvent::Focused(focused) => connection
                .compositor_mut()
                .keyboard_focus(focused.then_some(core_id)),
            WindowEvent::CursorMoved { position, .. } => self
                .renderer
                .surface_point(id, position)
                .map_or(Ok(()), |(x, y)| {
                    connection
                        .compositor_mut()
                        .pointer_motion(core_id, x, y, time)
                }),
            WindowEvent::CursorLeft { .. } => {
                connection.compositor_mut().pointer_focus(None, 0.0, 0.0)
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let button = match button {
                    MouseButton::Left => 0x110,
                    MouseButton::Right => 0x111,
                    MouseButton::Middle => 0x112,
                    MouseButton::Back => 0x113,
                    MouseButton::Forward => 0x114,
                    MouseButton::Other(value) => u32::from(value),
                };
                connection.compositor_mut().pointer_button(
                    button,
                    state == ElementState::Pressed,
                    time,
                )
            }
            WindowEvent::MouseWheel { delta, .. } => match delta {
                MouseScrollDelta::LineDelta(x, y) => {
                    let horizontal = if x == 0.0 {
                        Ok(())
                    } else {
                        connection.compositor_mut().pointer_axis(
                            false,
                            -f64::from(x) * 10.0,
                            Some((-x * 120.0) as i32),
                            time,
                        )
                    };
                    horizontal.and_then(|()| {
                        connection.compositor_mut().pointer_axis(
                            true,
                            -f64::from(y) * 10.0,
                            Some((-y * 120.0) as i32),
                            time,
                        )
                    })
                }
                MouseScrollDelta::PixelDelta(position) => {
                    let vertical = connection.compositor_mut().pointer_axis(
                        true,
                        -position.y / scale,
                        None,
                        time,
                    );
                    vertical.and_then(|()| {
                        if position.x == 0.0 {
                            Ok(())
                        } else {
                            connection.compositor_mut().pointer_axis(
                                false,
                                -position.x / scale,
                                None,
                                time,
                            )
                        }
                    })
                }
            },
            WindowEvent::KeyboardInput { event, .. } => {
                if event.repeat {
                    return;
                }
                let PhysicalKey::Code(code) = event.physical_key else {
                    return;
                };
                let Some(key) = keys::evdev_keycode(code) else {
                    return;
                };
                connection
                    .compositor_mut()
                    .key(key, event.state == ElementState::Pressed, time)
            }
            WindowEvent::RedrawRequested => {
                log::trace!("received redraw request for native window {id:?}");
                self.renderer.render(id);
                self.complete_presented();
                return;
            }
            _ => return,
        };
        if let Err(error) = result {
            log::warn!("failed to handle window event: {error}");
            self.disconnect(session);
            return;
        }
        self.flush(session);
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.poll_system_clipboard();
        event_loop.set_control_flow(ControlFlow::WaitUntil(
            self.last_clipboard_check + Duration::from_millis(500),
        ));
    }

    fn new_events(&mut self, event_loop: &ActiveEventLoop, _cause: winit::event::StartCause) {
        event_loop.set_control_flow(ControlFlow::Wait);
    }
}

fn idle_exit_code(exit_when_idle: ExitWhenIdle, unfinished_frame_limit: bool) -> Option<u8> {
    let ExitWhenIdle::Yes = exit_when_idle else {
        return None;
    };
    Some(u8::from(unfinished_frame_limit))
}

fn wayland_scale_from_factor(scale_factor: f64) -> i32 {
    if !scale_factor.is_finite() || scale_factor <= 0.0 {
        return 1;
    }
    (scale_factor.ceil() as i32).clamp(1, MAX_WAYLAND_SCALE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_command_sessions_exit_when_the_last_window_closes() {
        assert_eq!(idle_exit_code(ExitWhenIdle::No, false), None);
        assert_eq!(idle_exit_code(ExitWhenIdle::Yes, false), Some(0));
        assert_eq!(idle_exit_code(ExitWhenIdle::Yes, true), Some(1));
    }

    #[test]
    fn converts_host_scale_factor_to_integer_wayland_scale() {
        for (factor, expected) in [
            (f64::NAN, 1),
            (f64::INFINITY, 1),
            (0.0, 1),
            (-1.0, 1),
            (1.0, 1),
            (1.01, 2),
            (1.5, 2),
            (2.0, 2),
            (2.5, 3),
            (100.0, MAX_WAYLAND_SCALE),
        ] {
            assert_eq!(
                wayland_scale_from_factor(factor),
                expected,
                "unexpected scale for {factor}"
            );
        }
    }
}
