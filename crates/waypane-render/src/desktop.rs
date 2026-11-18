use std::collections::HashMap;
use std::sync::Arc;

use waypane_core::{Frame, WindowId, WindowInfo, WindowKind};
use waypipe_client::ResourceStore;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event_loop::ActiveEventLoop;
use winit::window::{Window, WindowLevel};

use crate::image::{output_size, Canvas, ImageError};

#[derive(Debug, thiserror::Error)]
pub enum DesktopError {
    #[error("window creation failed: {0}")]
    Window(#[from] winit::error::OsError),
    #[error("GPU initialization failed: {0}")]
    Gpu(String),
    #[error(transparent)]
    Image(#[from] ImageError),
    #[error("frame references unknown window {0}")]
    UnknownWindow(WindowId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresentedFrame {
    pub session: u64,
    pub frame_id: u64,
}

struct NativeWindow {
    session: u64,
    core_id: WindowId,
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    blitter: wgpu::util::TextureBlitter,
    texture: Option<wgpu::Texture>,
    texture_size: waypane_core::Size,
    canvas: Canvas,
}

impl NativeWindow {
    fn new(
        session: u64,
        core_id: WindowId,
        window: Arc<Window>,
        instance: &wgpu::Instance,
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
    ) -> Result<Self, DesktopError> {
        let size = window.inner_size();
        let surface = instance
            .create_surface(window.clone())
            .map_err(|error| DesktopError::Gpu(error.to_string()))?;
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            color_space: wgpu::SurfaceColorSpace::Auto,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::AutoVsync,
            desired_maximum_frame_latency: 2,
            alpha_mode: wgpu::CompositeAlphaMode::Auto,
            view_formats: Vec::new(),
        };
        surface.configure(device, &config);
        let blitter = wgpu::util::TextureBlitter::new(device, format);
        Ok(Self {
            session,
            core_id,
            window,
            surface,
            config,
            blitter,
            texture: None,
            texture_size: waypane_core::Size::default(),
            canvas: Canvas::default(),
        })
    }

    fn upload(
        &mut self,
        frame: &Frame,
        store: &ResourceStore,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
    ) -> Result<(), DesktopError> {
        let pool = frame
            .buffer
            .as_ref()
            .and_then(|buffer| store.data(buffer.rid));
        let uploads = self.canvas.apply_frame(frame, pool)?;
        let size = self.canvas.size();
        if size.width > 0 && size.height > 0 && self.texture_size != size {
            self.texture = Some(device.create_texture(&wgpu::TextureDescriptor {
                label: Some("waypane-surface"),
                size: wgpu::Extent3d {
                    width: size.width,
                    height: size.height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            }));
            self.texture_size = size;
        }
        let Some(texture) = &self.texture else {
            return Ok(());
        };
        for upload in uploads {
            let rect = upload.rect;
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d {
                        x: rect.x as u32,
                        y: rect.y as u32,
                        z: 0,
                    },
                    aspect: wgpu::TextureAspect::All,
                },
                &upload.rgba,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(rect.width as u32 * 4),
                    rows_per_image: Some(rect.height as u32),
                },
                wgpu::Extent3d {
                    width: rect.width as u32,
                    height: rect.height as u32,
                    depth_or_array_layers: 1,
                },
            );
        }
        self.window.request_redraw();
        log::trace!("queued redraw for remote frame {}", frame.id);
        Ok(())
    }

    fn render(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) -> bool {
        let Some(texture) = &self.texture else {
            log::trace!("skipping redraw before the first texture upload");
            return false;
        };
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame)
            | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
            wgpu::CurrentSurfaceTexture::Outdated => {
                self.surface.configure(device, &self.config);
                self.window.request_redraw();
                return false;
            }
            status => {
                log::debug!("surface texture unavailable: {status:?}");
                return false;
            }
        };
        let source = texture.create_view(&Default::default());
        let target = frame.texture.create_view(&Default::default());
        let mut encoder = device.create_command_encoder(&Default::default());
        self.blitter.copy(device, &mut encoder, &source, &target);
        queue.submit([encoder.finish()]);
        queue.present(frame);
        log::trace!("presented native frame");
        true
    }

    fn resize(&mut self, width: u32, height: u32, device: &wgpu::Device) {
        if width == 0 || height == 0 {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(device, &self.config);
        self.window.request_redraw();
    }
}

#[derive(Default)]
pub struct DesktopRenderer {
    instance: Option<wgpu::Instance>,
    device: Option<wgpu::Device>,
    queue: Option<wgpu::Queue>,
    format: Option<wgpu::TextureFormat>,
    windows: HashMap<winit::window::WindowId, NativeWindow>,
    ids: HashMap<(u64, WindowId), winit::window::WindowId>,
    pending_windows: HashMap<(u64, WindowId), (u64, WindowInfo)>,
    pending_frames: Vec<(winit::window::WindowId, PresentedFrame)>,
    presented: Vec<PresentedFrame>,
}

impl DesktopRenderer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn window_created(&mut self, session: u64, info: WindowInfo) {
        self.pending_windows
            .insert((session, info.id), (session, info));
    }

    pub fn window_metadata(
        &mut self,
        session: u64,
        id: WindowId,
        title: &str,
        app_id: &str,
        geometry: waypane_core::Rect,
    ) {
        if let Some(native_id) = self.ids.get(&(session, id)) {
            if let Some(window) = self.windows.get_mut(native_id) {
                window.window.set_title(&window_title(title, app_id));
            }
        }
        if let Some((_, info)) = self.pending_windows.get_mut(&(session, id)) {
            info.title = title.to_owned();
            info.app_id = app_id.to_owned();
            info.geometry = geometry;
        }
    }

    pub fn apply_frame(
        &mut self,
        event_loop: &ActiveEventLoop,
        session: u64,
        frame: &Frame,
        store: &ResourceStore,
    ) -> Result<(), DesktopError> {
        let native_id = if let Some(id) = self.ids.get(&(session, frame.window_id)) {
            *id
        } else {
            let (info_session, info) = self
                .pending_windows
                .remove(&(session, frame.window_id))
                .ok_or(DesktopError::UnknownWindow(frame.window_id))?;
            let window_size = frame
                .buffer
                .as_ref()
                .map(|buffer| {
                    let size =
                        output_size(buffer.width as u32, buffer.height as u32, buffer.transform);
                    waypane_core::Size {
                        width: (size.width as i32 / buffer.scale).max(1) as u32,
                        height: (size.height as i32 / buffer.scale).max(1) as u32,
                    }
                })
                .unwrap_or(waypane_core::Size {
                    width: 800,
                    height: 600,
                });
            let attributes = window_attributes(
                &info,
                window_size,
                self.native_position(session, info.kind, info.geometry),
            );
            let window = Arc::new(event_loop.create_window(attributes)?);
            self.ensure_gpu(&window)?;
            let id = window.id();
            let native = NativeWindow::new(
                info_session,
                info.id,
                window,
                self.instance.as_ref().unwrap(),
                self.device.as_ref().unwrap(),
                self.format.unwrap(),
            )?;
            self.windows.insert(id, native);
            self.ids.insert((info_session, info.id), id);
            id
        };

        let window = self.windows.get_mut(&native_id).unwrap();
        window.upload(
            frame,
            store,
            self.device.as_ref().unwrap(),
            self.queue.as_ref().unwrap(),
        )?;
        self.pending_frames.push((
            native_id,
            PresentedFrame {
                session,
                frame_id: frame.id,
            },
        ));
        Ok(())
    }

    pub fn render(&mut self, id: winit::window::WindowId) {
        let Some(device) = self.device.as_ref() else {
            return;
        };
        let Some(queue) = self.queue.as_ref() else {
            return;
        };
        if self
            .windows
            .get_mut(&id)
            .is_some_and(|window| window.render(device, queue))
        {
            let (ready, pending): (Vec<_>, Vec<_>) = self
                .pending_frames
                .drain(..)
                .partition(|(native_id, _)| *native_id == id);
            self.pending_frames = pending;
            self.presented
                .extend(ready.into_iter().map(|(_, frame)| frame));
        }
    }

    pub fn take_presented(&mut self) -> Vec<PresentedFrame> {
        std::mem::take(&mut self.presented)
    }

    pub fn resize(
        &mut self,
        id: winit::window::WindowId,
        width: u32,
        height: u32,
    ) -> Option<(u64, WindowId, waypane_core::Size)> {
        let window = self.windows.get_mut(&id)?;
        window.resize(width, height, self.device.as_ref()?);
        let scale = window.window.scale_factor();
        Some((
            window.session,
            window.core_id,
            waypane_core::Size {
                width: ((f64::from(width) / scale).round() as u32).max(1),
                height: ((f64::from(height) / scale).round() as u32).max(1),
            },
        ))
    }

    pub fn core_window(&self, id: winit::window::WindowId) -> Option<(u64, WindowId)> {
        self.windows
            .get(&id)
            .map(|window| (window.session, window.core_id))
    }

    pub fn close_core_window(&mut self, session: u64, id: WindowId) {
        self.pending_windows.remove(&(session, id));
        if let Some(native_id) = self.ids.remove(&(session, id)) {
            self.windows.remove(&native_id);
            self.pending_frames
                .retain(|(window, _)| *window != native_id);
        }
    }

    pub fn close_session(&mut self, session: u64) {
        let ids = self
            .windows
            .values()
            .filter(|window| window.session == session)
            .map(|window| window.core_id)
            .collect::<Vec<_>>();
        for id in ids {
            self.close_core_window(session, id);
        }
        self.presented.retain(|frame| frame.session != session);
    }

    pub fn scale_factor(&self, id: winit::window::WindowId) -> Option<f64> {
        self.windows
            .get(&id)
            .map(|window| window.window.scale_factor())
    }

    pub fn window_request(
        &mut self,
        session: u64,
        id: WindowId,
        request: waypane_core::WindowRequest,
    ) -> Option<()> {
        let native_id = self.ids.get(&(session, id))?;
        let window = &self.windows.get(native_id)?.window;
        match request {
            waypane_core::WindowRequest::Maximized => window.set_maximized(true),
            waypane_core::WindowRequest::Unmaximized => window.set_maximized(false),
            waypane_core::WindowRequest::Fullscreen(_) => {
                window.set_fullscreen(Some(winit::window::Fullscreen::Borderless(None)));
            }
            waypane_core::WindowRequest::Unfullscreen => window.set_fullscreen(None),
            waypane_core::WindowRequest::Minimized => window.set_minimized(true),
            waypane_core::WindowRequest::Move => {
                let _ = window.drag_window();
            }
            waypane_core::WindowRequest::Resize { edges } => {
                let direction = match edges {
                    1 => Some(winit::window::ResizeDirection::North),
                    2 => Some(winit::window::ResizeDirection::South),
                    4 => Some(winit::window::ResizeDirection::West),
                    5 => Some(winit::window::ResizeDirection::NorthWest),
                    6 => Some(winit::window::ResizeDirection::SouthWest),
                    8 => Some(winit::window::ResizeDirection::East),
                    9 => Some(winit::window::ResizeDirection::NorthEast),
                    10 => Some(winit::window::ResizeDirection::SouthEast),
                    _ => None,
                };
                if let Some(direction) = direction {
                    let _ = window.drag_resize_window(direction);
                }
            }
        }
        Some(())
    }

    pub fn window_count(&self) -> usize {
        self.windows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.windows.is_empty() && self.pending_windows.is_empty()
    }

    fn native_position(
        &self,
        session: u64,
        kind: WindowKind,
        geometry: waypane_core::Rect,
    ) -> Option<PhysicalPosition<i32>> {
        let WindowKind::Popup { parent } = kind else {
            return None;
        };
        let parent_id = self.ids.get(&(session, parent))?;
        let parent = self.windows.get(parent_id)?;
        let position = parent.window.outer_position().unwrap_or_default();
        Some(PhysicalPosition::new(
            position.x + geometry.x,
            position.y + geometry.y,
        ))
    }

    fn ensure_gpu(&mut self, window: &Arc<Window>) -> Result<(), DesktopError> {
        if self.device.is_some() {
            return Ok(());
        }
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let surface = instance
            .create_surface(window.clone())
            .map_err(|error| DesktopError::Gpu(error.to_string()))?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            compatible_surface: Some(&surface),
            ..Default::default()
        }))
        .map_err(|error| DesktopError::Gpu(error.to_string()))?;
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
                .map_err(|error| DesktopError::Gpu(error.to_string()))?;
        let capabilities = surface.get_capabilities(&adapter);
        let format = *capabilities
            .formats
            .first()
            .ok_or_else(|| DesktopError::Gpu("surface reports no supported formats".to_string()))?;
        log::info!(
            "GPU adapter: {} ({:?})",
            adapter.get_info().name,
            adapter.get_info().backend
        );
        self.instance = Some(instance);
        self.device = Some(device);
        self.queue = Some(queue);
        self.format = Some(format);
        Ok(())
    }
}

fn window_title(title: &str, app_id: &str) -> String {
    if !title.is_empty() {
        title.to_owned()
    } else if !app_id.is_empty() {
        format!("{app_id} — Waypane")
    } else {
        "Waypane".to_owned()
    }
}

fn window_attributes(
    info: &WindowInfo,
    size: waypane_core::Size,
    position: Option<PhysicalPosition<i32>>,
) -> winit::window::WindowAttributes {
    let popup = matches!(info.kind, WindowKind::Popup { .. });
    let attributes = Window::default_attributes()
        .with_title(window_title(&info.title, &info.app_id))
        .with_inner_size(LogicalSize::new(
            f64::from(size.width.max(1)),
            f64::from(size.height.max(1)),
        ))
        .with_decorations(!popup)
        .with_resizable(!popup);
    let attributes = if popup {
        attributes.with_window_level(WindowLevel::AlwaysOnTop)
    } else {
        attributes
    };
    match position {
        Some(position) => attributes.with_position(position),
        None => attributes,
    }
}
