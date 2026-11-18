use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr::NonNull;

use jni::objects::JObject;
use jni::refs::Global;
use jni::Env;
use raw_window_handle::{AndroidDisplayHandle, AndroidNdkWindowHandle};
use waypane_core::Size;

use crate::{ActorHandle, AndroidSurface, SurfaceKey};

#[cfg_attr(target_os = "android", link(name = "android"))]
unsafe extern "C" {
    fn ANativeWindow_fromSurface(
        env: *mut jni::sys::JNIEnv,
        surface: jni::sys::jobject,
    ) -> *mut c_void;
    fn ANativeWindow_release(window: *mut c_void);
}

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

struct NativeSurface {
    _java_surface: Global<JObject<'static>>,
    _window: NativeWindowRef,
    surface: Option<wgpu::Surface<'static>>,
    config: wgpu::SurfaceConfiguration,
    blitter: wgpu::util::TextureBlitter,
    texture: Option<wgpu::Texture>,
    texture_size: Size,
}

impl NativeSurface {
    fn create(
        env: &mut Env<'_>,
        java_surface: &JObject<'_>,
        instance: &wgpu::Instance,
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
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
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            color_space: wgpu::SurfaceColorSpace::Auto,
            width: 1,
            height: 1,
            present_mode: wgpu::PresentMode::AutoVsync,
            desired_maximum_frame_latency: 2,
            alpha_mode: wgpu::CompositeAlphaMode::Opaque,
            view_formats: Vec::new(),
        };
        surface.configure(device, &config);
        Ok(Self {
            _java_surface: java_surface,
            _window: window,
            surface: Some(surface),
            blitter: wgpu::util::TextureBlitter::new(device, format),
            config,
            texture: None,
            texture_size: Size::default(),
        })
    }

    fn resize(&mut self, width: u32, height: u32, device: &wgpu::Device) {
        if width == 0 || height == 0 {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        if let Some(surface) = &self.surface {
            surface.configure(device, &self.config);
        }
    }

    fn render(
        &mut self,
        state: &mut AndroidSurface,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
    ) -> Option<u64> {
        if !state.signal.is_dirty() {
            return None;
        }
        let size = state.canvas.size();
        if size.width == 0 || size.height == 0 {
            return None;
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
        }
        let texture = self.texture.as_ref()?;
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

        let surface = self.surface.as_ref()?;
        let frame = match surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame)
            | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
            wgpu::CurrentSurfaceTexture::Outdated => {
                surface.configure(device, &self.config);
                return None;
            }
            _ => return None,
        };
        let source = texture.create_view(&Default::default());
        let target = frame.texture.create_view(&Default::default());
        let mut encoder = device.create_command_encoder(&Default::default());
        self.blitter.copy(device, &mut encoder, &source, &target);
        queue.submit([encoder.finish()]);
        queue.present(frame);
        let version = state.version;
        state.signal.mark_clean();
        Some(version)
    }
}

impl Drop for NativeSurface {
    fn drop(&mut self) {
        let _ = self.surface.take();
    }
}

#[derive(Default)]
pub struct GpuRenderer {
    instance: Option<wgpu::Instance>,
    device: Option<wgpu::Device>,
    queue: Option<wgpu::Queue>,
    format: Option<wgpu::TextureFormat>,
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
            return Ok(());
        }
        if self.device.is_none() {
            self.initialize(env, java_surface)?;
        }
        let mut surface = NativeSurface::create(
            env,
            java_surface,
            self.instance.as_ref().unwrap(),
            self.device.as_ref().unwrap(),
            self.format.unwrap(),
        )?;
        if width > 0 && height > 0 {
            surface.resize(width, height, self.device.as_ref().unwrap());
        }
        self.surfaces.insert(key, surface);
        Ok(())
    }

    pub fn resize(&mut self, key: SurfaceKey, width: u32, height: u32) {
        if let (Some(surface), Some(device)) = (self.surfaces.get_mut(&key), self.device.as_ref()) {
            surface.resize(width, height, device);
        }
    }

    pub fn render(&mut self, actor: &ActorHandle, key: SurfaceKey) -> bool {
        let (Some(state), Some(surface), Some(device), Some(queue)) = (
            actor.surface(key),
            self.surfaces.get_mut(&key),
            self.device.as_ref(),
            self.queue.as_ref(),
        ) else {
            return false;
        };
        let mut state = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(version) = surface.render(&mut state, device, queue) else {
            return false;
        };
        drop(state);
        actor.send(crate::ActorCommand::Presented(key, version))
    }

    pub fn detach(&mut self, key: SurfaceKey) {
        self.surfaces.remove(&key);
        if self.surfaces.is_empty() {
            self.device = None;
            self.queue = None;
            self.format = None;
            self.instance = None;
        }
    }

    fn initialize(&mut self, env: &mut Env<'_>, java_surface: &JObject<'_>) -> Result<(), String> {
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
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
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            compatible_surface: Some(&surface),
            ..Default::default()
        }))
        .map_err(|error| format!("no compatible Android GPU adapter: {error}"))?;
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
                .map_err(|error| format!("failed to create Android GPU device: {error}"))?;
        let format = *surface
            .get_capabilities(&adapter)
            .formats
            .first()
            .ok_or_else(|| "Android surface has no supported formats".to_string())?;
        log::info!("Android GPU adapter: {}", adapter.get_info().name);
        self.instance = Some(instance);
        self.device = Some(device);
        self.queue = Some(queue);
        self.format = Some(format);
        Ok(())
    }
}
