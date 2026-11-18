#![cfg(unix)]

use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use waypane_core::{Connection, CoreEvent};
use waypipe_client::{ClientConfig, Compression};

struct TempRuntime(PathBuf);

impl TempRuntime {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = PathBuf::from("/tmp").join(format!("wpc-{:x}-{nonce:x}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }

    fn channel(&self) -> PathBuf {
        self.0.join("transport.sock")
    }
}

impl Drop for TempRuntime {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn serve(mut stream: UnixStream, compression: Compression) -> Result<(), String> {
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .map_err(|error| error.to_string())?;
    let started = Instant::now();
    let mut connection = Connection::new(ClientConfig {
        preferred_compression: compression,
        ..ClientConfig::default()
    });
    let mut frames = 0;
    let mut bytes = [0u8; 65536];

    loop {
        if started.elapsed() > Duration::from_secs(20) {
            return Err("timed out waiting for toplevel traffic".to_string());
        }
        let count = match stream.read(&mut bytes) {
            Ok(0) => return Err("transport closed before Close".to_string()),
            Ok(count) => count,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(error) => return Err(error.to_string()),
        };
        let batch = connection
            .receive(&bytes[..count])
            .map_err(|error| error.to_string())?;
        if !batch.outgoing.is_empty() {
            stream
                .write_all(&batch.outgoing)
                .map_err(|error| error.to_string())?;
        }
        for event in batch.events {
            match event {
                CoreEvent::Frame(frame) => {
                    let buffer = frame.buffer.as_ref().ok_or("frame without buffer")?;
                    let data = connection
                        .client()
                        .resources()
                        .data(buffer.rid)
                        .ok_or("missing SHM data")?;
                    let mut expected =
                        (bytes::Bytes::from((0u8..=255).collect::<Vec<_>>())).repeat(16);
                    if frames > 0 {
                        expected[4..12].fill(0xaa);
                        if !buffer
                            .damage
                            .iter()
                            .any(|rect| rect.y == 0 && rect.x <= 3 && rect.x + rect.width >= 2)
                        {
                            return Err(format!(
                                "second frame has incorrect damage: {:?}",
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
                    if !outgoing.is_empty() {
                        stream
                            .write_all(&outgoing)
                            .map_err(|error| error.to_string())?;
                    }
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
                        let input = connection.flush().map_err(|error| error.to_string())?;
                        if !input.is_empty() {
                            stream
                                .write_all(&input)
                                .map_err(|error| error.to_string())?;
                        }
                    }
                    frames += 1;
                }
                CoreEvent::WindowMetadata { title, app_id, .. } => {
                    if app_id == "waypane-test" && title != "Waypane E2E" {
                        return Err(format!("unexpected title {title}"));
                    }
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

#[test]
#[ignore = "requires an upstream Waypipe binary and Python 3"]
fn drives_xdg_toplevel_through_upstream_waypipe() {
    let Some(waypipe) = std::env::var_os("WAYPIPE_TEST_BINARY") else {
        return;
    };
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/toplevel_app.py");
    let runtime = TempRuntime::new();
    let listener = std::os::unix::net::UnixListener::bind(runtime.channel()).unwrap();
    let (tx, rx) = mpsc::channel();
    let peer = thread::spawn(move || {
        let result = listener
            .accept()
            .map_err(|error| error.to_string())
            .and_then(|(stream, _)| serve(stream, Compression::Lz4));
        let _ = tx.send(result);
    });

    let mut child = Command::new(waypipe)
        .arg("--no-gpu")
        .arg("--compress")
        .arg("lz4")
        .arg("--socket")
        .arg(runtime.channel())
        .arg("--display")
        .arg("waypane-core-test")
        .arg("server")
        .arg("--")
        .arg("python3")
        .arg(script)
        .env("XDG_RUNTIME_DIR", &runtime.0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let result = match rx.recv_timeout(Duration::from_secs(25)) {
        Ok(result) => result,
        Err(error) => {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            panic!(
                "timed out: {error}\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    };
    let output = child.wait_with_output().unwrap();
    peer.join().unwrap();
    if let Err(error) = result {
        panic!(
            "{error}\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(
        output.status.success() && String::from_utf8_lossy(&output.stdout).contains("completed"),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
