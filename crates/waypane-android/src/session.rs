use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

use bytes::Bytes;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use waypane_transport::{pump, CommandEvent, RemoteWaypipe, SshConfig, SshSession};

use crate::engine::{
    AndroidEngine, AndroidSurface, EngineError, EngineEvent, EngineOutput, SurfaceKey,
};
use waypane_core::Size;

pub type SessionId = u64;

type Routes = Arc<Mutex<HashMap<SessionId, mpsc::Sender<Bytes>>>>;

#[derive(Debug)]
pub enum ActorCommand {
    NetworkData(SessionId, Bytes),
    NetworkClosed(SessionId),
    TransportError(SessionId, String),
    Presented(SurfaceKey, u64),
    TouchDown(SurfaceKey, i32, f64, f64),
    TouchMotion(SurfaceKey, i32, f64, f64),
    TouchUp(SurfaceKey, i32),
    TouchCancel(SurfaceKey),
    Scroll(SurfaceKey, f64, f64, f64, f64),
    Key(SurfaceKey, u32, bool),
    Pointer(SurfaceKey, f64, f64, Option<(u32, bool)>),
    CloseWindow(SurfaceKey),
    DisconnectSession(SessionId),
    Resize(SurfaceKey, Size),
    Stop,
}

#[derive(Clone)]
pub struct ActorHandle {
    engine: Arc<Mutex<AndroidEngine>>,
    commands: mpsc::Sender<ActorCommand>,
    events: Arc<Mutex<mpsc::UnboundedReceiver<EngineEvent>>>,
}

impl ActorHandle {
    pub fn start(config: SshConfig, remote: RemoteWaypipe) -> Self {
        let compression = remote.compression;
        let engine = Arc::new(Mutex::new(AndroidEngine::new(compression)));
        let (commands, command_rx) = mpsc::channel(256);
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let handle = Self {
            engine: engine.clone(),
            commands,
            events: Arc::new(Mutex::new(event_rx)),
        };
        let actor_commands = handle.commands.clone();
        thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to create Android Tokio runtime");
            runtime.block_on(run_actor(
                config,
                remote,
                engine,
                command_rx,
                actor_commands,
                event_tx,
            ));
        });
        handle
    }

    pub fn surface(&self, key: SurfaceKey) -> Option<Arc<Mutex<AndroidSurface>>> {
        lock(&self.engine).surface(key)
    }

    pub fn next_event(&self) -> Option<EngineEvent> {
        lock(&self.events).try_recv().ok()
    }

    pub fn send(&self, command: ActorCommand) -> bool {
        self.commands.try_send(command).is_ok()
    }

    pub fn stop(&self) {
        let _ = self.commands.try_send(ActorCommand::Stop);
    }
}

async fn run_actor(
    config: SshConfig,
    remote: RemoteWaypipe,
    engine: Arc<Mutex<AndroidEngine>>,
    mut commands: mpsc::Receiver<ActorCommand>,
    command_tx: mpsc::Sender<ActorCommand>,
    events: mpsc::UnboundedSender<EngineEvent>,
) {
    let (network_tx, mut network_rx) = mpsc::channel::<(SessionId, Bytes)>(64);
    let routes: Routes = Arc::new(Mutex::new(HashMap::new()));
    let mut session = match SshSession::connect(config, &remote).await {
        Ok(session) => session,
        Err(error) => {
            let _ = events.send(EngineEvent::Error {
                session: None,
                message: error.to_string(),
            });
            return;
        }
    };
    let mut ssh_events = session.take_events();
    let mut ssh_open = true;
    let mut remote_exited = false;

    loop {
        if remote_exited && lock(&routes).is_empty() {
            break;
        }
        tokio::select! {
            stream = session.accept(), if ssh_open => match stream {
                Some(stream) => {
                    let session_id = next_session_id();
                    let output = lock(&engine).connected(session_id);
                    emit_output(&events, output.events);
                    spawn_stream(session_id, stream, routes.clone(), network_tx.clone(), command_tx.clone());
                }
                None => {
                    ssh_open = false;
                    remote_exited = true;
                }
            },
            status = next_status(&mut ssh_events) => match status {
                Some(CommandEvent::Stdout(data)) | Some(CommandEvent::Stderr(data)) => {
                    let _ = events.send(EngineEvent::Status {
                        message: String::from_utf8_lossy(&data).into_owned(),
                    });
                }
                Some(CommandEvent::ExitStatus(status)) => {
                    let _ = events.send(EngineEvent::Status {
                        message: format!("remote command exited: {status:?}"),
                    });
                    remote_exited = true;
                }
                None => {
                    ssh_events = None;
                    remote_exited = true;
                }
            },
            packet = network_rx.recv() => {
                if let Some((id, data)) = packet {
                    let result = lock(&engine).receive(id, &data);
                    dispatch_result(&engine, result, id, &routes, &events).await;
                }
            }
            command = commands.recv() => {
                let Some(command) = command else { break };
                let (id, result) = match command {
                    ActorCommand::NetworkData(id, data) => (id, lock(&engine).receive(id, &data)),
                    ActorCommand::NetworkClosed(id) => {
                        lock(&routes).remove(&id);
                        (id, lock(&engine).disconnect(id, false))
                    }
                    ActorCommand::TransportError(id, message) => {
                        let _ = events.send(EngineEvent::Error {
                            session: Some(id),
                            message,
                        });
                        lock(&routes).remove(&id);
                        (id, lock(&engine).disconnect(id, false))
                    }
                    ActorCommand::Presented(key, version) => (key.session, lock(&engine).presented(key, version)),
                    ActorCommand::TouchDown(key, pointer, x, y) => (key.session, lock(&engine).touch_down(key, pointer, x, y)),
                    ActorCommand::TouchMotion(key, pointer, x, y) => (key.session, lock(&engine).touch_motion(key, pointer, x, y)),
                    ActorCommand::TouchUp(key, pointer) => (key.session, lock(&engine).touch_up(key, pointer)),
                    ActorCommand::TouchCancel(key) => (key.session, lock(&engine).touch_cancel(key)),
                    ActorCommand::Scroll(key, x, y, horizontal, vertical) => (key.session, lock(&engine).scroll(key, x, y, horizontal, vertical)),
                    ActorCommand::Key(key, code, pressed) => (key.session, lock(&engine).key(key, code, pressed)),
                    ActorCommand::Pointer(key, x, y, button) => (key.session, lock(&engine).pointer(key, x, y, button)),
                    ActorCommand::CloseWindow(key) => (key.session, lock(&engine).close_window(key)),
                    ActorCommand::Resize(key, size) => (key.session, lock(&engine).resize(key, size)),
                    ActorCommand::DisconnectSession(id) => {
                        let result = lock(&engine).disconnect(id, true);
                        dispatch_result(&engine, result, id, &routes, &events).await;
                        lock(&routes).remove(&id);
                        continue;
                    }
                    ActorCommand::Stop => {
                        let session_ids = lock(&engine).session_ids();
                        for id in session_ids {
                            let result = lock(&engine).disconnect(id, true);
                            dispatch_result(&engine, result, id, &routes, &events).await;
                            lock(&routes).remove(&id);
                        }
                        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
                        while !lock(&routes).is_empty() && tokio::time::Instant::now() < deadline {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                        break;
                    }
                };
                dispatch_result(&engine, result, id, &routes, &events).await;
            }
        }
    }
    if let Err(error) = session.shutdown().await {
        let _ = events.send(EngineEvent::Error {
            session: None,
            message: error.to_string(),
        });
    }
}

async fn next_status(events: &mut Option<mpsc::Receiver<CommandEvent>>) -> Option<CommandEvent> {
    match events {
        Some(events) => events.recv().await,
        None => std::future::pending().await,
    }
}

async fn dispatch_result(
    engine: &Arc<Mutex<AndroidEngine>>,
    result: Result<EngineOutput, EngineError>,
    session: SessionId,
    routes: &Routes,
    events: &mpsc::UnboundedSender<EngineEvent>,
) {
    match result {
        Ok(output) => {
            if !output.outgoing.is_empty() {
                let sender = lock(routes).get(&session).cloned();
                if let Some(sender) = sender {
                    if sender.send(output.outgoing).await.is_err() {
                        lock(routes).remove(&session);
                    }
                }
            }
            emit_output(events, output.events);
        }
        Err(error) => {
            let _ = events.send(EngineEvent::Error {
                session: Some(session),
                message: error.to_string(),
            });
            if let Ok(output) = lock(engine).disconnect(session, false) {
                emit_output(events, output.events);
            }
            lock(routes).remove(&session);
        }
    }
}

fn spawn_stream<S>(
    id: SessionId,
    stream: S,
    routes: Routes,
    network: mpsc::Sender<(SessionId, Bytes)>,
    commands: mpsc::Sender<ActorCommand>,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (reader_tx, mut reader_rx) = mpsc::channel(64);
    let (writer_tx, writer_rx) = mpsc::channel(64);
    lock(&routes).insert(id, writer_tx);
    tokio::spawn(async move {
        let forward = tokio::spawn(async move {
            while let Some(data) = reader_rx.recv().await {
                if network.send((id, data)).await.is_err() {
                    break;
                }
            }
        });
        let result = pump(stream, reader_tx, writer_rx).await;
        let _ = forward.await;
        let _ = match result {
            Ok(()) => commands.try_send(ActorCommand::NetworkClosed(id)),
            Err(error) => commands.try_send(ActorCommand::TransportError(id, error.to_string())),
        };
    });
}

fn emit_output(events: &mpsc::UnboundedSender<EngineEvent>, output: Vec<EngineEvent>) {
    for event in output {
        let _ = events.send(event);
    }
}

fn next_session_id() -> SessionId {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
