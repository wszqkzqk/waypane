use std::collections::HashMap;
use std::sync::Arc;

use waypane_core::{Frame, WindowId, WindowInfo, WindowKind};
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event_loop::ActiveEventLoop;
use winit::window::{Window, WindowLevel};

use crate::image::{output_size, Canvas, ImageError};
use crate::present::{canvas_texture_format, preferred_surface_format};

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
    texture_format: wgpu::TextureFormat,
    canvas: Canvas,
    buffer_scale: i32,
    straight_alpha: bool,
}

impl NativeWindow {
    fn new(
        session: u64,
        core_id: WindowId,
        window: Arc<Window>,
        instance: &wgpu::Instance,
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        alpha_mode: wgpu::CompositeAlphaMode,
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
            alpha_mode,
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
            texture_format: canvas_texture_format(format),
            canvas: Canvas::default(),
            buffer_scale: 1,
            straight_alpha: alpha_mode == wgpu::CompositeAlphaMode::PostMultiplied,
        })
    }

    fn upload(
        &mut self,
        frame: &Frame,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
    ) -> Result<(), DesktopError> {
        if let Some(buffer) = &frame.buffer {
            self.buffer_scale = buffer.scale.max(1);
        }
        let uploads = self.canvas.apply_frame(frame)?;
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
                format: self.texture_format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            }));
            self.texture_size = size;
        }
        let Some(texture) = &self.texture else {
            return Ok(());
        };
        for mut upload in uploads {
            let rect = upload.rect;
            if self.straight_alpha {
                for pixel in upload.rgba.as_chunks_mut::<4>().0 {
                    *pixel = to_straight_rgba(*pixel);
                }
            }
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
    alpha_mode: Option<wgpu::CompositeAlphaMode>,
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
    ) -> Result<Option<f64>, DesktopError> {
        let mut created_scale_factor = None;
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
            created_scale_factor = Some(window.scale_factor());
            let native = NativeWindow::new(
                info_session,
                info.id,
                window,
                self.instance.as_ref().unwrap(),
                self.device.as_ref().unwrap(),
                self.format.unwrap(),
                self.alpha_mode.unwrap_or(wgpu::CompositeAlphaMode::Auto),
            )?;
            self.windows.insert(id, native);
            self.ids.insert((info_session, info.id), id);
            id
        };

        let window = self.windows.get_mut(&native_id).unwrap();
        window.upload(
            frame,
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
        Ok(created_scale_factor)
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

    /// Maps physical mouse coordinates onto the committed Wayland surface. The buffer's scale
    /// and the actual drawable size matter, not only the host scale factor: remote surfaces can
    /// use scale 1 on a Retina display or differ in size from the native window.
    pub fn surface_point(
        &self,
        id: winit::window::WindowId,
        position: PhysicalPosition<f64>,
    ) -> Option<(f64, f64)> {
        let native = self.windows.get(&id)?;
        surface_point(
            native.canvas.size(),
            native.buffer_scale,
            native.window.inner_size(),
            position,
        )
    }

    pub fn request_redraw(&self, id: winit::window::WindowId) {
        if let Some(window) = self.windows.get(&id) {
            window.window.request_redraw();
        }
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
        let format = preferred_surface_format(&capabilities.formats)
            .ok_or_else(|| DesktopError::Gpu("surface reports no supported formats".to_string()))?;
        let alpha_mode = surface_alpha_mode(&capabilities.alpha_modes);
        log::info!(
            "GPU adapter: {} ({:?}, alpha {:?}, surface {:?}, canvas {:?})",
            adapter.get_info().name,
            adapter.get_info().backend,
            alpha_mode,
            format,
            canvas_texture_format(format)
        );
        self.alpha_mode = Some(alpha_mode);
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
        .with_transparent(true)
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

/// Transparency requires a non-opaque surface. Metal advertises only Opaque/PostMultiplied and
/// only makes its layer non-opaque for PostMultiplied; elsewhere premultiplied may be offered.
/// With Auto (neither supported) shadows still show the opaque background.
fn surface_alpha_mode(supported: &[wgpu::CompositeAlphaMode]) -> wgpu::CompositeAlphaMode {
    if supported.contains(&wgpu::CompositeAlphaMode::PostMultiplied) {
        wgpu::CompositeAlphaMode::PostMultiplied
    } else if supported.contains(&wgpu::CompositeAlphaMode::PreMultiplied) {
        wgpu::CompositeAlphaMode::PreMultiplied
    } else {
        wgpu::CompositeAlphaMode::Auto
    }
}

/// The canvas composites premultiplied rgb; PostMultiplied surfaces need straight rgb.
fn to_straight_rgba([r, g, b, a]: [u8; 4]) -> [u8; 4] {
    if a == 0 {
        return [0; 4];
    }
    if a == 255 {
        return [r, g, b, a];
    }
    let straight =
        |channel: u8| ((u32::from(channel) * 255 + u32::from(a) / 2) / u32::from(a)).min(255) as u8;
    [straight(r), straight(g), straight(b), a]
}

fn surface_point(
    pixels: waypane_core::Size,
    buffer_scale: i32,
    drawable: winit::dpi::PhysicalSize<u32>,
    position: PhysicalPosition<f64>,
) -> Option<(f64, f64)> {
    if drawable.width == 0 || drawable.height == 0 {
        return None;
    }
    let scale = f64::from(buffer_scale.max(1));
    let logical = waypane_core::Size {
        width: (f64::from(pixels.width) / scale).round().max(1.0) as u32,
        height: (f64::from(pixels.height) / scale).round().max(1.0) as u32,
    };
    Some((
        position.x * f64::from(logical.width) / f64::from(drawable.width),
        position.y * f64::from(logical.height) / f64::from(drawable.height),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::dpi::PhysicalSize;

    fn size(width: u32, height: u32) -> waypane_core::Size {
        waypane_core::Size { width, height }
    }

    #[test]
    fn selects_a_surface_alpha_mode_supported_by_the_adapter() {
        use wgpu::CompositeAlphaMode::*;
        assert_eq!(
            surface_alpha_mode(&[Opaque, PostMultiplied]),
            PostMultiplied
        );
        assert_eq!(surface_alpha_mode(&[Opaque, PreMultiplied]), PreMultiplied);
        assert_eq!(surface_alpha_mode(&[Opaque]), Auto);
    }

    #[test]
    fn presents_srgb_canvas_values_unchanged_on_every_render_target() {
        use std::sync::mpsc;

        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            compatible_surface: None,
            ..Default::default()
        }));
        let adapter = match adapter {
            Ok(adapter) if adapter.get_info().device_type != wgpu::DeviceType::Cpu => adapter,
            result => {
                eprintln!("no hardware GPU adapter available: {result:?}");
                return;
            }
        };
        let Ok((device, queue)) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
        else {
            eprintln!("GPU device unavailable");
            return;
        };
        let input: Vec<[u8; 4]> = (0u8..=255)
            .flat_map(|v| [[v, v, v, 255], [v, 255 - v, v ^ 0x55, 255]])
            .collect();
        let width = input.len() as u32;
        let rgba: Vec<u8> = input.iter().flatten().copied().collect();
        let extent = wgpu::Extent3d {
            width,
            height: 1,
            depth_or_array_layers: 1,
        };
        let pitch = (width * 4).div_ceil(256) * 256;
        let mut tested = 0;
        for target_format in [
            wgpu::TextureFormat::Bgra8UnormSrgb,
            wgpu::TextureFormat::Bgra8Unorm,
            wgpu::TextureFormat::Rgba8UnormSrgb,
            wgpu::TextureFormat::Rgba8Unorm,
        ] {
            let bgra = matches!(
                target_format,
                wgpu::TextureFormat::Bgra8UnormSrgb | wgpu::TextureFormat::Bgra8Unorm
            );
            let texture = |label, format, usage| {
                device.create_texture(&wgpu::TextureDescriptor {
                    label: Some(label),
                    size: extent,
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage,
                    view_formats: &[],
                })
            };
            let source = texture(
                "waypane-surface",
                canvas_texture_format(target_format),
                wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            );
            queue.write_texture(
                source.as_image_copy(),
                &rgba,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(width * 4),
                    rows_per_image: Some(1),
                },
                extent,
            );
            let target = texture(
                "waypane-target",
                target_format,
                wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            );
            let readback = device.create_buffer(&wgpu::BufferDescriptor {
                label: None,
                size: u64::from(pitch),
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            let blitter = wgpu::util::TextureBlitter::new(&device, target_format);
            let mut encoder = device.create_command_encoder(&Default::default());
            blitter.copy(
                &device,
                &mut encoder,
                &source.create_view(&Default::default()),
                &target.create_view(&Default::default()),
            );
            encoder.copy_texture_to_buffer(
                target.as_image_copy(),
                wgpu::TexelCopyBufferInfo {
                    buffer: &readback,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(pitch),
                        rows_per_image: Some(1),
                    },
                },
                extent,
            );
            queue.submit([encoder.finish()]);
            let slice = readback.slice(..);
            let (tx, rx) = mpsc::channel();
            slice.map_async(wgpu::MapMode::Read, move |result| {
                let _ = tx.send(result);
            });
            let _ = device.poll(wgpu::PollType::wait_indefinitely());
            rx.recv().unwrap().unwrap();
            let bytes = slice.get_mapped_range().unwrap();
            for (index, (expected, actual)) in input
                .iter()
                .zip(bytes[..(width * 4) as usize].as_chunks::<4>().0)
                .enumerate()
            {
                let actual = if bgra {
                    [actual[2], actual[1], actual[0], actual[3]]
                } else {
                    [actual[0], actual[1], actual[2], actual[3]]
                };
                assert!(
                    expected
                        .iter()
                        .zip(actual)
                        .all(|(expected, actual)| expected.abs_diff(actual) <= 1),
                    "{target_format:?} changed sRGB pixel {index}: {expected:?} -> {actual:?}"
                );
            }
            tested += 1;
        }
        assert_eq!(tested, 4);
    }

    #[test]
    fn converts_premultiplied_rgba_for_straight_alpha_surfaces() {
        assert_eq!(to_straight_rgba([64, 0, 32, 128]), [128, 0, 64, 128]);
        assert_eq!(to_straight_rgba([12, 34, 56, 0]), [0, 0, 0, 0]);
        assert_eq!(to_straight_rgba([12, 34, 56, 255]), [12, 34, 56, 255]);
        assert_eq!(to_straight_rgba([255, 200, 100, 128]), [255, 255, 199, 128]);
    }

    #[test]
    fn pointer_uses_buffer_scale_and_actual_drawable_size() {
        let drawable = PhysicalSize::new(2200, 1474);
        assert_eq!(
            surface_point(
                size(1100, 737),
                1,
                drawable,
                PhysicalPosition::new(2200.0, 1474.0)
            ),
            Some((1100.0, 737.0))
        );
        assert_eq!(
            surface_point(
                size(2200, 1474),
                2,
                drawable,
                PhysicalPosition::new(1100.0, 737.0)
            ),
            Some((550.0, 368.5))
        );
        assert_eq!(
            surface_point(
                size(1100, 737),
                1,
                PhysicalSize::new(0, 0),
                PhysicalPosition::new(1.0, 1.0)
            ),
            None
        );
    }
}
