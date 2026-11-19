mod gpu;

use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(target_os = "android")]
use std::sync::Once;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use gpu::{GpuRenderer, RenderOutcome};
use jni::objects::{JClass, JObject, JString};
use jni::sys::{jboolean, jfloat, jint, jlong, jobject};
use jni::{Env, EnvUnowned};
use waypane_core::{Size, MAX_WAYLAND_SCALE};
use waypane_render::Viewport;
use waypane_transport::{Authentication, HostKeyDecision, HostKeyPolicy, RemoteWaypipe, SshConfig};
use waypipe_client::Compression;

use crate::keycodes::android_keycode_to_evdev;
use crate::{ActorCommand, ActorHandle, AndroidSurface, EngineError, SessionId, SurfaceKey};

#[cfg(target_os = "android")]
fn init_logging() {
    static START: Once = Once::new();
    START.call_once(|| {
        android_logger::init_once(
            android_logger::Config::default()
                .with_tag("WaypaneRust")
                .with_max_level(log::LevelFilter::Info),
        );
        std::panic::set_hook(Box::new(|info| {
            let payload = info.payload();
            let message = payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| {
                    payload
                        .downcast_ref::<&str>()
                        .map(|value| (*value).to_owned())
                })
                .or_else(|| {
                    payload
                        .downcast_ref::<wgpu::Error>()
                        .map(ToString::to_string)
                })
                .unwrap_or_else(|| "non-string panic payload".to_owned());
            let backtrace = std::backtrace::Backtrace::force_capture();
            if let Some(location) = info.location() {
                log::error!(
                    "Rust panic at {}:{}:{}: {message}\n{backtrace}",
                    location.file(),
                    location.line(),
                    location.column()
                );
            } else {
                log::error!("Rust panic: {message}\n{backtrace}");
            }
        }));
    });
}

#[cfg(not(target_os = "android"))]
fn init_logging() {}

struct NativeState {
    gpu: Mutex<GpuRenderer>,
    actor: ActorHandle,
}

struct NativeEntry {
    handle: jlong,
    state: Arc<NativeState>,
}

static STATE: Mutex<Option<NativeEntry>> = Mutex::new(None);
static NEXT_HANDLE: AtomicU64 = AtomicU64::new(1);

const RENDER_IDLE: jint = 0;
const RENDER_PRESENTED: jint = 1;
const RENDER_RETRY: jint = 2;
const RENDER_OCCLUDED: jint = 3;

#[derive(Debug)]
struct JniError(String);

impl fmt::Display for JniError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for JniError {}

impl From<jni::errors::Error> for JniError {
    fn from(error: jni::errors::Error) -> Self {
        Self(error.to_string())
    }
}

macro_rules! jni_guard {
    ($unowned:expr, $jni_env:ident, $ret:ty, $body:expr) => {{
        $unowned
            .with_env(|mut $jni_env| -> Result<$ret, JniError> {
                let _ = &mut $jni_env;
                let result: Result<$ret, String> = (|| $body)();
                result.map_err(JniError)
            })
            .resolve::<jni::errors::ThrowRuntimeExAndDefault>()
    }};
}

fn text(env: &mut Env<'_>, value: &JString<'_>) -> Result<String, String> {
    let value = value
        .try_to_string(env)
        .map_err(|error| format!("invalid JNI string: {error}"))?;
    Ok(value)
}

fn optional_text(env: &mut Env<'_>, value: &JString<'_>) -> Result<Option<String>, String> {
    let value = text(env, value)?;
    Ok((!value.trim().is_empty()).then_some(value))
}

fn checked_output_scale(scale: jint) -> Result<i32, String> {
    if !(1..=MAX_WAYLAND_SCALE).contains(&scale) {
        return Err(format!(
            "invalid Wayland scale {scale}; expected 1..={MAX_WAYLAND_SCALE}"
        ));
    }
    Ok(scale)
}

fn key(session: jlong, window: jint) -> Result<SurfaceKey, String> {
    let session = SessionId::try_from(session).map_err(|_| "invalid session id".to_string())?;
    let window = u32::try_from(window).map_err(|_| "invalid window id".to_string())?;
    Ok(SurfaceKey { session, window })
}

fn state(handle: jlong) -> Result<Arc<NativeState>, String> {
    if handle == 0 {
        return Err("native handle is closed".to_string());
    }
    lock(&STATE)
        .as_ref()
        .filter(|entry| entry.handle == handle)
        .map(|entry| entry.state.clone())
        .ok_or_else(|| "native handle is closed".to_string())
}

fn surface(state: &NativeState, key: SurfaceKey) -> Result<Arc<Mutex<AndroidSurface>>, String> {
    state
        .actor
        .surface(key)
        .ok_or_else(|| EngineError::UnknownSurface(key).to_string())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn jb(value: bool) -> jboolean {
    value
}

#[no_mangle]
pub extern "system" fn Java_org_waypane_app_NativeBridge_nativeConnect(
    mut env: EnvUnowned<'_>,
    _class: JClass<'_>,
    host: JString<'_>,
    port: jint,
    user: JString<'_>,
    identity: JString<'_>,
    passphrase: JString<'_>,
    password: JString<'_>,
    known_hosts: JString<'_>,
    remote_socket: JString<'_>,
    compression: JString<'_>,
    command: JString<'_>,
    wayland_scale: jint,
) -> jlong {
    jni_guard!(env, env, jlong, {
        init_logging();
        let host = text(env, &host)?;
        let user = text(env, &user)?;
        if host.trim().is_empty() || user.trim().is_empty() {
            return Err("host and user are required".to_string());
        }
        let port = u16::try_from(port).map_err(|_| "invalid SSH port".to_string())?;
        let output_scale = checked_output_scale(wayland_scale)?;
        let command = text(env, &command)?;
        let command = shlex::split(&command)
            .filter(|parts| !parts.is_empty())
            .ok_or_else(|| "remote command contains an unterminated quote".to_string())?;
        let compression = match text(env, &compression)?.as_str() {
            "none" => Compression::None,
            "lz4" => Compression::Lz4,
            "zstd" => Compression::Zstd,
            _ => return Err("unsupported compression".to_string()),
        };
        let identity = optional_text(env, &identity)?;
        let passphrase = optional_text(env, &passphrase)?;
        let password = optional_text(env, &password)?;
        if passphrase.is_some() && identity.is_none() {
            return Err("a private key passphrase requires an imported private key".to_string());
        }
        let known_hosts = optional_text(env, &known_hosts)?
            .map(PathBuf::from)
            .ok_or_else(|| "known_hosts path is required".to_string())?;
        let authentication = match (identity, password) {
            (Some(path), _) => Authentication::PublicKey {
                path: PathBuf::from(path),
                passphrase,
            },
            (None, Some(password)) => Authentication::Password(password),
            (None, None) => {
                return Err("import an SSH private key or enter a password".to_string());
            }
        };
        let mut remote =
            RemoteWaypipe::new(compression, command).map_err(|error| error.to_string())?;
        if let Some(path) = optional_text(env, &remote_socket)? {
            remote = remote
                .with_remote_socket(path)
                .map_err(|error| error.to_string())?;
        }
        let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed) as jlong;
        if handle <= 0 {
            return Err("native handle space exhausted".to_string());
        }
        let state = Arc::new(NativeState {
            gpu: Mutex::new(GpuRenderer::default()),
            actor: ActorHandle::start(
                SshConfig {
                    host,
                    port,
                    user,
                    authentication,
                    known_hosts: Some(known_hosts),
                    host_key_policy: HostKeyPolicy::Check,
                },
                remote,
                output_scale,
            ),
        });
        let previous = lock(&STATE).replace(NativeEntry { handle, state });
        drop(previous);
        Ok(handle)
    })
}

#[no_mangle]
pub extern "system" fn Java_org_waypane_app_NativeBridge_nativeResolveHostKey(
    mut env: EnvUnowned<'_>,
    _class: JClass<'_>,
    handle: jlong,
    trusted: jboolean,
) -> jboolean {
    jni_guard!(env, env, jboolean, {
        let decision = if trusted {
            HostKeyDecision::TrustAndSave
        } else {
            HostKeyDecision::Reject
        };
        Ok(jb(state(handle)?.actor.resolve_host_key(decision)))
    })
}

#[no_mangle]
pub extern "system" fn Java_org_waypane_app_NativeBridge_nativeNextEvent(
    mut env: EnvUnowned<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jobject {
    jni_guard!(env, env, jobject, {
        let state = state(handle)?;
        let Some(event) = state.actor.next_event() else {
            return Ok(JObject::null().into_raw());
        };
        let json = serde_json::to_string(&event).map_err(|error| error.to_string())?;
        let value = env
            .new_string(json)
            .map_err(|error| format!("failed to allocate JNI string: {error}"))?;
        Ok(value.into_raw())
    })
}

#[no_mangle]
pub extern "system" fn Java_org_waypane_app_NativeBridge_nativeAttachSurface(
    mut env: EnvUnowned<'_>,
    _class: JClass<'_>,
    handle: jlong,
    session: jlong,
    window: jint,
    java_surface: JObject<'_>,
    width: jint,
    height: jint,
) -> jboolean {
    jni_guard!(env, env, jboolean, {
        let key = key(session, window)?;
        let state = state(handle)?;
        let view = surface(&state, key)?;
        let width = u32::try_from(width).map_err(|_| "invalid width".to_string())?;
        let height = u32::try_from(height).map_err(|_| "invalid height".to_string())?;
        lock(&state.gpu).attach(env, key, &java_surface, width, height)?;
        lock(&view).set_view_size(Size {
            width: width.max(1),
            height: height.max(1),
        });
        Ok(jb(true))
    })
}

#[no_mangle]
pub extern "system" fn Java_org_waypane_app_NativeBridge_nativeResizeSurface(
    mut env: EnvUnowned<'_>,
    _class: JClass<'_>,
    handle: jlong,
    session: jlong,
    window: jint,
    width: jint,
    height: jint,
) {
    jni_guard!(env, env, (), {
        let key = key(session, window)?;
        let width = u32::try_from(width).map_err(|_| "invalid width".to_string())?;
        let height = u32::try_from(height).map_err(|_| "invalid height".to_string())?;
        let state = state(handle)?;
        let view = surface(&state, key)?;
        lock(&state.gpu).resize(key, width, height)?;
        lock(&view).set_view_size(Size {
            width: width.max(1),
            height: height.max(1),
        });
        Ok(())
    })
}

#[no_mangle]
pub extern "system" fn Java_org_waypane_app_NativeBridge_nativeSetViewport(
    mut env: EnvUnowned<'_>,
    _class: JClass<'_>,
    handle: jlong,
    session: jlong,
    window: jint,
    zoom: jfloat,
    pan_x: jfloat,
    pan_y: jfloat,
) {
    jni_guard!(env, env, (), {
        let key = key(session, window)?;
        let state = state(handle)?;
        let view = surface(&state, key)?;
        lock(&view).set_viewport(Viewport { zoom, pan_x, pan_y });
        Ok(())
    })
}

#[no_mangle]
pub extern "system" fn Java_org_waypane_app_NativeBridge_nativeSetOutputScale(
    mut env: EnvUnowned<'_>,
    _class: JClass<'_>,
    handle: jlong,
    scale: jint,
) -> jboolean {
    jni_guard!(env, env, jboolean, {
        let scale = checked_output_scale(scale)?;
        Ok(jb(state(handle)?
            .actor
            .send(ActorCommand::SetOutputScale(scale))))
    })
}

#[no_mangle]
pub extern "system" fn Java_org_waypane_app_NativeBridge_nativeRender(
    mut env: EnvUnowned<'_>,
    _class: JClass<'_>,
    handle: jlong,
    session: jlong,
    window: jint,
) -> jint {
    jni_guard!(env, env, jint, {
        let key = key(session, window)?;
        let state = state(handle)?;
        let outcome = lock(&state.gpu).render(&state.actor, key)?;
        Ok(match outcome {
            RenderOutcome::Idle => RENDER_IDLE,
            RenderOutcome::Presented(_) => RENDER_PRESENTED,
            RenderOutcome::Retry => RENDER_RETRY,
            RenderOutcome::Occluded => RENDER_OCCLUDED,
        })
    })
}

#[no_mangle]
pub extern "system" fn Java_org_waypane_app_NativeBridge_nativeDetachSurface(
    mut env: EnvUnowned<'_>,
    _class: JClass<'_>,
    handle: jlong,
    session: jlong,
    window: jint,
) {
    jni_guard!(env, env, (), {
        let state = state(handle)?;
        lock(&state.gpu).detach(key(session, window)?);
        Ok(())
    })
}

#[no_mangle]
pub extern "system" fn Java_org_waypane_app_NativeBridge_nativeIsDirty(
    mut env: EnvUnowned<'_>,
    _class: JClass<'_>,
    handle: jlong,
    session: jlong,
    window: jint,
) -> jboolean {
    jni_guard!(env, env, jboolean, {
        let key = key(session, window)?;
        let dirty = state(handle)?
            .actor
            .surface(key)
            .map(|surface| {
                surface
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .signal
                    .is_dirty()
            })
            .unwrap_or(false);
        Ok(jb(dirty))
    })
}

#[no_mangle]
pub extern "system" fn Java_org_waypane_app_NativeBridge_nativeWaitDirty(
    mut env: EnvUnowned<'_>,
    _class: JClass<'_>,
    handle: jlong,
    session: jlong,
    window: jint,
    timeout_ms: jint,
) -> jlong {
    jni_guard!(env, env, jlong, {
        let key = key(session, window)?;
        let timeout_ms = u64::try_from(timeout_ms).unwrap_or(0).min(300_000);
        let signal = state(handle)?.actor.surface(key).map(|surface| {
            surface
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .signal()
        });
        let version =
            signal.and_then(|signal| signal.wait_dirty(Duration::from_millis(timeout_ms)));
        Ok(version.map_or(0, |version| version as jlong))
    })
}

#[no_mangle]
pub extern "system" fn Java_org_waypane_app_NativeBridge_nativeWakeSurface(
    mut env: EnvUnowned<'_>,
    _class: JClass<'_>,
    handle: jlong,
    session: jlong,
    window: jint,
) {
    jni_guard!(env, env, (), {
        if let Some(surface) = state(handle)?.actor.surface(key(session, window)?) {
            let signal = surface
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .signal();
            signal.wake();
        }
        Ok(())
    })
}

#[no_mangle]
pub extern "system" fn Java_org_waypane_app_NativeBridge_nativeTouch(
    mut env: EnvUnowned<'_>,
    _class: JClass<'_>,
    handle: jlong,
    session: jlong,
    window: jint,
    pointer: jint,
    action: jint,
    x: jfloat,
    y: jfloat,
) -> jboolean {
    jni_guard!(env, env, jboolean, {
        let key = key(session, window)?;
        let command = match action {
            0 => ActorCommand::TouchDown(key, pointer, f64::from(x), f64::from(y)),
            1 => ActorCommand::TouchMotion(key, pointer, f64::from(x), f64::from(y)),
            2 => ActorCommand::TouchUp(key, pointer, f64::from(x), f64::from(y)),
            _ => return Err("unknown touch action".to_string()),
        };
        Ok(jb(state(handle)?.actor.send(command)))
    })
}

#[no_mangle]
pub extern "system" fn Java_org_waypane_app_NativeBridge_nativeTouchCancel(
    mut env: EnvUnowned<'_>,
    _class: JClass<'_>,
    handle: jlong,
    session: jlong,
    window: jint,
) -> jboolean {
    jni_guard!(env, env, jboolean, {
        Ok(jb(state(handle)?
            .actor
            .send(ActorCommand::TouchCancel(key(session, window)?))))
    })
}

#[no_mangle]
pub extern "system" fn Java_org_waypane_app_NativeBridge_nativePointer(
    mut env: EnvUnowned<'_>,
    _class: JClass<'_>,
    handle: jlong,
    session: jlong,
    window: jint,
    x: jfloat,
    y: jfloat,
    button: jint,
    pressed: jboolean,
) -> jboolean {
    jni_guard!(env, env, jboolean, {
        let key = key(session, window)?;
        let button = (button >= 0).then(|| (u32::try_from(button).unwrap(), pressed));
        Ok(jb(state(handle)?.actor.send(ActorCommand::Pointer(
            key,
            f64::from(x),
            f64::from(y),
            button,
        ))))
    })
}

#[no_mangle]
pub extern "system" fn Java_org_waypane_app_NativeBridge_nativeScroll(
    mut env: EnvUnowned<'_>,
    _class: JClass<'_>,
    handle: jlong,
    session: jlong,
    window: jint,
    x: jfloat,
    y: jfloat,
    horizontal: jfloat,
    vertical: jfloat,
) -> jboolean {
    jni_guard!(env, env, jboolean, {
        Ok(jb(state(handle)?.actor.send(ActorCommand::Scroll(
            key(session, window)?,
            f64::from(x),
            f64::from(y),
            f64::from(horizontal),
            f64::from(vertical),
        ))))
    })
}

#[no_mangle]
pub extern "system" fn Java_org_waypane_app_NativeBridge_nativeKey(
    mut env: EnvUnowned<'_>,
    _class: JClass<'_>,
    handle: jlong,
    session: jlong,
    window: jint,
    android_keycode: jint,
    pressed: jboolean,
) -> jboolean {
    jni_guard!(env, env, jboolean, {
        let Some(code) = android_keycode_to_evdev(android_keycode) else {
            return Ok(jb(false));
        };
        Ok(jb(state(handle)?.actor.send(ActorCommand::Key(
            key(session, window)?,
            code,
            pressed,
        ))))
    })
}

#[no_mangle]
pub extern "system" fn Java_org_waypane_app_NativeBridge_nativeCommitText(
    mut env: EnvUnowned<'_>,
    _class: JClass<'_>,
    handle: jlong,
    session: jlong,
    window: jint,
    value: JString<'_>,
) -> jboolean {
    jni_guard!(env, env, jboolean, {
        let text = text(env, &value)?;
        Ok(jb(state(handle)?.actor.send(ActorCommand::CommitText(
            key(session, window)?,
            text,
        ))))
    })
}

#[no_mangle]
pub extern "system" fn Java_org_waypane_app_NativeBridge_nativeSetPreeditText(
    mut env: EnvUnowned<'_>,
    _class: JClass<'_>,
    handle: jlong,
    session: jlong,
    window: jint,
    value: JString<'_>,
    cursor_begin: jint,
    cursor_end: jint,
) -> jboolean {
    jni_guard!(env, env, jboolean, {
        let text = text(env, &value)?;
        let text = (!text.is_empty()).then_some(text);
        Ok(jb(state(handle)?.actor.send(ActorCommand::SetPreedit(
            key(session, window)?,
            text,
            cursor_begin,
            cursor_end,
        ))))
    })
}

#[no_mangle]
pub extern "system" fn Java_org_waypane_app_NativeBridge_nativeDeleteSurroundingText(
    mut env: EnvUnowned<'_>,
    _class: JClass<'_>,
    handle: jlong,
    session: jlong,
    window: jint,
    before_length: jint,
    after_length: jint,
) -> jboolean {
    jni_guard!(env, env, jboolean, {
        let before_length =
            u32::try_from(before_length).map_err(|_| "invalid before_length".to_string())?;
        let after_length =
            u32::try_from(after_length).map_err(|_| "invalid after_length".to_string())?;
        Ok(jb(state(handle)?.actor.send(
            ActorCommand::DeleteSurrounding(key(session, window)?, before_length, after_length),
        )))
    })
}

#[no_mangle]
pub extern "system" fn Java_org_waypane_app_NativeBridge_nativeSetClipboardText(
    mut env: EnvUnowned<'_>,
    _class: JClass<'_>,
    handle: jlong,
    value: JString<'_>,
) -> jboolean {
    jni_guard!(env, env, jboolean, {
        let text = text(env, &value)?;
        Ok(jb(state(handle)?
            .actor
            .send(ActorCommand::SetClipboard(text))))
    })
}

#[no_mangle]
pub extern "system" fn Java_org_waypane_app_NativeBridge_nativeCloseWindow(
    mut env: EnvUnowned<'_>,
    _class: JClass<'_>,
    handle: jlong,
    session: jlong,
    window: jint,
) -> jboolean {
    jni_guard!(env, env, jboolean, {
        Ok(jb(state(handle)?
            .actor
            .send(ActorCommand::CloseWindow(key(session, window)?))))
    })
}

#[no_mangle]
pub extern "system" fn Java_org_waypane_app_NativeBridge_nativeResizeWindow(
    mut env: EnvUnowned<'_>,
    _class: JClass<'_>,
    handle: jlong,
    session: jlong,
    window: jint,
    width: jint,
    height: jint,
) -> jboolean {
    jni_guard!(env, env, jboolean, {
        let width = u32::try_from(width).map_err(|_| "invalid width".to_string())?;
        let height = u32::try_from(height).map_err(|_| "invalid height".to_string())?;
        Ok(jb(state(handle)?.actor.send(ActorCommand::Resize(
            key(session, window)?,
            Size { width, height },
        ))))
    })
}

#[no_mangle]
pub extern "system" fn Java_org_waypane_app_NativeBridge_nativeDisconnectSession(
    mut env: EnvUnowned<'_>,
    _class: JClass<'_>,
    handle: jlong,
    session: jlong,
) -> jboolean {
    jni_guard!(env, env, jboolean, {
        let session = SessionId::try_from(session).map_err(|_| "invalid session id".to_string())?;
        Ok(jb(state(handle)?
            .actor
            .send(ActorCommand::DisconnectSession(session))))
    })
}

#[allow(clippy::redundant_closure_call)]
#[no_mangle]
pub extern "system" fn Java_org_waypane_app_NativeBridge_nativeDestroy(
    mut env: EnvUnowned<'_>,
    _class: JClass<'_>,
    handle: jlong,
) {
    jni_guard!(env, env, (), {
        if handle != 0 {
            let previous = lock(&STATE).take_if(|entry| entry.handle == handle);
            drop(previous);
        }
        Ok(())
    })
}
