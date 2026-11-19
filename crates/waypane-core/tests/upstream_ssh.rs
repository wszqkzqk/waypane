#![cfg(unix)]

use std::fs;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use waypane_core::{Connection, CoreEvent};
use waypane_transport::{
    Authentication, CommandEvent, HostKeyChannel, HostKeyDecision, HostKeyPolicy, RemoteWaypipe,
    SshConfig, SshSession,
};
use waypipe_client::{ClientConfig, Compression};

const KEY_PASSPHRASE: &str = "waypane integration test passphrase";

struct TestSshd {
    root: PathBuf,
    child: tokio::process::Child,
    user: String,
    user_key: PathBuf,
    rsa_key: PathBuf,
    port: u16,
    termux_tmp: PathBuf,
}

impl TestSshd {
    fn start(sshd: &Path) -> Result<Self, String> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_nanos();
        let root =
            PathBuf::from("/tmp").join(format!("wpc-ssh-{:x}-{nonce:x}", std::process::id()));
        fs::create_dir_all(&root).map_err(|error| error.to_string())?;
        let host_key = root.join("ssh_host_ed25519_key");
        let user_key = root.join("id_ed25519");
        let rsa_key = root.join("id_rsa");
        generate_key(&host_key, "ed25519", None, "")?;
        generate_key(&user_key, "ed25519", None, KEY_PASSPHRASE)?;
        generate_key(&rsa_key, "rsa", Some("2048"), KEY_PASSPHRASE)?;
        let authorized_keys = [
            fs::read_to_string(root.join("id_ed25519.pub")).map_err(|error| error.to_string())?,
            fs::read_to_string(root.join("id_rsa.pub")).map_err(|error| error.to_string())?,
        ]
        .concat();
        fs::write(root.join("authorized_keys"), authorized_keys)
            .map_err(|error| error.to_string())?;
        let termux_tmp = PathBuf::from("/data/data/com.termux/files/usr/tmp");
        let probe = termux_tmp.join(format!("waypane-write-probe-{}", std::process::id()));
        fs::write(&probe, b"").map_err(|error| {
            format!(
                "Termux test TMPDIR {} must exist and be writable: {error}",
                termux_tmp.display()
            )
        })?;
        let _ = fs::remove_file(probe);

        let listener =
            std::net::TcpListener::bind(("127.0.0.1", 0)).map_err(|error| error.to_string())?;
        let port = listener
            .local_addr()
            .map_err(|error| error.to_string())?
            .port();
        drop(listener);
        let user = std::env::var("USER")
            .or_else(|_| std::env::var("LOGNAME"))
            .map_err(|_| "USER or LOGNAME must be set for the sshd test".to_string())?;
        let config = root.join("sshd_config");
        fs::write(
            &config,
            format!(
                "ListenAddress 127.0.0.1\nPort {port}\nHostKey {}\nAuthorizedKeysFile {}\nPidFile {}\nStrictModes no\nUsePAM no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nPubkeyAuthentication yes\nPubkeyAcceptedAlgorithms ssh-ed25519,rsa-sha2-512,rsa-sha2-256\nAllowAgentForwarding no\nAllowTcpForwarding remote\nX11Forwarding no\nAllowStreamLocalForwarding yes\nSetEnv TMPDIR={}\nPrintMotd no\nLogLevel VERBOSE\n",
                host_key.display(),
                root.join("authorized_keys").display(),
                root.join("sshd.pid").display(),
                termux_tmp.display(),
            ),
        )
        .map_err(|error| error.to_string())?;
        let log = fs::File::create(root.join("sshd.log")).map_err(|error| error.to_string())?;
        let mut child = tokio::process::Command::new(sshd)
            .arg("-D")
            .arg("-e")
            .arg("-f")
            .arg(&config)
            .stdout(Stdio::null())
            .stderr(Stdio::from(log))
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| error.to_string())?;
        for _ in 0..100 {
            if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
                return Err(format!(
                    "sshd exited with {status}: {}",
                    fs::read_to_string(root.join("sshd.log")).unwrap_or_default()
                ));
            }
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return Ok(Self {
                    root,
                    child,
                    user,
                    user_key,
                    rsa_key,
                    port,
                    termux_tmp,
                });
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Err(format!(
            "timed out starting sshd: {}",
            fs::read_to_string(root.join("sshd.log")).unwrap_or_default()
        ))
    }

    async fn stop(&mut self) {
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
        let _ = fs::remove_dir_all(&self.root);
    }
}

impl Drop for TestSshd {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn generate_key(
    path: &Path,
    kind: &str,
    bits: Option<&str>,
    passphrase: &str,
) -> Result<(), String> {
    let mut command = Command::new("ssh-keygen");
    command
        .arg("-q")
        .arg("-t")
        .arg(kind)
        .arg("-N")
        .arg(passphrase)
        .arg("-f")
        .arg(path);
    if let Some(bits) = bits {
        command.arg("-b").arg(bits);
    }
    let status = command
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|error| error.to_string())?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| format!("ssh-keygen failed for {}", path.display()))
}

async fn write<S>(stream: &mut S, data: &[u8]) -> Result<(), String>
where
    S: AsyncWrite + Unpin,
{
    if data.is_empty() {
        return Ok(());
    }
    stream
        .write_all(data)
        .await
        .map_err(|error| error.to_string())
}

async fn drive_wayland<S>(mut stream: S) -> Result<(), String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut connection = Connection::new(ClientConfig {
        preferred_compression: Compression::Lz4,
        ..ClientConfig::default()
    });
    let mut bytes = [0u8; 65536];
    let mut frames = 0;
    loop {
        let count = stream
            .read(&mut bytes)
            .await
            .map_err(|error| error.to_string())?;
        if count == 0 {
            return Err("Waypipe transport closed before Close".to_string());
        }
        let batch = connection
            .receive(&bytes[..count])
            .map_err(|error| error.to_string())?;
        write(&mut stream, &batch.outgoing).await?;
        for event in batch.events {
            match event {
                CoreEvent::Frame(frame) => {
                    let buffer = frame.buffer.as_ref().ok_or("frame without buffer")?;
                    let data = connection
                        .client()
                        .resources()
                        .data(buffer.rid)
                        .ok_or("missing SHM data")?;
                    let mut expected = vec![0u8; 4096];
                    for (index, byte) in expected.iter_mut().enumerate() {
                        *byte = index as u8;
                    }
                    if frames > 0 {
                        expected[4..12].fill(0xaa);
                        if !buffer
                            .damage
                            .iter()
                            .any(|rect| rect.y == 0 && rect.x <= 3 && rect.x + rect.width >= 2)
                        {
                            return Err(format!(
                                "incorrect second-frame damage: {:?}",
                                buffer.damage
                            ));
                        }
                    }
                    if data != expected.as_slice() {
                        return Err(format!("SHM pixel mismatch on frame {frames}"));
                    }
                    let outgoing = connection
                        .complete_frame(frame.id, 100 + frames as u32)
                        .map_err(|error| error.to_string())?;
                    write(&mut stream, &outgoing).await?;
                    if frames == 0 {
                        connection
                            .compositor_mut()
                            .keyboard_focus(Some(frame.window_id))
                            .map_err(|error| error.to_string())?;
                        connection
                            .compositor_mut()
                            .pointer_motion(frame.window_id, 1.0, 1.0, 101)
                            .map_err(|error| error.to_string())?;
                        connection
                            .compositor_mut()
                            .key(42, true, 102)
                            .map_err(|error| error.to_string())?;
                        let outgoing = connection.flush().map_err(|error| error.to_string())?;
                        write(&mut stream, &outgoing).await?;
                    }
                    frames += 1;
                }
                CoreEvent::Closed => {
                    if frames != 2 {
                        return Err(format!("closed after {frames} frames instead of 2"));
                    }
                    return Ok(());
                }
                _ => {}
            }
        }
    }
}

#[tokio::test]
#[ignore = "requires upstream Waypipe, sshd, and ssh-keygen"]
async fn connects_waypipe_through_sshd_with_tofu_and_termux_tmpdir() {
    let Some(waypipe) = std::env::var_os("WAYPIPE_TEST_BINARY") else {
        return;
    };
    let Some(sshd) = std::env::var_os("WAYPIPE_SSHD") else {
        return;
    };
    let waypipe = PathBuf::from(waypipe);
    let sshd = PathBuf::from(sshd);
    let mut server = match TestSshd::start(&sshd) {
        Ok(server) => server,
        Err(error) => panic!("failed to start test sshd: {error}"),
    };
    let known_hosts = server.root.join("known_hosts");
    let remote = RemoteWaypipe::new(
        Compression::Lz4,
        vec![
            "python3".to_string(),
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/toplevel_app.py")
                .to_string_lossy()
                .into_owned(),
        ],
    )
    .unwrap()
    .with_binary(waypipe.to_string_lossy().into_owned());
    let config = SshConfig {
        host: "127.0.0.1".to_string(),
        port: server.port,
        user: server.user.clone(),
        authentication: Authentication::PublicKey {
            path: server.user_key.clone(),
            passphrase: Some(KEY_PASSPHRASE.to_string()),
        },
        known_hosts: Some(known_hosts.clone()),
        host_key_policy: HostKeyPolicy::Check,
    };

    let error = match SshSession::connect(config.clone(), &remote).await {
        Ok(_) => panic!("strict host key checking unexpectedly accepted an unknown key"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("host key is unknown"), "{error}");
    let (host_keys, mut prompts) = HostKeyChannel::new();
    let interactive = SshConfig {
        host_key_policy: HostKeyPolicy::Interactive(host_keys.clone()),
        ..config
    };
    let (session, prompt) = tokio::join!(SshSession::connect(interactive, &remote), async {
        let prompt = prompts.recv().await.expect("missing host key prompt");
        assert!(!prompt.changed);
        assert!(host_keys.decide(HostKeyDecision::TrustAndSave));
        prompt
    },);
    let mut session = session.unwrap_or_else(|error| {
        panic!(
            "SSH connection failed after accepting host key: {error}; sshd log: {}",
            fs::read_to_string(server.root.join("sshd.log")).unwrap_or_default()
        )
    });
    assert_eq!(prompt.host, "127.0.0.1");
    assert_eq!(prompt.port, server.port);
    assert!(prompt.fingerprint.starts_with("SHA256:"));
    let saved = fs::read_to_string(&known_hosts).unwrap();
    assert!(saved.contains(&format!("[127.0.0.1]:{} {}", server.port, prompt.algorithm)));
    let expected_prefix = format!("{}/wp-", server.termux_tmp.display());
    assert!(
        session.remote_socket().starts_with(&expected_prefix),
        "socket {} is outside {expected_prefix}",
        session.remote_socket()
    );

    let mut events = session.take_events().unwrap();
    let mut remote_stdout = String::new();
    let mut remote_stderr = String::new();
    let stream = loop {
        tokio::select! {
            stream = session.accept() => break stream,
            event = events.recv() => match event {
                Some(CommandEvent::Stdout(data)) => remote_stdout.push_str(&String::from_utf8_lossy(&data)),
                Some(CommandEvent::Stderr(data)) => remote_stderr.push_str(&String::from_utf8_lossy(&data)),
                Some(CommandEvent::ExitStatus(status)) => panic!(
                    "Waypipe exited with {status:?} before connecting; stdout: {remote_stdout}; stderr: {remote_stderr}; sshd log: {}",
                    fs::read_to_string(server.root.join("sshd.log")).unwrap_or_default()
                ),
                None => panic!("remote command channel closed before Waypipe connected; stdout: {remote_stdout}; stderr: {remote_stderr}"),
            },
            _ = tokio::time::sleep(Duration::from_secs(15)) => panic!(
                "timed out waiting for Waypipe streamlocal connection; stdout: {remote_stdout}; stderr: {remote_stderr}; sshd log: {}",
                fs::read_to_string(server.root.join("sshd.log")).unwrap_or_default()
            ),
        }
    }
    .expect("Waypipe streamlocal channel closed");
    let result = tokio::time::timeout(Duration::from_secs(20), drive_wayland(stream))
        .await
        .expect("timed out driving Wayland over SSH");
    let mut exit_status = None;
    while let Ok(Some(event)) = tokio::time::timeout(Duration::from_secs(3), events.recv()).await {
        match event {
            CommandEvent::Stdout(data) => remote_stdout.push_str(&String::from_utf8_lossy(&data)),
            CommandEvent::Stderr(data) => remote_stderr.push_str(&String::from_utf8_lossy(&data)),
            CommandEvent::ExitStatus(status) => {
                exit_status = status;
                break;
            }
        }
    }
    let _ = session.shutdown().await;

    let rsa_remote = RemoteWaypipe::new(Compression::Lz4, vec!["true".to_string()])
        .unwrap()
        .with_binary(waypipe.to_string_lossy().into_owned());
    let rsa_config = SshConfig {
        host: "127.0.0.1".to_string(),
        port: server.port,
        user: server.user.clone(),
        authentication: Authentication::PublicKey {
            path: server.rsa_key.clone(),
            passphrase: Some(KEY_PASSPHRASE.to_string()),
        },
        known_hosts: Some(known_hosts.clone()),
        host_key_policy: HostKeyPolicy::Check,
    };
    let rsa_session = SshSession::connect(rsa_config, &rsa_remote)
        .await
        .unwrap_or_else(|error| {
            panic!(
                "RSA-SHA2 authentication failed: {error}; sshd log: {}",
                fs::read_to_string(server.root.join("sshd.log")).unwrap_or_default()
            )
        });
    assert!(rsa_session.remote_socket().starts_with(&expected_prefix));
    let _ = rsa_session.shutdown().await;
    server.stop().await;
    if let Err(error) = result {
        panic!(
            "Wayland over SSH failed: {error}; stdout: {remote_stdout}; stderr: {remote_stderr}"
        );
    }
    assert_eq!(exit_status, Some(0), "stderr: {remote_stderr}");
    assert!(
        remote_stdout.contains("completed"),
        "stdout: {remote_stdout}; stderr: {remote_stderr}"
    );
}
