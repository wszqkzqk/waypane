use std::fmt;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use russh::client::{self, ChannelOpenHandle, Handle, Handler, Msg, Session};
use russh::keys::{
    check_known_hosts, check_known_hosts_path, load_secret_key, HashAlg, PrivateKeyWithHashAlg,
    PublicKey, PublicKeyOrCertificate,
};
use russh::{Channel, ChannelMsg, ChannelStream, Disconnect};
use tokio::sync::{mpsc, oneshot};

use crate::error::{Error, Result};
use crate::remote::{RemoteSocket, RemoteWaypipe, MAX_REMOTE_SOCKET_BYTES};

const REMOTE_DIRECTORY_COMMAND: &str = "if [ -n \"${TMPDIR:-}\" ] && [ -d \"$TMPDIR\" ] && [ -w \"$TMPDIR\" ]; then printf %s \"$TMPDIR\"; elif [ -n \"${XDG_RUNTIME_DIR:-}\" ] && [ -d \"$XDG_RUNTIME_DIR\" ] && [ -w \"$XDG_RUNTIME_DIR\" ]; then printf %s \"$XDG_RUNTIME_DIR\"; elif [ -d /tmp ] && [ -w /tmp ]; then printf %s /tmp; else exit 3; fi";
const MAX_KNOWN_HOSTS_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone)]
pub enum Authentication {
    Auto,
    Password(String),
    PublicKey {
        path: PathBuf,
        passphrase: Option<String>,
    },
}

#[derive(Clone, Default)]
pub enum HostKeyPolicy {
    #[default]
    Check,
    AcceptNew,
    Interactive(HostKeyChannel),
}

impl fmt::Debug for HostKeyPolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Check => formatter.write_str("Check"),
            Self::AcceptNew => formatter.write_str("AcceptNew"),
            Self::Interactive(channel) => formatter
                .debug_tuple("Interactive")
                .field(&channel.is_pending())
                .finish(),
        }
    }
}

#[derive(Clone)]
pub struct HostKeyChannel {
    prompts: mpsc::UnboundedSender<HostKeyPrompt>,
    pending: Arc<Mutex<Option<oneshot::Sender<HostKeyDecision>>>>,
}

impl HostKeyChannel {
    pub fn new() -> (Self, mpsc::UnboundedReceiver<HostKeyPrompt>) {
        let (prompts, receiver) = mpsc::unbounded_channel();
        (
            Self {
                prompts,
                pending: Arc::new(Mutex::new(None)),
            },
            receiver,
        )
    }

    pub fn decide(&self, decision: HostKeyDecision) -> bool {
        lock(&self.pending)
            .take()
            .is_some_and(|sender| sender.send(decision).is_ok())
    }

    pub fn cancel(&self) {
        lock(&self.pending).take();
    }

    pub fn is_pending(&self) -> bool {
        lock(&self.pending).is_some()
    }

    fn register(&self, prompt: HostKeyPrompt) -> Result<oneshot::Receiver<HostKeyDecision>> {
        let (sender, receiver) = oneshot::channel();
        {
            let mut pending = lock(&self.pending);
            if pending.is_some() {
                return Err(Error::InvalidConfiguration(
                    "an SSH host key decision is already pending".to_string(),
                ));
            }
            *pending = Some(sender);
        }
        if self.prompts.send(prompt).is_err() {
            self.cancel();
            return Err(Error::InvalidConfiguration(
                "SSH host key prompt channel is closed".to_string(),
            ));
        }
        Ok(receiver)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostKeyPrompt {
    pub host: String,
    pub port: u16,
    pub algorithm: String,
    pub fingerprint: String,
    pub changed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostKeyDecision {
    Reject,
    TrustAndSave,
}

#[derive(Debug, Clone)]
pub struct SshConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub authentication: Authentication,
    pub known_hosts: Option<PathBuf>,
    pub host_key_policy: HostKeyPolicy,
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
    host_key_policy: HostKeyPolicy,
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
        verify_host_key(
            &self.host,
            self.port,
            self.known_hosts.as_deref(),
            &self.host_key_policy,
            key,
        )
        .await
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

async fn verify_host_key(
    host: &str,
    port: u16,
    known_hosts: Option<&Path>,
    policy: &HostKeyPolicy,
    key: &PublicKey,
) -> Result<bool> {
    let checked = match known_hosts {
        Some(path) => check_known_hosts_path(host, port, key, path),
        None => check_known_hosts(host, port, key),
    };
    match checked {
        Ok(true) => Ok(true),
        Ok(false) => match policy {
            HostKeyPolicy::Check => Err(host_key_error(host, port, key, "host key is unknown")),
            HostKeyPolicy::AcceptNew => {
                learn_host_key(host, port, key, known_hosts)?;
                Ok(true)
            }
            HostKeyPolicy::Interactive(channel) => {
                let prompt = HostKeyPrompt {
                    host: host.to_string(),
                    port,
                    algorithm: key.algorithm().as_str().to_string(),
                    fingerprint: key.fingerprint(HashAlg::Sha256).to_string(),
                    changed: false,
                };
                confirm_and_learn(host, port, known_hosts, channel, key, prompt, false).await
            }
        },
        Err(russh::keys::Error::KeyChanged { line }) => match policy {
            HostKeyPolicy::Check | HostKeyPolicy::AcceptNew => Err(host_key_error(
                host,
                port,
                key,
                &format!("host key has changed (known_hosts line {line})"),
            )),
            HostKeyPolicy::Interactive(channel) => {
                let prompt = HostKeyPrompt {
                    host: host.to_string(),
                    port,
                    algorithm: key.algorithm().as_str().to_string(),
                    fingerprint: key.fingerprint(HashAlg::Sha256).to_string(),
                    changed: true,
                };
                confirm_and_learn(host, port, known_hosts, channel, key, prompt, true).await
            }
        },
        Err(error) => Err(host_key_error(
            host,
            port,
            key,
            &format!("failed to read known_hosts: {error}"),
        )),
    }
}

async fn confirm_and_learn(
    host: &str,
    port: u16,
    known_hosts: Option<&Path>,
    channel: &HostKeyChannel,
    key: &PublicKey,
    prompt: HostKeyPrompt,
    replace: bool,
) -> Result<bool> {
    let Some(path) = known_hosts else {
        return Err(Error::InvalidConfiguration(
            "interactive host key verification requires a known_hosts path".to_string(),
        ));
    };
    let decision = channel
        .register(prompt)?
        .await
        .map_err(|_| host_key_error(host, port, key, "host key confirmation was cancelled"))?;
    match decision {
        HostKeyDecision::Reject => Err(host_key_error(
            host,
            port,
            key,
            "host key was rejected by the user",
        )),
        HostKeyDecision::TrustAndSave => {
            if replace {
                replace_known_host_key(host, port, key, path)?;
            } else {
                learn_host_key(host, port, key, Some(path))?;
            }
            Ok(true)
        }
    }
}

fn host_key_error(host: &str, port: u16, key: &PublicKey, reason: &str) -> Error {
    Error::HostKeyVerification {
        host: host.to_string(),
        port,
        reason: format!(
            "{reason}: {} {}",
            key.algorithm().as_str(),
            key.fingerprint(HashAlg::Sha256)
        ),
    }
}

fn learn_host_key(
    host: &str,
    port: u16,
    key: &PublicKey,
    known_hosts: Option<&Path>,
) -> Result<()> {
    let path = match known_hosts {
        Some(path) => path.to_owned(),
        None => default_known_hosts_path().map_err(|reason| {
            host_key_error(
                host,
                port,
                key,
                &format!("failed to locate known_hosts: {reason}"),
            )
        })?,
    };
    let save = || -> std::io::Result<()> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder.create(parent)?;
        }
        let mut options = std::fs::OpenOptions::new();
        options.read(true).append(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let encoded = key.to_openssh().map_err(std::io::Error::other)?;
        let mut file = options.open(&path)?;
        let ends_in_newline = if file.metadata()?.len() == 0 {
            true
        } else {
            file.seek(SeekFrom::End(-1))?;
            let mut last = [0u8; 1];
            file.read_exact(&mut last)?;
            last[0] == b'\n'
        };
        file.seek(SeekFrom::End(0))?;
        if !ends_in_newline {
            file.write_all(b"\n")?;
        }
        if port == 22 {
            file.write_all(host.as_bytes())?;
        } else {
            file.write_all(format!("[{host}]:{port}").as_bytes())?;
        }
        file.write_all(b" ")?;
        file.write_all(encoded.as_bytes())?;
        file.write_all(b"\n")?;
        file.flush()
    };
    save().map_err(|error| {
        host_key_error(
            host,
            port,
            key,
            &format!("failed to save host key in {}: {error}", path.display()),
        )
    })
}

fn default_known_hosts_path() -> std::result::Result<PathBuf, String> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .ok_or_else(|| "HOME/USERPROFILE is not set".to_string())?;
    Ok(PathBuf::from(home).join(".ssh").join("known_hosts"))
}

fn replace_known_host_key(host: &str, port: u16, key: &PublicKey, path: &Path) -> Result<()> {
    let metadata = std::fs::metadata(path).map_err(|error| {
        host_key_error(
            host,
            port,
            key,
            &format!("failed to read known_hosts before replacement: {error}"),
        )
    })?;
    if metadata.len() > MAX_KNOWN_HOSTS_BYTES as u64 {
        return Err(host_key_error(
            host,
            port,
            key,
            "known_hosts exceeds the 4 MiB replacement limit",
        ));
    }
    let contents = std::fs::read_to_string(path).map_err(|error| {
        host_key_error(
            host,
            port,
            key,
            &format!("failed to read known_hosts before replacement: {error}"),
        )
    })?;
    let host_pattern = if port == 22 {
        host.to_string()
    } else {
        format!("[{host}]:{port}")
    };
    let key_algorithm = key.algorithm();
    let algorithm = key_algorithm.as_str();
    let mut replaced = false;
    let mut retained = String::with_capacity(contents.len());
    for line in contents.lines() {
        let mut fields = line.split_whitespace();
        let matches = fields.next() == Some(host_pattern.as_str())
            && fields.next() == Some(algorithm)
            && fields.next().is_some_and(|key| !key.is_empty());
        if matches {
            replaced = true;
        } else {
            retained.push_str(line);
            retained.push('\n');
        }
    }
    if !replaced {
        return Err(host_key_error(
            host,
            port,
            key,
            "cannot automatically replace a hashed or malformed known_hosts entry",
        ));
    }
    std::fs::write(path, retained).map_err(|error| {
        host_key_error(
            host,
            port,
            key,
            &format!("failed to replace known_hosts entry: {error}"),
        )
    })?;
    learn_host_key(host, port, key, Some(path))
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
            host_key_policy: config.host_key_policy.clone(),
            incoming: incoming_tx,
        };
        let client_config = Arc::new(client::Config {
            nodelay: true,
            ..Default::default()
        });
        let mut handle =
            client::connect(client_config, (config.host.as_str(), config.port), handler).await?;
        authenticate(&mut handle, &config).await?;

        let socket_path = match &remote.socket {
            RemoteSocket::Exact(path) => path.clone(),
            RemoteSocket::Auto { .. } => {
                let directory =
                    exec_capture(&mut handle, REMOTE_DIRECTORY_COMMAND, MAX_REMOTE_SOCKET_BYTES + 1)
                        .await
                        .map_err(|error| {
                            Error::InvalidConfiguration(format!(
                                "could not discover a writable remote temporary directory; set an explicit remote socket path: {error}"
                            ))
                        })?;
                remote.socket_path_in(&directory)?
            }
        };
        handle.streamlocal_forward(socket_path.clone()).await?;
        let channel = handle.channel_open_session().await?;
        channel
            .exec(true, remote.command_line(&socket_path))
            .await?;
        let events = Some(watch_remote_command(channel));

        Ok(Self {
            handle,
            incoming,
            events,
            remote_socket: socket_path,
        })
    }

    pub async fn accept(&mut self) -> Option<ChannelStream<Msg>> {
        self.incoming.recv().await
    }

    pub fn take_events(&mut self) -> Option<mpsc::Receiver<CommandEvent>> {
        self.events.take()
    }

    pub fn remote_socket(&self) -> &str {
        &self.remote_socket
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

async fn exec_capture(
    handle: &mut Handle<SshHandler>,
    command: &str,
    output_limit: usize,
) -> Result<String> {
    let mut channel = handle.channel_open_session().await?;
    channel.exec(true, command).await?;
    let mut output = Vec::new();
    let mut status = None;
    while let Some(message) = channel.wait().await {
        match message {
            ChannelMsg::Data { data } => {
                if output.len() + data.len() > output_limit {
                    return Err(Error::InvalidConfiguration(
                        "remote environment probe returned too much data".to_string(),
                    ));
                }
                output.extend_from_slice(&data);
            }
            ChannelMsg::ExitStatus { exit_status } => status = Some(exit_status),
            ChannelMsg::Close | ChannelMsg::Eof => break,
            _ => {}
        }
    }
    if status.is_some_and(|status| status != 0) {
        return Err(Error::InvalidConfiguration(format!(
            "remote environment probe exited with status {status:?}"
        )));
    }
    String::from_utf8(output).map_err(|error| {
        Error::InvalidConfiguration(format!("remote temporary directory is not UTF-8: {error}"))
    })
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
pub struct LocalListener {
    listener: tokio::net::UnixListener,
    path: PathBuf,
}

#[cfg(unix)]
impl LocalListener {
    pub fn bind(path: impl Into<PathBuf>) -> Result<Self> {
        use std::os::unix::fs::FileTypeExt;

        let path = path.into();
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_socket() => {
                std::fs::remove_file(&path)?;
            }
            Ok(_) => {
                return Err(Error::Io(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    format!("refusing to replace non-socket path {}", path.display()),
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let listener = tokio::net::UnixListener::bind(&path)?;
        Ok(Self { listener, path })
    }

    pub async fn accept(&self) -> Result<tokio::net::UnixStream> {
        Ok(self.listener.accept().await?.0)
    }
}

#[cfg(unix)]
impl Drop for LocalListener {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn key(last: char) -> PublicKey {
        PublicKey::from_openssh(&format!(
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILagOJFgwaMNhBWQINinKOXmqS4Gh5NgxgriXwdOoIN{last}"
        ))
        .unwrap()
    }

    fn temp_file(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("waypane-ssh-{name}-{}-{nonce}", std::process::id()))
    }

    async fn receive_and_decide(
        receiver: &mut mpsc::UnboundedReceiver<HostKeyPrompt>,
        channel: &HostKeyChannel,
    ) -> HostKeyPrompt {
        let prompt = receiver.recv().await.expect("expected a host key prompt");
        assert!(channel.decide(HostKeyDecision::TrustAndSave));
        prompt
    }

    #[tokio::test]
    async fn accept_new_saves_unknown_keys_but_rejects_changed_keys() {
        let path = temp_file("known-hosts");
        let original = key('J');
        assert!(verify_host_key(
            "127.0.0.1",
            8022,
            Some(&path),
            &HostKeyPolicy::AcceptNew,
            &original,
        )
        .await
        .unwrap());
        assert!(check_known_hosts_path("127.0.0.1", 8022, &original, &path).unwrap());
        let error = verify_host_key(
            "127.0.0.1",
            8022,
            Some(&path),
            &HostKeyPolicy::AcceptNew,
            &key('K'),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("host key has changed"));
        assert!(check_known_hosts_path("127.0.0.1", 8022, &original, &path).unwrap());
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn interactive_tofu_saves_unknown_and_replaces_confirmed_changed_keys() {
        let path = temp_file("known-hosts-interactive");
        let (channel, mut prompts) = HostKeyChannel::new();
        let policy = HostKeyPolicy::Interactive(channel.clone());
        let original = key('J');
        let (result, prompt) = tokio::join!(
            verify_host_key("example.test", 22, Some(&path), &policy, &original),
            receive_and_decide(&mut prompts, &channel),
        );
        assert!(result.unwrap());
        assert!(!prompt.changed);
        assert_eq!(prompt.host, "example.test");
        assert_eq!(
            prompt.fingerprint,
            "SHA256:ldyiXa1JQakitNU5tErauu8DvWQ1dZ7aXu+rm7KQuog"
        );

        let replacement = key('K');
        let (result, prompt) = tokio::join!(
            verify_host_key("example.test", 22, Some(&path), &policy, &replacement),
            receive_and_decide(&mut prompts, &channel),
        );
        assert!(result.unwrap());
        assert!(prompt.changed);
        assert!(check_known_hosts_path("example.test", 22, &replacement, &path).unwrap());
        assert!(matches!(
            check_known_hosts_path("example.test", 22, &original, &path),
            Err(russh::keys::Error::KeyChanged { .. })
        ));
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn cancelling_prompt_wakes_blocked_verification() {
        let path = temp_file("known-hosts-cancel");
        let (channel, mut prompts) = HostKeyChannel::new();
        let policy = HostKeyPolicy::Interactive(channel.clone());
        let original = key('J');
        let (result, ()) = tokio::join!(
            verify_host_key("127.0.0.1", 8022, Some(&path), &policy, &original),
            async {
                prompts.recv().await.expect("expected a host key prompt");
                channel.cancel();
            },
        );
        assert!(result.unwrap_err().to_string().contains("cancelled"));
        assert!(!channel.is_pending());
    }
}
