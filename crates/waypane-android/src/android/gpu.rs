use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::ptr::NonNull;
use std::sync::Arc;

use jni::objects::JObject;
use jni::refs::Global;
use jni::Env;
use raw_window_handle::{AndroidDisplayHandle, AndroidNdkWindowHandle};
use waypane_core::Size;
use waypane_render::ViewportTransform;

use crate::{ActorHandle, AndroidSurface, SurfaceKey};

#[cfg(target_os = "android")]
#[link(name = "android")]
unsafe extern "C" {
    fn ANativeWindow_fromSurface(
        env: *mut jni::sys::JNIEnv,
        surface: jni::sys::jobject,
    ) -> *mut c_void;
    fn ANativeWindow_release(window: *mut c_void);
}

#[cfg(not(target_os = "android"))]
#[no_mangle]
pub unsafe extern "C" fn ANativeWindow_fromSurface(
    _env: *mut jni::sys::JNIEnv,
    _surface: jni::sys::jobject,
) -> *mut c_void {
    std::ptr::null_mut()
}

#[cfg(not(target_os = "android"))]
#[no_mangle]
pub unsafe extern "C" fn ANativeWindow_release(_window: *mut c_void) {}

#[derive(Debug)]
struct NativeWindowRef(NonNull<c_void>);

// SAFETY: ANativeWindow_fromSurface returns a retained, reference-counted ANativeWindow whose
// NDK operations may be called from any thread; the retained reference is released exactly once.
unsafe impl Send for NativeWindowRef {}
// SAFETY: Shared references never mutate the pointer itself; ANativeWindow's reference counting and
// underlying buffer-queue operations are thread-safe and only touch it while the reference is alive.
unsafe impl Sync for NativeWindowRef {}

impl Drop for NativeWindowRef {
    fn drop(&mut self) {
        unsafe { ANativeWindow_release(self.0.as_ptr()) };
    }
}

struct ViewportBlitter {
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    vertices: wgpu::Buffer,
}

impl ViewportBlitter {
    fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let vertices = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("waypane-viewport-vertices"),
            size: std::mem::size_of::<[[f32; 4]; 6]>() as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("waypane-viewport-sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("waypane-viewport-bind-group-layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("waypane-viewport-pipeline-layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });
        let shader = device.create_shader_module(wgpu::include_wgsl!("viewport.wgsl"));
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("waypane-viewport-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<[f32; 4]>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2],
                })],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        });
        Self {
            pipeline,
            bind_group_layout,
            sampler,
            vertices,
        }
    }

    fn draw(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        source: &wgpu::TextureView,
        target: &wgpu::TextureView,
        transform: &ViewportTransform,
    ) {
        queue.write_buffer(
            &self.vertices,
            0,
            bytemuck::cast_slice(&transform.vertices()),
        );
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("waypane-viewport-bind-group"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(source),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("waypane-viewport-pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: 0.0,
                        g: 0.0,
                        b: 0.0,
                        a: 1.0,
                    }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &bind_group, &[]);
        pass.set_vertex_buffer(0, self.vertices.slice(..));
        pass.draw(0..6, 0..1);
    }
}

const PREFERRED_SURFACE_FORMATS: &[wgpu::TextureFormat] = &[
    wgpu::TextureFormat::Rgba8UnormSrgb,
    wgpu::TextureFormat::Bgra8UnormSrgb,
    wgpu::TextureFormat::Rgba8Unorm,
    wgpu::TextureFormat::Bgra8Unorm,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SurfacePlan {
    format: wgpu::TextureFormat,
    alpha_mode: wgpu::CompositeAlphaMode,
    present_mode: wgpu::PresentMode,
    color_space: wgpu::SurfaceColorSpace,
}

impl SurfacePlan {
    fn negotiate(capabilities: &wgpu::SurfaceCapabilities) -> Result<Self, String> {
        let format = PREFERRED_SURFACE_FORMATS
            .iter()
            .copied()
            .find(|format| capabilities.formats.contains(format))
            .ok_or_else(|| {
                format!(
                    "Android surface offers no Rgba8/Bgra8 formats; advertised formats: {:?}",
                    capabilities.formats
                )
            })?;
        if !capabilities
            .usages
            .contains(wgpu::TextureUsages::RENDER_ATTACHMENT)
        {
            return Err(format!(
                "Android surface usage {:?} does not include RENDER_ATTACHMENT",
                capabilities.usages
            ));
        }
        if capabilities.alpha_modes.is_empty() {
            return Err("Android surface reports no supported alpha modes".to_string());
        }
        if capabilities.present_modes.is_empty() {
            return Err("Android surface reports no supported present modes".to_string());
        }
        Ok(Self {
            format,
            alpha_mode: wgpu::CompositeAlphaMode::Auto,
            present_mode: wgpu::PresentMode::AutoVsync,
            color_space: wgpu::SurfaceColorSpace::Auto,
        })
    }

    fn configuration(self, width: u32, height: u32) -> wgpu::SurfaceConfiguration {
        wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: self.format,
            width: width.max(1),
            height: height.max(1),
            present_mode: self.present_mode,
            desired_maximum_frame_latency: 2,
            alpha_mode: self.alpha_mode,
            view_formats: Vec::new(),
            color_space: self.color_space,
        }
    }
}

struct NativeSurface {
    texture: Option<wgpu::Texture>,
    texture_size: Size,
    texture_version: Option<u64>,
    blitter: Option<ViewportBlitter>,
    config: Option<wgpu::SurfaceConfiguration>,
    surface: ManuallyDrop<wgpu::Surface<'static>>,
    _window: NativeWindowRef,
    _java_surface: Global<JObject<'static>>,
}

impl NativeSurface {
    fn new(
        env: &mut Env<'_>,
        java_surface: &JObject<'_>,
        instance: &wgpu::Instance,
    ) -> Result<Self, String> {
        let window = unsafe { ANativeWindow_fromSurface(env.get_raw(), java_surface.as_raw()) };
        let window = NativeWindowRef(
            NonNull::new(window)
                .ok_or_else(|| "ANativeWindow_fromSurface returned null".to_string())?,
        );
        let target = wgpu::SurfaceTargetUnsafe::RawHandle {
            raw_display_handle: Some(AndroidDisplayHandle::new().into()),
            raw_window_handle: AndroidNdkWindowHandle::new(window.0).into(),
        };
        let surface = unsafe { instance.create_surface_unsafe(target) }
            .map_err(|error| format!("failed to create Android GPU surface: {error}"))?;
        let java_surface = env
            .new_global_ref(java_surface)
            .map_err(|error| format!("failed to retain Java Surface: {error}"))?;
        Ok(Self {
            texture: None,
            texture_size: Size::default(),
            texture_version: None,
            blitter: None,
            config: None,
            surface: ManuallyDrop::new(surface),
            _window: window,
            _java_surface: java_surface,
        })
    }

    fn configure(
        &mut self,
        device: &wgpu::Device,
        adapter: &wgpu::Adapter,
        width: u32,
        height: u32,
    ) -> Result<(), String> {
        let capabilities = self.surface.get_capabilities(adapter);
        let plan = SurfacePlan::negotiate(&capabilities)?;
        let config = plan.configuration(width, height);
        if self.config.as_ref() == Some(&config) && self.blitter.is_some() {
            return Ok(());
        }

        log::info!(
            "Configuring Android surface {}x{} with format {:?}; advertised formats: {:?}, alpha modes: {:?}, present modes: {:?}",
            config.width,
            config.height,
            config.format,
            capabilities.formats,
            capabilities.alpha_modes,
            capabilities.present_modes
        );
        let error_scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
        self.surface.configure(device, &config);
        let blitter = ViewportBlitter::new(device, config.format);
        if let Some(error) = pollster::block_on(error_scope.pop()) {
            let _ = blitter;
            return Err(format!(
                "Surface::configure failed for format {:?}, alpha {:?}, present mode {:?}, size {}x{}: {error}",
                config.format,
                config.alpha_mode,
                config.present_mode,
                config.width,
                config.height
            ));
        }

        self.config = Some(config);
        self.blitter = Some(blitter);
        Ok(())
    }

    fn present(
        &mut self,
        state: &mut AndroidSurface,
        device: &wgpu::Device,
        adapter: &wgpu::Adapter,
        queue: &wgpu::Queue,
    ) -> Result<RenderOutcome, String> {
        if !state.signal.is_dirty() {
            return Ok(RenderOutcome::Idle);
        }
        let size = state.canvas.size();
        if size.width == 0 || size.height == 0 {
            return Ok(RenderOutcome::Idle);
        }
        if self.texture_size != size {
            self.texture = Some(device.create_texture(&wgpu::TextureDescriptor {
                label: Some("waypane-android-canvas"),
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
            self.texture_version = None;
        }
        let texture = self
            .texture
            .as_ref()
            .ok_or_else(|| "Android canvas texture was not allocated".to_string())?;
        if self.texture_version != Some(state.version) {
            queue.write_texture(
                texture.as_image_copy(),
                state.canvas.rgba(),
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(size.width * 4),
                    rows_per_image: Some(size.height),
                },
                wgpu::Extent3d {
                    width: size.width,
                    height: size.height,
                    depth_or_array_layers: 1,
                },
            );
            self.texture_version = Some(state.version);
        }

        let (width, height) = self
            .config
            .as_ref()
            .map(|config| (config.width, config.height))
            .ok_or_else(|| "Android surface has not been configured".to_string())?;
        let transform = ViewportTransform::new(size, Size { width, height }, state.viewport)
            .ok_or_else(|| {
                format!(
                    "cannot compute viewport transform for buffer {}x{} and view {}x{}",
                    size.width, size.height, width, height
                )
            })?;
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame) => frame,
            wgpu::CurrentSurfaceTexture::Suboptimal(frame) => {
                log::debug!(
                    "Android surface texture is suboptimal; presenting once before reconfiguration"
                );
                frame
            }
            wgpu::CurrentSurfaceTexture::Outdated => {
                self.configure(device, adapter, width, height)?;
                return Ok(RenderOutcome::Retry);
            }
            wgpu::CurrentSurfaceTexture::Timeout => return Ok(RenderOutcome::Retry),
            wgpu::CurrentSurfaceTexture::Occluded => return Ok(RenderOutcome::Occluded),
            wgpu::CurrentSurfaceTexture::Lost => {
                return Err("Android GPU surface was lost".to_string());
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                return Err("Surface::get_current_texture failed validation".to_string());
            }
        };
        let blitter = self
            .blitter
            .as_ref()
            .ok_or_else(|| "Android surface blitter was not initialized".to_string())?;
        let source = texture.create_view(&Default::default());
        let target = frame.texture.create_view(&Default::default());
        let mut encoder = device.create_command_encoder(&Default::default());
        blitter.draw(device, queue, &mut encoder, &source, &target, &transform);
        queue.submit([encoder.finish()]);
        queue.present(frame);
        let version = state.version;
        state.signal.mark_clean();
        Ok(RenderOutcome::Presented(version))
    }
}

impl Drop for NativeSurface {
    fn drop(&mut self) {
        self.texture.take();
        self.blitter.take();
        self.config.take();
        // SAFETY: `surface` was initialized in `new` and is dropped exactly once here, before
        // NativeWindowRef releases ANativeWindow and the global Java Surface reference is dropped.
        unsafe { ManuallyDrop::drop(&mut self.surface) };
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderOutcome {
    Idle,
    Retry,
    Occluded,
    Presented(u64),
}

#[derive(Default)]
pub struct GpuRenderer {
    instance: Option<wgpu::Instance>,
    adapter: Option<wgpu::Adapter>,
    device: Option<wgpu::Device>,
    queue: Option<wgpu::Queue>,
    surfaces: HashMap<SurfaceKey, NativeSurface>,
}

impl GpuRenderer {
    pub fn attach(
        &mut self,
        env: &mut Env<'_>,
        key: SurfaceKey,
        java_surface: &JObject<'_>,
        width: u32,
        height: u32,
    ) -> Result<(), String> {
        if self.surfaces.contains_key(&key) {
            return self.resize(key, width, height);
        }
        if self.instance.is_none() {
            self.instance = Some(wgpu::Instance::new(
                wgpu::InstanceDescriptor::new_without_display_handle_from_env(),
            ));
        }
        let mut surface = NativeSurface::new(
            env,
            java_surface,
            self.instance
                .as_ref()
                .ok_or_else(|| "Android GPU instance was not created".to_string())?,
        )?;
        if self.adapter.is_none() {
            self.initialize(&surface.surface)?;
        }
        let adapter = self
            .adapter
            .as_ref()
            .ok_or_else(|| "Android GPU adapter was not initialized".to_string())?;
        let device = self
            .device
            .as_ref()
            .ok_or_else(|| "Android GPU device was not initialized".to_string())?;
        surface.configure(device, adapter, width, height)?;
        self.surfaces.insert(key, surface);
        Ok(())
    }

    pub fn resize(&mut self, key: SurfaceKey, width: u32, height: u32) -> Result<(), String> {
        let Some(surface) = self.surfaces.get_mut(&key) else {
            return Ok(());
        };
        let adapter = self.adapter.as_ref().ok_or_else(|| {
            "cannot resize before the Android GPU adapter is initialized".to_string()
        })?;
        let device = self.device.as_ref().ok_or_else(|| {
            "cannot resize before the Android GPU device is initialized".to_string()
        })?;
        surface.configure(device, adapter, width, height)
    }

    pub fn render(
        &mut self,
        actor: &ActorHandle,
        key: SurfaceKey,
    ) -> Result<RenderOutcome, String> {
        let (Some(adapter), Some(device), Some(queue)) = (
            self.adapter.as_ref(),
            self.device.as_ref(),
            self.queue.as_ref(),
        ) else {
            return Ok(RenderOutcome::Idle);
        };
        let Some(surface) = self.surfaces.get_mut(&key) else {
            return Ok(RenderOutcome::Idle);
        };
        let Some(state) = actor.surface(key) else {
            return Ok(RenderOutcome::Idle);
        };
        let mut state = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let outcome = surface.present(&mut state, device, adapter, queue)?;
        drop(state);
        if let RenderOutcome::Presented(version) = outcome {
            match actor.send_presented(key, version) {
                Ok(()) => {}
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    return Ok(RenderOutcome::Idle);
                }
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    return Err(format!(
                        "presentation completion queue is full for session {} window {}",
                        key.session, key.window
                    ));
                }
            }
        }
        Ok(outcome)
    }

    pub fn detach(&mut self, key: SurfaceKey) {
        self.surfaces.remove(&key);
        if self.surfaces.is_empty() {
            self.queue.take();
            self.device.take();
            self.adapter.take();
            self.instance.take();
        }
    }

    fn initialize(&mut self, surface: &wgpu::Surface<'_>) -> Result<(), String> {
        let instance = self
            .instance
            .as_ref()
            .ok_or_else(|| "Android GPU instance was not created".to_string())?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            compatible_surface: Some(surface),
            ..Default::default()
        }))
        .map_err(|error| format!("no compatible Android GPU adapter: {error}"))?;
        let adapter_info = adapter.get_info();
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
                .map_err(|error| {
                    format!(
                        "failed to create Android GPU device on {}: {error}",
                        adapter_info.name
                    )
                })?;
        device.on_uncaptured_error(Arc::new(|error| {
            log::error!("Uncaptured Android GPU error: {error}");
        }));
        self.adapter = Some(adapter);
        self.device = Some(device);
        self.queue = Some(queue);
        log::info!(
            "Android GPU adapter: {} ({:?})",
            adapter_info.name,
            adapter_info.backend
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_capabilities(formats: Vec<wgpu::TextureFormat>) -> wgpu::SurfaceCapabilities {
        wgpu::SurfaceCapabilities {
            formats,
            present_modes: vec![wgpu::PresentMode::Fifo],
            alpha_modes: vec![wgpu::CompositeAlphaMode::Inherit],
            usages: wgpu::TextureUsages::RENDER_ATTACHMENT,
            ..Default::default()
        }
    }

    fn compressed() -> wgpu::TextureFormat {
        wgpu::TextureFormat::Astc {
            block: wgpu::AstcBlock::B4x4,
            channel: wgpu::AstcChannel::Unorm,
        }
    }

    #[test]
    fn negotiates_uncompressed_formats_and_capability_safe_modes() {
        let capabilities = test_capabilities(vec![
            compressed(),
            wgpu::TextureFormat::Bgra8Unorm,
            wgpu::TextureFormat::Rgba8UnormSrgb,
        ]);
        let plan = SurfacePlan::negotiate(&capabilities).unwrap();
        assert_eq!(plan.format, wgpu::TextureFormat::Rgba8UnormSrgb);
        assert_eq!(plan.alpha_mode, wgpu::CompositeAlphaMode::Auto);
        assert_eq!(plan.present_mode, wgpu::PresentMode::AutoVsync);
        assert_eq!(plan.color_space, wgpu::SurfaceColorSpace::Auto);

        let config = plan.configuration(0, 0);
        assert_eq!((config.width, config.height), (1, 1));
        assert_eq!(config.usage, wgpu::TextureUsages::RENDER_ATTACHMENT);
        assert_eq!(config.alpha_mode, wgpu::CompositeAlphaMode::Auto);
        assert_eq!(config.present_mode, wgpu::PresentMode::AutoVsync);
        assert_eq!(config.color_space, wgpu::SurfaceColorSpace::Auto);
        assert!(config.view_formats.is_empty());
    }

    #[test]
    fn rejects_missing_usage_alpha_present_or_renderable_formats() {
        let mut capabilities = test_capabilities(vec![compressed()]);
        assert!(SurfacePlan::negotiate(&capabilities)
            .unwrap_err()
            .contains("no Rgba8/Bgra8 formats"));

        capabilities.formats = vec![wgpu::TextureFormat::Rgba8Unorm];
        capabilities.usages = wgpu::TextureUsages::TEXTURE_BINDING;
        assert!(SurfacePlan::negotiate(&capabilities)
            .unwrap_err()
            .contains("RENDER_ATTACHMENT"));

        let mut capabilities = test_capabilities(vec![wgpu::TextureFormat::Rgba8Unorm]);
        capabilities.alpha_modes.clear();
        assert!(SurfacePlan::negotiate(&capabilities)
            .unwrap_err()
            .contains("no supported alpha modes"));

        let mut capabilities = test_capabilities(vec![wgpu::TextureFormat::Rgba8Unorm]);
        capabilities.present_modes.clear();
        assert!(SurfacePlan::negotiate(&capabilities)
            .unwrap_err()
            .contains("no supported present modes"));
    }
}
