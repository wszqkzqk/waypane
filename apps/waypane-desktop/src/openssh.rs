#![cfg(unix)]

use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use waypane_transport::{
    LocalListener, RemoteSocket, RemoteWaypipe, MAX_REMOTE_SOCKET_BYTES, REMOTE_DIRECTORY_COMMAND,
};
use winit::event_loop::EventLoopProxy;

use crate::session::{SessionEvent, SessionTarget};

const LOCAL_RUNTIME_ROOT: &str = "/tmp";
const LOCAL_SOCKET_NAME: &str = "waypipe.sock";
const LOG_FILE_NAME: &str = "waypane-desktop.log";
const MAX_REMOTE_STDERR_BYTES: usize = 4096;
const PROCESS_STOP_TIMEOUT: Duration = Duration::from_secs(2);

static RUNTIME_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
pub struct OpenSshOptions {
    pub destination: String,
    pub port: Option<u16>,
    pub identity: Option<PathBuf>,
    pub known_hosts: Option<PathBuf>,
    pub accept_new_host_key: bool,
    pub remote: RemoteWaypipe,
}

impl OpenSshOptions {
    fn common_args(&self) -> Vec<OsString> {
        let mut args = Vec::new();
        if let Some(port) = self.port {
            args.extend([OsString::from("-p"), OsString::from(port.to_string())]);
        }
        if let Some(identity) = &self.identity {
            args.extend([
                OsString::from("-i"),
                identity.as_os_str().to_owned(),
                OsString::from("-o"),
                OsString::from("IdentitiesOnly=yes"),
            ]);
        }
        if let Some(known_hosts) = &self.known_hosts {
            args.extend([
                OsString::from("-o"),
                joined_os_value("UserKnownHostsFile=", known_hosts.as_os_str()),
            ]);
        }
        if self.accept_new_host_key {
            args.extend([
                OsString::from("-o"),
                OsString::from("StrictHostKeyChecking=accept-new"),
            ]);
        }
        args
    }

    fn discovery_command(&self) -> Command {
        let mut command = Command::new("ssh");
        command
            .args(self.common_args())
            .arg("--")
            .arg(&self.destination)
            .arg(REMOTE_DIRECTORY_COMMAND)
            .stdin(Stdio::inherit())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn shell_command(&self, remote_socket: &str, local_socket: &Path) -> Result<Command, String> {
        let remote_command = self
            .remote
            .login_shell_command_line(remote_socket)
            .map_err(|error| error.to_string())?;
        let mut linkage = OsString::from(remote_socket);
        linkage.push(":");
        linkage.push(local_socket.as_os_str());

        let mut command = Command::new("ssh");
        command
            .arg("-t")
            .arg("-o")
            .arg("ExitOnForwardFailure=yes")
            .arg("-R")
            .arg(linkage)
            .args(self.common_args())
            .arg("--")
            .arg(&self.destination)
            .arg(remote_command)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        Ok(command)
    }

    fn remote_socket_path(&self) -> Result<String, String> {
        match &self.remote.socket {
            RemoteSocket::Exact(path) => Ok(path.clone()),
            RemoteSocket::Auto { .. } => {
                let output = self.discovery_command().output().map_err(|error| {
                    format!("could not run system OpenSSH to discover a remote directory: {error}")
                })?;
                if !output.status.success() {
                    let stderr = bounded_tail(&output.stderr, MAX_REMOTE_STDERR_BYTES);
                    return Err(format!(
                        "system OpenSSH could not discover a writable remote temporary directory ({}): {}",
                        output.status,
                        if stderr.is_empty() { "no stderr output" } else { &stderr }
                    ));
                }
                let directory = String::from_utf8(output.stdout).map_err(|_| {
                    "remote temporary directory path is not valid UTF-8; set --remote-socket"
                        .to_string()
                })?;
                let directory = directory.trim_end_matches(['\r', '\n']);
                self.remote.socket_path_in(directory).map_err(|error| {
                    format!(
                        "invalid remote temporary directory discovered through OpenSSH; set --remote-socket: {error}"
                    )
                })
            }
        }
    }
}

pub struct InteractiveSession {
    runtime: Option<RuntimeDirectory>,
    child: Option<OpenSshChild>,
    socket_path: PathBuf,
    log_path: PathBuf,
}

impl Drop for InteractiveSession {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            child.stop();
        }
        let keep_log = fs::metadata(&self.log_path)
            .map(|metadata| metadata.len() > 0)
            .unwrap_or(false);
        let _ = fs::remove_file(&self.socket_path);
        if let Some(runtime) = self.runtime.take() {
            if keep_log {
                let kept = runtime.keep();
                eprintln!("Waypane desktop log: {}", kept.display());
            }
        }
    }
}

pub fn start(
    options: OpenSshOptions,
    proxy: EventLoopProxy<SessionEvent>,
) -> Result<(SessionTarget, InteractiveSession), String> {
    let runtime = RuntimeDirectory::create().map_err(|error| {
        format!(
            "could not create private OpenSSH runtime directory in {LOCAL_RUNTIME_ROOT}: {error}"
        )
    })?;
    let log_path = runtime.log_path();
    initialize_logger(&log_path)?;
    let listener = LocalListener::bind(&runtime.socket_path)
        .map_err(|error| format!("could not bind local OpenSSH listener: {error}"))?;
    let remote_socket = options.remote_socket_path()?;
    let command = options.shell_command(&remote_socket, &runtime.socket_path)?;
    let child = OpenSshChild::spawn(command, proxy)?;
    let socket_path = runtime.socket_path.clone();
    Ok((
        SessionTarget::Bound(listener),
        InteractiveSession {
            runtime: Some(runtime),
            child: Some(child),
            socket_path,
            log_path,
        },
    ))
}

fn initialize_logger(log_path: &Path) -> Result<(), String> {
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(log_path)
        .map_err(|error| {
            format!(
                "could not create interactive log {}: {error}",
                log_path.display()
            )
        })?;
    let cloned = file
        .try_clone()
        .map_err(|error| format!("could not clone interactive log handle: {error}"))?;
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("error"))
        .target(env_logger::Target::Pipe(Box::new(cloned)))
        .try_init()
        .map_err(|error| format!("could not initialize desktop logging: {error}"))
}

struct RuntimeDirectory {
    path: PathBuf,
    socket_path: PathBuf,
    cleanup: bool,
}

impl RuntimeDirectory {
    fn create() -> io::Result<Self> {
        for _ in 0..16 {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let sequence = RUNTIME_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = Path::new(LOCAL_RUNTIME_ROOT).join(format!(
                "waypane-desktop-{:x}-{nonce:x}-{sequence:x}",
                std::process::id()
            ));
            match fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => {
                    let socket_path = path.join(LOCAL_SOCKET_NAME);
                    if socket_path.as_os_str().as_bytes().len() > MAX_REMOTE_SOCKET_BYTES {
                        let _ = fs::remove_dir(&path);
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!(
                                "local socket path exceeds {MAX_REMOTE_SOCKET_BYTES} bytes: {}",
                                socket_path.display()
                            ),
                        ));
                    }
                    return Ok(Self {
                        path,
                        socket_path,
                        cleanup: true,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a unique local runtime directory",
        ))
    }

    fn log_path(&self) -> PathBuf {
        self.path.join(LOG_FILE_NAME)
    }

    fn keep(mut self) -> PathBuf {
        self.cleanup = false;
        self.path.clone()
    }
}

impl Drop for RuntimeDirectory {
    fn drop(&mut self) {
        if self.cleanup {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

struct ReapOnDrop(Option<Child>);

impl ReapOnDrop {
    fn wait(mut self) -> io::Result<ExitStatus> {
        let result = self
            .0
            .as_mut()
            .expect("child is present until waited")
            .wait();
        drop(self.0.take());
        result
    }
}

impl Drop for ReapOnDrop {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

struct OpenSshChild {
    pid: u32,
    status: mpsc::Receiver<io::Result<ExitStatus>>,
    stopping: Arc<AtomicBool>,
}

impl OpenSshChild {
    fn spawn(command: Command, proxy: EventLoopProxy<SessionEvent>) -> Result<Self, String> {
        Self::spawn_inner(command, Some(proxy))
    }

    fn spawn_inner(
        mut command: Command,
        proxy: Option<EventLoopProxy<SessionEvent>>,
    ) -> Result<Self, String> {
        let child = command
            .spawn()
            .map_err(|error| format!("could not start system OpenSSH (ssh): {error}"))?;
        let pid = child.id();
        let reaper = ReapOnDrop(Some(child));
        let stopping = Arc::new(AtomicBool::new(false));
        let notify = stopping.clone();
        let (sender, receiver) = mpsc::channel();
        thread::Builder::new()
            .name("waypane-openssh-wait".to_string())
            .spawn(move || {
                let result = reaper.wait();
                if !notify.load(Ordering::Acquire) {
                    let (message, failed) = match &result {
                        Ok(status) if status.success() => {
                            (format!("system OpenSSH exited normally ({status})"), false)
                        }
                        Ok(status) => (
                            format!("system OpenSSH exited unsuccessfully ({status})"),
                            true,
                        ),
                        Err(error) => (format!("failed waiting for system OpenSSH: {error}"), true),
                    };
                    if let Some(proxy) = &proxy {
                        let _ = proxy.send_event(SessionEvent::TransportClosed { message, failed });
                    }
                }
                let _ = sender.send(result);
            })
            .map_err(|error| format!("could not start OpenSSH wait thread: {error}"))?;
        Ok(Self {
            pid,
            status: receiver,
            stopping,
        })
    }

    fn stop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        send_signal(self.pid, libc::SIGHUP);
        match self.status.recv_timeout(PROCESS_STOP_TIMEOUT) {
            Ok(_) => return,
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        send_signal(self.pid, libc::SIGKILL);
        let _ = self.status.recv_timeout(PROCESS_STOP_TIMEOUT);
    }
}

fn send_signal(pid: u32, signal: i32) {
    let Ok(pid) = i32::try_from(pid) else {
        return;
    };
    let result = unsafe { libc::kill(pid, signal) };
    if result != 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            log::debug!("failed to send signal {signal} to OpenSSH process {pid}: {error}");
        }
    }
}

fn joined_os_value(prefix: &str, value: &OsStr) -> OsString {
    let mut result = OsString::from(prefix);
    result.push(value);
    result
}

fn bounded_tail(bytes: &[u8], limit: usize) -> String {
    let start = bytes.len().saturating_sub(limit);
    String::from_utf8_lossy(&bytes[start..]).trim().to_owned()
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use waypipe_client::Compression;

    fn options(destination: &str) -> OpenSshOptions {
        OpenSshOptions {
            destination: destination.to_string(),
            port: None,
            identity: None,
            known_hosts: None,
            accept_new_host_key: false,
            remote: RemoteWaypipe::login_shell(Compression::Lz4),
        }
    }

    fn arguments(command: &Command) -> Vec<String> {
        command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn preserves_ssh_config_host_port_and_credentials_for_discovery() {
        let command = options("workstation").discovery_command();
        assert_eq!(command.get_program(), "ssh");
        assert_eq!(
            arguments(&command),
            ["--", "workstation", REMOTE_DIRECTORY_COMMAND]
        );
    }

    #[test]
    fn maps_explicit_options_and_builds_pty_forwarding_command() {
        let mut options = options("alice@example.test");
        options.port = Some(2200);
        options.identity = Some(PathBuf::from("/tmp/Key With Space"));
        options.known_hosts = Some(PathBuf::from("/tmp/known hosts"));
        options.accept_new_host_key = true;
        options.remote = RemoteWaypipe::login_shell(Compression::Zstd)
            .with_remote_socket("/tmp/remote.sock")
            .unwrap();

        let command = options
            .shell_command("/tmp/remote.sock", Path::new("/tmp/client.sock"))
            .unwrap();
        assert_eq!(
            arguments(&command),
            [
                "-t",
                "-o",
                "ExitOnForwardFailure=yes",
                "-R",
                "/tmp/remote.sock:/tmp/client.sock",
                "-p",
                "2200",
                "-i",
                "/tmp/Key With Space",
                "-o",
                "IdentitiesOnly=yes",
                "-o",
                "UserKnownHostsFile=/tmp/known hosts",
                "-o",
                "StrictHostKeyChecking=accept-new",
                "--",
                "alice@example.test",
                "env XDG_RUNTIME_DIR=/tmp waypipe --no-gpu --compress zstd --socket /tmp/remote.sock --login-shell --unlink-socket server",
            ]
        );
    }

    #[test]
    fn runtime_directory_is_private_cleaned_up_and_preserved_only_when_kept() {
        let runtime = RuntimeDirectory::create().unwrap();
        let path = runtime.path.clone();
        assert!(path.is_dir());
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert!(runtime.socket_path.as_os_str().as_bytes().len() <= MAX_REMOTE_SOCKET_BYTES);
        drop(runtime);
        assert!(!path.exists());

        let runtime = RuntimeDirectory::create().unwrap();
        let path = runtime.keep();
        assert!(path.exists());
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn stopping_child_hangs_it_up_and_reaps_it_before_the_kill_fallback() {
        let mut command = Command::new("sleep");
        command.arg("30");
        let mut child = OpenSshChild::spawn_inner(command, None).unwrap();
        let pid = i32::try_from(child.pid).unwrap();
        let started = std::time::Instant::now();
        child.stop();
        assert!(started.elapsed() < PROCESS_STOP_TIMEOUT);

        let result = unsafe { libc::kill(pid, 0) };
        assert_eq!(result, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    }

    #[test]
    fn bounded_stderr_tail_handles_oversized_output() {
        let output = vec![b'x'; MAX_REMOTE_STDERR_BYTES + 32];
        assert_eq!(
            bounded_tail(&output, MAX_REMOTE_STDERR_BYTES).len(),
            MAX_REMOTE_STDERR_BYTES
        );
        assert_eq!(bounded_tail(b"error\n", MAX_REMOTE_STDERR_BYTES), "error");
    }
}
