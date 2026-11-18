use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use serde::{Deserialize, Serialize};
use waypane_core::{Connection, CoreEvent, Frame, Point, Size, WindowId, WindowKind};
use waypane_render::{Canvas, ImageError};
use waypipe_client::{ClientConfig, Compression};

use crate::session::SessionId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SurfaceKey {
    pub session: SessionId,
    pub window: WindowId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AndroidWindow {
    pub key: SurfaceKey,
    pub title: String,
    pub app_id: String,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EngineEvent {
    Connected {
        session: SessionId,
    },
    WindowUpsert(AndroidWindow),
    WindowRemoved {
        key: SurfaceKey,
    },
    SessionClosed {
        session: SessionId,
    },
    Status {
        message: String,
    },
    ClipboardText {
        session: SessionId,
        text: String,
    },
    Error {
        session: Option<SessionId>,
        message: String,
    },
}

#[derive(Debug, Default)]
pub struct EngineOutput {
    pub outgoing: Bytes,
    pub extra_outgoing: Vec<(SessionId, Bytes)>,
    pub events: Vec<EngineEvent>,
}

impl EngineOutput {
    fn join(&mut self, mut other: EngineOutput) {
        let mut outgoing = BytesMut::from(self.outgoing.as_ref());
        outgoing.extend_from_slice(&other.outgoing);
        self.outgoing = outgoing.freeze();
        self.extra_outgoing.append(&mut other.extra_outgoing);
        self.events.extend(other.events);
    }
}

#[derive(Debug, Default)]
pub struct SurfaceSignal {
    dirty: AtomicBool,
    version: AtomicU64,
    wake_epoch: AtomicU64,
    wait_lock: Mutex<()>,
    condvar: Condvar,
}

impl SurfaceSignal {
    pub fn is_dirty(&self) -> bool {
        self.dirty.load(Ordering::Acquire)
    }

    pub fn mark_dirty(&self, version: u64) {
        self.version.store(version, Ordering::Release);
        self.dirty.store(true, Ordering::Release);
        self.condvar.notify_all();
    }

    pub fn mark_clean(&self) {
        self.dirty.store(false, Ordering::Release);
    }

    pub fn wake(&self) {
        self.wake_epoch.fetch_add(1, Ordering::AcqRel);
        self.condvar.notify_all();
    }

    pub fn wait_dirty(&self, seen_version: u64, timeout: Duration) -> Option<u64> {
        if let Some(version) = self.dirty_version(seen_version) {
            return Some(version);
        }
        let epoch = self.wake_epoch.load(Ordering::Acquire);
        let guard = self
            .wait_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(version) = self.dirty_version(seen_version) {
            return Some(version);
        }
        let _ = self.condvar.wait_timeout_while(guard, timeout, |_| {
            self.dirty_version(seen_version).is_none()
                && self.wake_epoch.load(Ordering::Acquire) == epoch
        });
        self.dirty_version(seen_version)
    }

    fn dirty_version(&self, seen_version: u64) -> Option<u64> {
        if !self.is_dirty() {
            return None;
        }
        let version = self.version.load(Ordering::Acquire);
        (version > seen_version).then_some(version)
    }
}

#[derive(Debug)]
pub struct AndroidSurface {
    pub canvas: Canvas,
    pub version: u64,
    pub signal: Arc<SurfaceSignal>,
    scale: i32,
    frames: VecDeque<(u64, u64)>,
}

impl AndroidSurface {
    fn new() -> Self {
        Self {
            canvas: Canvas::default(),
            version: 0,
            signal: Arc::new(SurfaceSignal::default()),
            scale: 1,
            frames: VecDeque::new(),
        }
    }

    pub fn signal(&self) -> Arc<SurfaceSignal> {
        self.signal.clone()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Core(#[from] waypane_core::Error),
    #[error(transparent)]
    Image(#[from] ImageError),
    #[error("unknown session {0}")]
    UnknownSession(SessionId),
    #[error("unknown Android surface {0:?}")]
    UnknownSurface(SurfaceKey),
    #[error("surface {0:?} has no rendered frame")]
    EmptySurface(SurfaceKey),
}

struct PopupInfo {
    parent: WindowId,
    position: Point,
}

pub struct AndroidEngine {
    config: ClientConfig,
    connections: HashMap<SessionId, Connection>,
    surfaces: HashMap<SurfaceKey, Arc<Mutex<AndroidSurface>>>,
    windows: HashMap<SurfaceKey, AndroidWindow>,
    popups: HashMap<SurfaceKey, PopupInfo>,
    host_clipboard: Option<String>,
    clock: Instant,
}

impl AndroidEngine {
    pub fn new(compression: Compression) -> Self {
        Self {
            config: ClientConfig {
                preferred_compression: compression,
                ..ClientConfig::default()
            },
            connections: HashMap::new(),
            surfaces: HashMap::new(),
            windows: HashMap::new(),
            popups: HashMap::new(),
            host_clipboard: None,
            clock: Instant::now(),
        }
    }

    pub fn connected(&mut self, session: SessionId) -> EngineOutput {
        self.connections
            .insert(session, Connection::new(self.config));
        if let Some(text) = self.host_clipboard.clone() {
            if let Some(connection) = self.connections.get_mut(&session) {
                let _ = connection.compositor_mut().set_clipboard_text(&text);
                let _ = connection.compositor_mut().take_events();
            }
        }
        EngineOutput {
            outgoing: Bytes::new(),
            extra_outgoing: Vec::new(),
            events: vec![EngineEvent::Connected { session }],
        }
    }

    pub fn receive(
        &mut self,
        session: SessionId,
        data: &[u8],
    ) -> Result<EngineOutput, EngineError> {
        let batch = self
            .connections
            .get_mut(&session)
            .ok_or(EngineError::UnknownSession(session))?
            .receive(data)?;
        let closed = batch
            .events
            .iter()
            .any(|event| matches!(event, CoreEvent::Closed));
        let events = batch
            .events
            .into_iter()
            .filter_map(|event| match event {
                CoreEvent::PipeAvailable { .. } => None,
                other => Some(other),
            })
            .collect::<Vec<_>>();
        let mut output = self.apply_core_events(session, events)?;
        if let Some(connection) = self.connections.get_mut(&session) {
            let flush = connection.flush()?;
            let mut bytes = BytesMut::from(batch.outgoing.as_ref());
            bytes.extend_from_slice(&flush);
            output.outgoing = bytes.freeze();
        }
        if closed {
            output.join(self.disconnect(session, false)?);
        }
        Ok(output)
    }

    pub fn surface(&self, key: SurfaceKey) -> Option<Arc<Mutex<AndroidSurface>>> {
        self.surfaces.get(&key).cloned()
    }

    pub fn set_clipboard_text(&mut self, text: &str) -> Result<EngineOutput, EngineError> {
        self.host_clipboard = Some(text.to_owned());
        let mut output = EngineOutput::default();
        for session in self.session_ids() {
            if let Some(connection) = self.connections.get_mut(&session) {
                connection.compositor_mut().set_clipboard_text(text)?;
                let _ = connection.compositor_mut().take_events();
                let flushed = connection.flush()?;
                if !flushed.is_empty() {
                    output.extra_outgoing.push((session, flushed));
                }
            }
        }
        Ok(output)
    }

    pub fn session_ids(&self) -> Vec<SessionId> {
        let mut sessions = self.connections.keys().copied().collect::<Vec<_>>();
        sessions.sort_unstable();
        sessions
    }

    pub fn windows(&self) -> Vec<AndroidWindow> {
        let mut windows = self.windows.values().cloned().collect::<Vec<_>>();
        windows.sort_by_key(|window| window.key);
        windows
    }

    pub fn logical_point(
        &self,
        key: SurfaceKey,
        x: f64,
        y: f64,
    ) -> Result<(f64, f64), EngineError> {
        let surface = self.surface(key).ok_or(EngineError::UnknownSurface(key))?;
        let surface = surface
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if surface.canvas.size().width == 0 || surface.canvas.size().height == 0 {
            return Err(EngineError::EmptySurface(key));
        }
        let scale = f64::from(surface.scale.max(1));
        Ok((
            x.clamp(0.0, 1.0) * f64::from(surface.canvas.size().width) / scale,
            y.clamp(0.0, 1.0) * f64::from(surface.canvas.size().height) / scale,
        ))
    }

    pub fn presented(
        &mut self,
        key: SurfaceKey,
        version: u64,
    ) -> Result<EngineOutput, EngineError> {
        let surface = self.surface(key).ok_or(EngineError::UnknownSurface(key))?;
        let frames = {
            let mut surface = surface
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut frames = Vec::new();
            while surface
                .frames
                .front()
                .is_some_and(|(candidate, _)| *candidate <= version)
            {
                frames.push(surface.frames.pop_front().unwrap().1);
            }
            surface.signal.mark_clean();
            frames
        };
        let time = self.millis();
        let connection = self
            .connections
            .get_mut(&key.session)
            .ok_or(EngineError::UnknownSession(key.session))?;
        let mut outgoing = BytesMut::new();
        for frame in frames {
            outgoing.extend_from_slice(&connection.complete_frame(frame, time)?);
        }
        Ok(EngineOutput {
            outgoing: outgoing.freeze(),
            extra_outgoing: Vec::new(),
            events: Vec::new(),
        })
    }

    pub fn touch_down(
        &mut self,
        key: SurfaceKey,
        id: i32,
        x: f64,
        y: f64,
    ) -> Result<EngineOutput, EngineError> {
        let (x, y) = self.logical_point(key, x, y)?;
        let time = self.millis();
        let connection = self
            .connections
            .get_mut(&key.session)
            .ok_or(EngineError::UnknownSession(key.session))?;
        connection
            .compositor_mut()
            .keyboard_focus(Some(key.window))?;
        connection
            .compositor_mut()
            .touch_down(key.window, id, x, y, time)?;
        self.flush(key.session)
    }

    pub fn touch_motion(
        &mut self,
        key: SurfaceKey,
        id: i32,
        x: f64,
        y: f64,
    ) -> Result<EngineOutput, EngineError> {
        let (x, y) = self.logical_point(key, x, y)?;
        let time = self.millis();
        self.connections
            .get_mut(&key.session)
            .ok_or(EngineError::UnknownSession(key.session))?
            .compositor_mut()
            .touch_motion(key.window, id, x, y, time);
        self.flush(key.session)
    }

    pub fn touch_up(&mut self, key: SurfaceKey, id: i32) -> Result<EngineOutput, EngineError> {
        let time = self.millis();
        self.connections
            .get_mut(&key.session)
            .ok_or(EngineError::UnknownSession(key.session))?
            .compositor_mut()
            .touch_up(id, time);
        self.flush(key.session)
    }

    pub fn touch_cancel(&mut self, key: SurfaceKey) -> Result<EngineOutput, EngineError> {
        self.connections
            .get_mut(&key.session)
            .ok_or(EngineError::UnknownSession(key.session))?
            .compositor_mut()
            .touch_cancel();
        self.flush(key.session)
    }

    pub fn scroll(
        &mut self,
        key: SurfaceKey,
        x: f64,
        y: f64,
        horizontal: f64,
        vertical: f64,
    ) -> Result<EngineOutput, EngineError> {
        let (x, y) = self.logical_point(key, x, y)?;
        let time = self.millis();
        let connection = self
            .connections
            .get_mut(&key.session)
            .ok_or(EngineError::UnknownSession(key.session))?;
        connection
            .compositor_mut()
            .pointer_motion(key.window, x, y, time)?;
        if horizontal != 0.0 {
            connection
                .compositor_mut()
                .pointer_axis(false, horizontal, None, time)?;
        }
        if vertical != 0.0 {
            connection
                .compositor_mut()
                .pointer_axis(true, vertical, None, time)?;
        }
        self.flush(key.session)
    }

    pub fn key(
        &mut self,
        key: SurfaceKey,
        code: u32,
        pressed: bool,
    ) -> Result<EngineOutput, EngineError> {
        let time = self.millis();
        let connection = self
            .connections
            .get_mut(&key.session)
            .ok_or(EngineError::UnknownSession(key.session))?;
        connection
            .compositor_mut()
            .keyboard_focus(Some(key.window))?;
        connection.compositor_mut().key(code, pressed, time)?;
        self.flush(key.session)
    }

    pub fn pointer(
        &mut self,
        key: SurfaceKey,
        x: f64,
        y: f64,
        button: Option<(u32, bool)>,
    ) -> Result<EngineOutput, EngineError> {
        let (x, y) = self.logical_point(key, x, y)?;
        let time = self.millis();
        let connection = self
            .connections
            .get_mut(&key.session)
            .ok_or(EngineError::UnknownSession(key.session))?;
        connection
            .compositor_mut()
            .pointer_motion(key.window, x, y, time)?;
        if let Some((button, pressed)) = button {
            connection
                .compositor_mut()
                .pointer_button(button, pressed, time)?;
        }
        self.flush(key.session)
    }

    pub fn close_window(&mut self, key: SurfaceKey) -> Result<EngineOutput, EngineError> {
        self.connections
            .get_mut(&key.session)
            .ok_or(EngineError::UnknownSession(key.session))?
            .compositor_mut()
            .close_window(key.window)?;
        self.flush(key.session)
    }

    pub fn resize(&mut self, key: SurfaceKey, size: Size) -> Result<EngineOutput, EngineError> {
        self.connections
            .get_mut(&key.session)
            .ok_or(EngineError::UnknownSession(key.session))?
            .compositor_mut()
            .configure_window(key.window, Some(size), vec![4])?;
        self.flush(key.session)
    }

    pub fn disconnect(
        &mut self,
        session: SessionId,
        notify_remote: bool,
    ) -> Result<EngineOutput, EngineError> {
        let before = self.connections.len();
        let keys = self
            .windows
            .keys()
            .filter(|key| key.session == session)
            .copied()
            .collect::<Vec<_>>();
        let mut events = Vec::new();
        for key in keys {
            self.windows.remove(&key);
            self.surfaces.remove(&key);
            events.push(EngineEvent::WindowRemoved { key });
        }
        self.popups.retain(|key, _| key.session != session);
        let outgoing = self
            .connections
            .remove(&session)
            .filter(|_| notify_remote)
            .map(|mut connection| connection.close())
            .transpose()?
            .unwrap_or_default();
        if before > 0 || notify_remote {
            events.push(EngineEvent::SessionClosed { session });
        }
        Ok(EngineOutput {
            outgoing,
            extra_outgoing: Vec::new(),
            events,
        })
    }

    fn flush(&mut self, session: SessionId) -> Result<EngineOutput, EngineError> {
        let outgoing = self
            .connections
            .get_mut(&session)
            .ok_or(EngineError::UnknownSession(session))?
            .flush()?;
        Ok(EngineOutput {
            outgoing,
            extra_outgoing: Vec::new(),
            events: Vec::new(),
        })
    }

    fn millis(&self) -> u32 {
        self.clock.elapsed().as_millis() as u32
    }

    fn apply_core_events(
        &mut self,
        session: SessionId,
        events: Vec<CoreEvent>,
    ) -> Result<EngineOutput, EngineError> {
        let mut output = EngineOutput::default();
        for event in events {
            match event {
                CoreEvent::WindowCreated(info) => {
                    let key = SurfaceKey {
                        session,
                        window: info.id,
                    };
                    if matches!(info.kind, WindowKind::Popup { .. }) {
                        let WindowKind::Popup { parent } = info.kind else {
                            unreachable!()
                        };
                        self.popups.insert(
                            key,
                            PopupInfo {
                                parent,
                                position: Point {
                                    x: info.geometry.x,
                                    y: info.geometry.y,
                                },
                            },
                        );
                        continue;
                    }
                    let window = AndroidWindow {
                        key,
                        title: info.title,
                        app_id: info.app_id,
                        width: 0,
                        height: 0,
                    };
                    self.surfaces
                        .insert(key, Arc::new(Mutex::new(AndroidSurface::new())));
                    self.windows.insert(key, window.clone());
                    output.events.push(EngineEvent::WindowUpsert(window));
                }
                CoreEvent::WindowMetadata {
                    id,
                    title,
                    app_id,
                    geometry,
                } => {
                    let key = SurfaceKey {
                        session,
                        window: id,
                    };
                    if let Some(popup) = self.popups.get_mut(&key) {
                        popup.position = Point {
                            x: geometry.x,
                            y: geometry.y,
                        };
                    }
                    if let Some(window) = self.windows.get_mut(&key) {
                        window.title = title;
                        window.app_id = app_id;
                        output
                            .events
                            .push(EngineEvent::WindowUpsert(window.clone()));
                    }
                }
                CoreEvent::WindowClosed { id } => {
                    let key = SurfaceKey {
                        session,
                        window: id,
                    };
                    if let Some(popup) = self.popups.remove(&key) {
                        if let Some(surface) = self.surface(SurfaceKey {
                            session,
                            window: popup.parent,
                        }) {
                            let mut surface = surface
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            surface.canvas.remove_layer(id);
                            surface.version += 1;
                            let version = surface.version;
                            surface.signal.mark_dirty(version);
                        }
                    }
                    if self.windows.remove(&key).is_some() {
                        self.surfaces.remove(&key);
                        output.events.push(EngineEvent::WindowRemoved { key });
                    }
                }
                CoreEvent::WindowConfigured { .. } | CoreEvent::WindowRequest { .. } => {}
                CoreEvent::PipeAvailable { .. } => {}
                CoreEvent::Frame(frame) => {
                    let key = SurfaceKey {
                        session,
                        window: frame.window_id,
                    };
                    let (target, frame) = if let Some(popup) = self.popups.get(&key) {
                        let target = SurfaceKey {
                            session,
                            window: popup.parent,
                        };
                        let mut frame = frame;
                        frame.window_id = popup.parent;
                        frame.parent = Some((frame.surface_id, popup.position));
                        (target, frame)
                    } else {
                        (key, frame)
                    };
                    self.apply_frame(target, &frame)?;
                    let size = self
                        .surface(target)
                        .unwrap()
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .canvas
                        .size();
                    if let Some(window) = self.windows.get_mut(&target) {
                        if window.width != size.width || window.height != size.height {
                            window.width = size.width;
                            window.height = size.height;
                            output
                                .events
                                .push(EngineEvent::WindowUpsert(window.clone()));
                        }
                    }
                }
                CoreEvent::ClipboardChanged { source, mime_types } => {
                    if let Some(_source_id) = source {
                        let connection = self
                            .connections
                            .get_mut(&session)
                            .ok_or(EngineError::UnknownSession(session))?;
                        let outgoing = connection.request_clipboard_text()?;
                        let mut bytes = BytesMut::from(output.outgoing.as_ref());
                        bytes.extend_from_slice(&outgoing);
                        output.outgoing = bytes.freeze();
                    } else if mime_types.is_empty() {
                        output.events.push(EngineEvent::ClipboardText {
                            session,
                            text: String::new(),
                        });
                    }
                }
                CoreEvent::ClipboardData { data, .. } => {
                    let text = String::from_utf8_lossy(&data).into_owned();
                    self.host_clipboard = Some(text.clone());
                    output
                        .events
                        .push(EngineEvent::ClipboardText { session, text });
                }
                CoreEvent::Closed => {}
            }
        }
        Ok(output)
    }

    fn apply_frame(&mut self, key: SurfaceKey, frame: &Frame) -> Result<(), EngineError> {
        let surface = self.surface(key).ok_or(EngineError::UnknownSurface(key))?;
        let pool = frame
            .buffer
            .as_ref()
            .and_then(|buffer| {
                self.connections
                    .get(&key.session)?
                    .client()
                    .resources()
                    .data(buffer.rid)
            })
            .map(|data| data.to_vec());
        let mut surface = surface
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        surface.canvas.apply_frame(frame, pool.as_deref())?;
        if frame.parent.is_none() {
            if let Some(buffer) = &frame.buffer {
                surface.scale = buffer.scale.max(1);
            }
        }
        surface.version += 1;
        let version = surface.version;
        surface.frames.push_back((version, frame.id));
        surface.signal.mark_dirty(version);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use waypane_core::{BufferView, Rect, ShmFormat, WindowInfo};
    use waypipe_client::{encode_frame, MessageType, WAYPIPE_PROTOCOL_VERSION};

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

    fn window(session: SessionId, id: WindowId, kind: WindowKind) -> CoreEvent {
        CoreEvent::WindowCreated(WindowInfo {
            id,
            kind,
            title: format!("s{session}"),
            app_id: "test".into(),
            geometry: Rect {
                x: 1,
                y: 2,
                width: 1,
                height: 1,
            },
        })
    }

    fn frame(window: WindowId, surface: WindowId, rid: waypipe_client::Rid) -> CoreEvent {
        CoreEvent::Frame(Frame {
            id: surface as u64,
            window_id: window,
            surface_id: surface,
            parent: None,
            buffer: Some(BufferView {
                rid,
                offset: 0,
                width: 1,
                height: 1,
                stride: 4,
                format: ShmFormat::Xrgb8888,
                scale: 2,
                transform: 0,
                damage: vec![Rect {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                }],
            }),
            z_order: None,
        })
    }

    fn open_pool(
        engine: &mut AndroidEngine,
        session: SessionId,
        rid: waypipe_client::Rid,
        pixel: [u8; 4],
    ) {
        let mut open = Vec::new();
        open.extend_from_slice(&rid.0.to_le_bytes());
        open.extend_from_slice(&4i32.to_le_bytes());
        let mut bytes = encode_frame(MessageType::OpenFile, &open).unwrap().to_vec();
        let mut fill = Vec::new();
        fill.extend_from_slice(&rid.0.to_le_bytes());
        fill.extend_from_slice(&0u32.to_le_bytes());
        fill.extend_from_slice(&4u32.to_le_bytes());
        fill.extend_from_slice(&pixel);
        bytes.extend_from_slice(&encode_frame(MessageType::BufferFill, &fill).unwrap());
        engine.receive(session, &bytes).unwrap();
    }

    #[test]
    fn isolates_identical_window_ids_across_sessions() {
        let mut engine = AndroidEngine::new(Compression::None);
        for session in [1, 2] {
            engine.connected(session);
            engine.receive(session, &header()).unwrap();
            engine
                .apply_core_events(session, vec![window(session, 10, WindowKind::Toplevel)])
                .unwrap();
            let pixel = if session == 1 {
                [0, 0, 255, 0]
            } else {
                [0, 255, 0, 0]
            };
            open_pool(
                &mut engine,
                session,
                waypipe_client::Rid(session as i32),
                pixel,
            );
            engine
                .apply_core_events(
                    session,
                    vec![frame(10, 20, waypipe_client::Rid(session as i32))],
                )
                .unwrap();
        }
        let red = engine
            .surface(SurfaceKey {
                session: 1,
                window: 10,
            })
            .unwrap();
        let green = engine
            .surface(SurfaceKey {
                session: 2,
                window: 10,
            })
            .unwrap();
        assert_eq!(&red.lock().unwrap().canvas.rgba()[..4], &[255, 0, 0, 255]);
        assert_eq!(&green.lock().unwrap().canvas.rgba()[..4], &[0, 255, 0, 255]);
        assert_eq!(
            engine
                .logical_point(
                    SurfaceKey {
                        session: 1,
                        window: 10
                    },
                    1.0,
                    0.5
                )
                .unwrap(),
            (0.5, 0.25)
        );

        engine.disconnect(1, false).unwrap();
        assert!(engine.windows.len() == 1);
        assert!(engine
            .surface(SurfaceKey {
                session: 2,
                window: 10
            })
            .is_some());
    }

    #[cfg(feature = "jni-check")]
    #[test]
    fn serializes_engine_events_in_kotlin_schema() {
        let window = AndroidWindow {
            key: SurfaceKey {
                session: 1,
                window: 2,
            },
            title: "Terminal".into(),
            app_id: "org.waypipe.foot".into(),
            width: 800,
            height: 600,
        };
        assert_eq!(
            serde_json::to_string(&EngineEvent::WindowUpsert(window)).unwrap(),
            r#"{"type":"window_upsert","key":{"session":1,"window":2},"title":"Terminal","app_id":"org.waypipe.foot","width":800,"height":600}"#
        );
    }

    #[test]
    fn composites_popup_into_its_parent_surface() {
        let mut engine = AndroidEngine::new(Compression::None);
        engine.connected(1);
        engine.receive(1, &header()).unwrap();
        engine
            .apply_core_events(1, vec![window(1, 10, WindowKind::Toplevel)])
            .unwrap();
        open_pool(&mut engine, 1, waypipe_client::Rid(1), [0, 0, 0, 0]);
        engine
            .apply_core_events(1, vec![frame(10, 11, waypipe_client::Rid(1))])
            .unwrap();
        engine
            .apply_core_events(
                1,
                vec![
                    window(1, 20, WindowKind::Popup { parent: 10 }),
                    frame(20, 21, waypipe_client::Rid(1)),
                ],
            )
            .unwrap();
        assert_eq!(engine.windows.len(), 1);
        let surface = engine
            .surface(SurfaceKey {
                session: 1,
                window: 10,
            })
            .unwrap();
        assert_eq!(surface.lock().unwrap().version, 2);
    }
}
