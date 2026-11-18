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
        let root = PathBuf::from("/tmp").join(format!("wpti-{:x}-{nonce:x}", std::process::id()));
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

fn serve(mut stream: UnixStream) -> Result<(), String> {
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .map_err(|error| error.to_string())?;
    let started = Instant::now();
    let mut connection = Connection::new(ClientConfig {
        preferred_compression: Compression::Lz4,
        ..ClientConfig::default()
    });
    let mut focused = false;
    let mut window_id = None;
    let mut sent_ime = false;
    let mut bytes = [0u8; 65536];

    loop {
        if started.elapsed() > Duration::from_secs(20) {
            return Err("text-input test timed out".to_string());
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
                CoreEvent::WindowCreated(info) if !focused => {
                    connection
                        .compositor_mut()
                        .keyboard_focus(Some(info.id))
                        .map_err(|error| error.to_string())?;
                    window_id = Some(info.id);
                    focused = true;
                }
                CoreEvent::TextInputState { enabled: true, .. } if !sent_ime => {
                    let focused_window =
                        window_id.ok_or_else(|| "no focused window".to_string())?;
                    let preedit = connection
                        .compositor_mut()
                        .set_preedit_text(focused_window, Some("拼音"), 0, 6)
                        .map_err(|error| error.to_string())?;
                    if preedit != 1 {
                        return Err(format!("preedit was sent to {preedit} targets"));
                    }
                    let committed = connection
                        .compositor_mut()
                        .commit_text(focused_window, "你好，Waypane 🚀")
                        .map_err(|error| error.to_string())?;
                    if committed != 1 {
                        return Err(format!("commit was sent to {committed} targets"));
                    }
                    sent_ime = true;
                }
                CoreEvent::Closed => {
                    if focused && sent_ime {
                        return Ok(());
                    }
                    return Err("closed before text input completed".to_string());
                }
                _ => {}
            }
        }
        let outgoing = connection.flush().map_err(|error| error.to_string())?;
        if !outgoing.is_empty() {
            stream
                .write_all(&outgoing)
                .map_err(|error| error.to_string())?;
            stream.flush().map_err(|error| error.to_string())?;
        }
    }
}

#[test]
#[ignore = "requires an upstream Waypipe binary and Python 3"]
fn forwards_unicode_preedit_and_commit_with_commit_count_serials() {
    let Some(waypipe) = std::env::var_os("WAYPIPE_TEST_BINARY") else {
        return;
    };
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/text_input_app.py");
    let runtime = TempRuntime::new();
    let listener = std::os::unix::net::UnixListener::bind(runtime.channel()).unwrap();
    let (tx, rx) = mpsc::channel();
    let peer = thread::spawn(move || {
        let result = listener
            .accept()
            .map_err(|error| error.to_string())
            .and_then(|(stream, _)| serve(stream));
        let _ = tx.send(result);
    });

    let mut child = Command::new(waypipe)
        .arg("--no-gpu")
        .arg("--compress")
        .arg("lz4")
        .arg("--socket")
        .arg(runtime.channel())
        .arg("--display")
        .arg("waypane-text-input-test")
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
    if result.is_err() {
        let _ = child.kill();
    }
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
