// Waypane: window-level Wayland forwarding over SSH.
// Copyright (C) 2026  Zhou Qiankang <wszqkzqk@qq.com>
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.
//
// SPDX-License-Identifier: GPL-3.0-or-later

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

const REMOTE_TEXT: &[u8] = b"Waypipe clipboard";

struct TempRuntime(PathBuf);

impl TempRuntime {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = PathBuf::from("/tmp").join(format!("wpcb-{:x}-{nonce:x}", std::process::id()));
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
    let mut source_selected = false;
    let mut host_set = false;
    let mut bytes = [0u8; 65536];

    loop {
        if started.elapsed() > Duration::from_secs(20) {
            return Err("clipboard test timed out".to_string());
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
                    focused = true;
                }
                CoreEvent::ClipboardChanged {
                    source: Some(source_id),
                    ..
                } => {
                    if source_id == 13 {
                        source_selected = true;
                    }
                }
                CoreEvent::ClipboardData { data, .. } => {
                    if data != REMOTE_TEXT {
                        return Err(format!("unexpected clipboard data {data:?}"));
                    }
                    connection
                        .compositor_mut()
                        .set_clipboard_text("from host")
                        .map_err(|error| error.to_string())?;
                    let _ = connection.compositor_mut().take_events();
                    host_set = true;
                }
                CoreEvent::Closed => {
                    if source_selected && host_set {
                        return Ok(());
                    }
                    return Err("connection closed before clipboard test completed".to_string());
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
fn transfers_clipboard_in_both_directions_through_waypipe_pipes() {
    let Some(waypipe) = std::env::var_os("WAYPIPE_TEST_BINARY") else {
        return;
    };
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/clipboard_app.py");
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
        .arg("--debug")
        .arg("--no-gpu")
        .arg("--compress")
        .arg("lz4")
        .arg("--socket")
        .arg(runtime.channel())
        .arg("--display")
        .arg("waypane-clipboard-test")
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
