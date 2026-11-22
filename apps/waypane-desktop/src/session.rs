use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

use bytes::Bytes;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
#[cfg(unix)]
use waypane_transport::LocalListener;
use waypane_transport::{pump, CommandEvent, RemoteWaypipe, SshConfig, SshSession};
use winit::event_loop::EventLoopProxy;

pub type SessionId = u64;

static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
pub enum SessionEvent {
    Connected(SessionId),
    Data(SessionId, Bytes),
    Disconnected(SessionId),
    Output(String),
    TransportClosed {
        message: String,
        failed: bool,
    },
    Error {
        session: Option<SessionId>,
        message: String,
    },
}

enum SessionCommand {
    Send(SessionId, Bytes),
    Close(SessionId),
}

pub enum SessionTarget {
    Local(PathBuf),
    #[cfg(unix)]
    Bound(LocalListener),
    Ssh {
        config: Box<SshConfig>,
        remote: Box<RemoteWaypipe>,
    },
}

type Routes = Arc<Mutex<HashMap<SessionId, mpsc::Sender<Bytes>>>>;

/// Whether closing every forwarded session and window ends the client (explicit commands), or
/// leaves it serving the login shell until its SSH transport closes (interactive mode).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitWhenIdle {
    Yes,
    No,
}

#[derive(Clone)]
pub struct SessionManager {
    commands: mpsc::Sender<SessionCommand>,
}

impl SessionManager {
    pub fn start(
        target: SessionTarget,
        proxy: EventLoopProxy<SessionEvent>,
        exit_when_idle: ExitWhenIdle,
    ) -> Self {
        let (commands, receiver) = mpsc::channel(64);
        thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("failed to create Tokio runtime");
            runtime.block_on(run(target, receiver, proxy, exit_when_idle));
        });
        Self { commands }
    }

    pub fn send(&self, session: SessionId, data: Bytes) -> bool {
        data.is_empty()
            || self
                .commands
                .try_send(SessionCommand::Send(session, data))
                .is_ok()
    }

    pub fn close(&self, session: SessionId) {
        let _ = self.commands.try_send(SessionCommand::Close(session));
    }
}

async fn run(
    target: SessionTarget,
    mut commands: mpsc::Receiver<SessionCommand>,
    proxy: EventLoopProxy<SessionEvent>,
    exit_when_idle: ExitWhenIdle,
) {
    let routes: Routes = Arc::new(Mutex::new(HashMap::new()));
    let router = routes.clone();
    let (shutdown, local_shutdown) = tokio::sync::watch::channel(());
    tokio::spawn(async move {
        let _shutdown = shutdown;
        while let Some(command) = commands.recv().await {
            match command {
                SessionCommand::Send(session, data) => {
                    let sender = lock_routes(&router).get(&session).cloned();
                    if let Some(sender) = sender {
                        if sender.send(data).await.is_err() {
                            lock_routes(&router).remove(&session);
                        }
                    }
                }
                SessionCommand::Close(session) => {
                    lock_routes(&router).remove(&session);
                }
            }
        }
    });

    match target {
        #[cfg(unix)]
        SessionTarget::Local(path) => match LocalListener::bind(path) {
            Ok(listener) => {
                run_local(
                    listener,
                    routes.clone(),
                    proxy.clone(),
                    exit_when_idle,
                    local_shutdown,
                )
                .await
            }
            Err(error) => send_proxy(&proxy, error_event(None, error)),
        },
        #[cfg(not(unix))]
        SessionTarget::Local(path) => {
            let _ = path;
            send_proxy(
                &proxy,
                error_event(None, "--socket is only supported on Unix hosts; use --ssh"),
            );
        }
        #[cfg(unix)]
        SessionTarget::Bound(listener) => {
            run_local(
                listener,
                routes.clone(),
                proxy.clone(),
                exit_when_idle,
                local_shutdown,
            )
            .await
        }
        SessionTarget::Ssh { config, remote } => {
            match SshSession::connect(*config, &remote).await {
                Ok(mut session) => {
                    if let Some(mut events) = session.take_events() {
                        let proxy = proxy.clone();
                        tokio::spawn(async move {
                            while let Some(event) = events.recv().await {
                                let message = match event {
                                    CommandEvent::Stdout(data) | CommandEvent::Stderr(data) => {
                                        String::from_utf8_lossy(&data).into_owned()
                                    }
                                    CommandEvent::ExitStatus { status, stderr } => match stderr {
                                        Some(stderr) if !stderr.is_empty() => format!(
                                            "remote command exited: {status:?}: {}",
                                            String::from_utf8_lossy(&stderr)
                                        ),
                                        _ => format!("remote command exited: {status:?}"),
                                    },
                                };
                                send_proxy(&proxy, SessionEvent::Output(message));
                            }
                        });
                    }
                    while let Some(stream) = session.accept().await {
                        let id = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
                        spawn_stream(id, stream, routes.clone(), proxy.clone());
                    }
                    if let Err(error) = session.shutdown().await {
                        send_proxy(&proxy, error_event(None, error));
                    }
                }
                Err(error) => send_proxy(&proxy, error_event(None, error)),
            }
        }
    }

    while !lock_routes(&routes).is_empty() {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[cfg(unix)]
async fn run_local(
    mut listener: LocalListener,
    routes: Routes,
    proxy: EventLoopProxy<SessionEvent>,
    exit_when_idle: ExitWhenIdle,
    mut shutdown: tokio::sync::watch::Receiver<()>,
) {
    let mut connected = false;
    loop {
        if connected && exit_when_idle == ExitWhenIdle::Yes && lock_routes(&routes).is_empty() {
            break;
        }
        let result = if connected && exit_when_idle == ExitWhenIdle::Yes {
            tokio::select! {
                result = listener.accept() => Some(result),
                _ = shutdown.changed() => None,
                _ = tokio::time::sleep(Duration::from_millis(50)) => continue,
            }
        } else {
            tokio::select! {
                result = listener.accept() => Some(result),
                _ = shutdown.changed() => None,
            }
        };
        let Some(result) = result else { break };
        match result {
            Ok(stream) => {
                connected = true;
                let id = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
                spawn_stream(id, stream, routes.clone(), proxy.clone());
            }
            Err(error) => {
                send_proxy(&proxy, error_event(None, error));
                break;
            }
        }
    }
}

fn spawn_stream<S>(id: SessionId, stream: S, routes: Routes, proxy: EventLoopProxy<SessionEvent>)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (reader_tx, mut reader_rx) = mpsc::channel(64);
    let (writer_tx, writer_rx) = mpsc::channel(64);
    lock_routes(&routes).insert(id, writer_tx);
    tokio::spawn(async move {
        let reader_proxy = proxy.clone();
        let reader = tokio::spawn(async move {
            while let Some(data) = reader_rx.recv().await {
                if reader_proxy
                    .send_event(SessionEvent::Data(id, data))
                    .is_err()
                {
                    break;
                }
            }
        });

        send_proxy(&proxy, SessionEvent::Connected(id));
        let result = pump(stream, reader_tx, writer_rx).await;
        let _ = reader.await;
        if let Err(error) = result {
            send_proxy(
                &proxy,
                SessionEvent::Error {
                    session: Some(id),
                    message: error.to_string(),
                },
            );
        }
        send_proxy(&proxy, SessionEvent::Disconnected(id));
        lock_routes(&routes).remove(&id);
    });
}

fn lock_routes(routes: &Routes) -> MutexGuard<'_, HashMap<SessionId, mpsc::Sender<Bytes>>> {
    routes
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn error_event(session: Option<SessionId>, error: impl ToString) -> SessionEvent {
    SessionEvent::Error {
        session,
        message: error.to_string(),
    }
}

fn send_proxy(proxy: &EventLoopProxy<SessionEvent>, event: SessionEvent) {
    let _ = proxy.send_event(event);
}
