use std::path::{Path, PathBuf};
use std::sync::Arc;

use russh::client::{self, ChannelOpenHandle, Handle, Handler, Msg, Session};
use russh::keys::{
    check_known_hosts, check_known_hosts_path, load_secret_key, PrivateKeyWithHashAlg,
    PublicKeyOrCertificate,
};
use russh::{Channel, ChannelMsg, ChannelStream, Disconnect};
use tokio::sync::mpsc;

use crate::error::{Error, Result};
use crate::remote::RemoteWaypipe;

#[derive(Debug, Clone)]
pub enum Authentication {
    Auto,
    Password(String),
    PublicKey {
        path: PathBuf,
        passphrase: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct SshConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub authentication: Authentication,
    pub known_hosts: Option<PathBuf>,
    pub accept_unknown_host_key: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandEvent {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    ExitStatus(Option<u32>),
}

struct SshHandler {
    host: String,
    port: u16,
    known_hosts: Option<PathBuf>,
    accept_unknown_host_key: bool,
    incoming: mpsc::Sender<ChannelStream<Msg>>,
}

impl Handler for SshHandler {
    type Error = Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKeyOrCertificate,
    ) -> Result<bool> {
        let PublicKeyOrCertificate::PublicKey { key, .. } = server_public_key else {
            return Err(Error::HostKeyVerification {
                host: self.host.clone(),
                port: self.port,
                reason: "SSH certificates are not supported; use a regular host key".to_string(),
            });
        };
        let checked = match &self.known_hosts {
            Some(path) => check_known_hosts_path(&self.host, self.port, key, path),
            None => check_known_hosts(&self.host, self.port, key),
        };
        match checked {
            Ok(true) => Ok(true),
            Ok(false) if self.accept_unknown_host_key => Ok(true),
            Ok(false) => Err(Error::HostKeyVerification {
                host: self.host.clone(),
                port: self.port,
                reason: "host key is unknown or has changed".to_string(),
            }),
            Err(_) if self.accept_unknown_host_key => Ok(true),
            Err(error) => Err(Error::HostKeyVerification {
                host: self.host.clone(),
                port: self.port,
                reason: error.to_string(),
            }),
        }
    }

    async fn server_channel_open_forwarded_streamlocal(
        &mut self,
        channel: Channel<Msg>,
        _socket_path: &str,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<()> {
        reply.accept().await;
        let _ = self.incoming.send(channel.into_stream()).await;
        Ok(())
    }
}

pub struct SshSession {
    handle: Handle<SshHandler>,
    incoming: mpsc::Receiver<ChannelStream<Msg>>,
    events: Option<mpsc::Receiver<CommandEvent>>,
    remote_socket: String,
}

impl SshSession {
    pub async fn connect(config: SshConfig, remote: &RemoteWaypipe) -> Result<Self> {
        let (incoming_tx, incoming) = mpsc::channel(16);
        let handler = SshHandler {
            host: config.host.clone(),
            port: config.port,
            known_hosts: config.known_hosts.clone(),
            accept_unknown_host_key: config.accept_unknown_host_key,
            incoming: incoming_tx,
        };
        let client_config = Arc::new(client::Config {
            nodelay: true,
            ..Default::default()
        });
        let mut handle =
            client::connect(client_config, (config.host.as_str(), config.port), handler).await?;
        authenticate(&mut handle, &config).await?;

        handle
            .streamlocal_forward(remote.socket_path.clone())
            .await?;
        let channel = handle.channel_open_session().await?;
        channel.exec(true, remote.command_line()).await?;
        let events = Some(watch_remote_command(channel));

        Ok(Self {
            handle,
            incoming,
            events,
            remote_socket: remote.socket_path.clone(),
        })
    }

    pub async fn accept(&mut self) -> Option<ChannelStream<Msg>> {
        self.incoming.recv().await
    }

    pub fn take_events(&mut self) -> Option<mpsc::Receiver<CommandEvent>> {
        self.events.take()
    }

    pub async fn shutdown(self) -> Result<()> {
        let _ = self
            .handle
            .cancel_streamlocal_forward(self.remote_socket)
            .await;
        self.handle
            .disconnect(Disconnect::ByApplication, "", "en")
            .await?;
        Ok(())
    }
}

async fn authenticate(handle: &mut Handle<SshHandler>, config: &SshConfig) -> Result<()> {
    let authenticated = match &config.authentication {
        Authentication::Password(password) => {
            if password.is_empty() {
                return Err(Error::MissingAuthentication);
            }
            handle
                .authenticate_password(&config.user, password)
                .await?
                .success()
        }
        Authentication::PublicKey { path, passphrase } => {
            authenticate_key(handle, &config.user, path, passphrase.as_deref()).await?
        }
        Authentication::Auto => {
            let mut authenticated = false;
            for path in default_key_paths() {
                if path.is_file() && authenticate_key(handle, &config.user, &path, None).await? {
                    authenticated = true;
                    break;
                }
            }
            authenticated
        }
    };
    if !authenticated {
        return Err(Error::AuthenticationFailed(config.user.clone()));
    }
    Ok(())
}

async fn authenticate_key(
    handle: &mut Handle<SshHandler>,
    user: &str,
    path: &Path,
    passphrase: Option<&str>,
) -> Result<bool> {
    let key = load_secret_key(path, passphrase).map_err(|error| {
        Error::InvalidConfiguration(format!(
            "failed to load SSH key {}: {error}",
            path.display()
        ))
    })?;
    let key = PrivateKeyWithHashAlg::new(Arc::new(key), None);
    Ok(handle.authenticate_publickey(user, key).await?.success())
}

fn default_key_paths() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) else {
        return Vec::new();
    };
    let ssh = PathBuf::from(home).join(".ssh");
    vec![ssh.join("id_ed25519"), ssh.join("id_rsa")]
}

fn watch_remote_command(mut channel: Channel<Msg>) -> mpsc::Receiver<CommandEvent> {
    let (sender, receiver) = mpsc::channel(32);
    tokio::spawn(async move {
        let mut status = None;
        while let Some(message) = channel.wait().await {
            let event = match message {
                ChannelMsg::Data { data } => CommandEvent::Stdout(data.to_vec()),
                ChannelMsg::ExtendedData { data, .. } => CommandEvent::Stderr(data.to_vec()),
                ChannelMsg::ExitStatus { exit_status } => {
                    status = Some(exit_status);
                    continue;
                }
                ChannelMsg::Close | ChannelMsg::Eof => break,
                _ => continue,
            };
            if sender.send(event).await.is_err() {
                return;
            }
        }
        let _ = sender.send(CommandEvent::ExitStatus(status)).await;
    });
    receiver
}

#[cfg(unix)]
pub async fn connect_local(path: impl AsRef<Path>) -> Result<tokio::net::UnixStream> {
    Ok(tokio::net::UnixStream::connect(path).await?)
}
