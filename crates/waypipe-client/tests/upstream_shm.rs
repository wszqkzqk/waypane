#![cfg(unix)]

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use waypipe_client::{
    Client, ClientConfig, ClientEvent, Compression, ProtocolMessage, Rid, WaylandMessage,
};

const KEYMAP: &[u8] = b"xkb_keymap { xkb_keycodes { include \"evdev\" }; };";

struct TempRuntime {
    root: PathBuf,
    channel: PathBuf,
}

impl TempRuntime {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = PathBuf::from("/tmp").join(format!("wp-{:x}-{nonce:x}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let channel = root.join("transport.sock");
        Self { root, channel }
    }
}

impl Drop for TempRuntime {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Interface {
    Registry,
    Shm,
    Seat,
    Keyboard,
    Callback,
}

fn message(object_id: u32, opcode: u16, payload: &[u8]) -> Vec<u8> {
    let mut result = Vec::with_capacity(8 + payload.len());
    result.extend_from_slice(&object_id.to_le_bytes());
    result.extend_from_slice(
        &((((8 + payload.len()) as u32) << 16) | u32::from(opcode)).to_le_bytes(),
    );
    result.extend_from_slice(payload);
    result
}

fn string(value: &str) -> Vec<u8> {
    let mut encoded = Vec::new();
    encoded.extend_from_slice(&((value.len() + 1) as u32).to_le_bytes());
    encoded.extend_from_slice(value.as_bytes());
    encoded.push(0);
    while !encoded.len().is_multiple_of(4) {
        encoded.push(0);
    }
    encoded
}

fn global(registry: u32, name: u32, interface: &str, version: u32) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&name.to_le_bytes());
    payload.extend_from_slice(&string(interface));
    payload.extend_from_slice(&version.to_le_bytes());
    message(registry, 0, &payload)
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn bind_id(payload: &[u8]) -> u32 {
    u32::from_le_bytes(payload[payload.len() - 4..].try_into().unwrap())
}

fn bind_interface(payload: &[u8]) -> String {
    let len = read_u32(payload, 4) as usize;
    String::from_utf8_lossy(&payload[8..8 + len - 1]).into_owned()
}

fn push_plain(output: &mut Vec<ProtocolMessage>, bytes: Vec<u8>) {
    output.push(ProtocolMessage::new(Bytes::from(bytes)).unwrap());
}

fn handle_messages(
    client: &mut Client,
    messages: &[WaylandMessage],
    objects: &mut HashMap<u32, Interface>,
) -> Result<Bytes, String> {
    let mut output = Vec::new();
    for request in messages {
        let object_id = request.object_id();
        let payload = request.payload();
        match (objects.get(&object_id).copied(), request.opcode()) {
            (None, 0) if object_id == 1 => {
                let callback = read_u32(payload, 0);
                objects.insert(callback, Interface::Callback);
                push_plain(&mut output, message(callback, 0, &0u32.to_le_bytes()));
                push_plain(&mut output, message(1, 1, &callback.to_le_bytes()));
            }
            (None, 1) if object_id == 1 => {
                let registry = read_u32(payload, 0);
                objects.insert(registry, Interface::Registry);
                push_plain(&mut output, global(registry, 1, "wl_compositor", 5));
                push_plain(&mut output, global(registry, 2, "wl_shm", 1));
                push_plain(&mut output, global(registry, 3, "wl_seat", 7));
            }
            (Some(Interface::Registry), 0) => {
                let id = bind_id(payload);
                match bind_interface(payload).as_str() {
                    "wl_shm" => {
                        objects.insert(id, Interface::Shm);
                        push_plain(&mut output, message(id, 0, &0u32.to_le_bytes()));
                        push_plain(&mut output, message(id, 0, &1u32.to_le_bytes()));
                    }
                    "wl_seat" => {
                        objects.insert(id, Interface::Seat);
                        push_plain(&mut output, message(id, 0, &3u32.to_le_bytes()));
                        push_plain(&mut output, message(id, 1, &string("waypane")));
                    }
                    interface => return Err(format!("unexpected bind for {interface}")),
                }
            }
            (Some(Interface::Seat), 1) => {
                let keyboard = read_u32(payload, 0);
                objects.insert(keyboard, Interface::Keyboard);
                let rid = client
                    .create_readonly_file(Arc::from(KEYMAP))
                    .map_err(|error| error.to_string())?;
                let mut keymap = Vec::new();
                keymap.extend_from_slice(&1u32.to_le_bytes());
                keymap.extend_from_slice(&(KEYMAP.len() as u32).to_le_bytes());
                output.push(
                    ProtocolMessage::with_fds(
                        Bytes::from(message(keyboard, 0, &keymap)),
                        vec![rid],
                    )
                    .map_err(|error| error.to_string())?,
                );
            }
            _ => {}
        }
    }
    client
        .encode_messages(&output)
        .map_err(|error| error.to_string())
}

fn serve(mut stream: UnixStream, compression: Compression) -> Result<(), String> {
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .map_err(|error| error.to_string())?;
    let started = Instant::now();
    let mut client = Client::new(ClientConfig {
        preferred_compression: compression,
        ..ClientConfig::default()
    });
    let mut objects = HashMap::new();
    let mut pool: Option<Rid> = None;
    let mut updates = 0usize;
    let mut buffer = [0u8; 8192];

    loop {
        if started.elapsed() > Duration::from_secs(20) {
            return Err("timed out waiting for waypipe traffic".to_string());
        }
        let read = match stream.read(&mut buffer) {
            Ok(0) => return Err("waypipe closed the transport before Close".to_string()),
            Ok(read) => read,
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
        let batch = client
            .receive(&buffer[..read])
            .map_err(|error| error.to_string())?;
        if !batch.outgoing.is_empty() {
            stream
                .write_all(&batch.outgoing)
                .map_err(|error| error.to_string())?;
        }

        for event in batch.events {
            match event {
                ClientEvent::Closed => return Ok(()),
                ClientEvent::Protocol(messages) => {
                    if messages.len() == 1
                        && messages[0].object_id() == 1
                        && messages[0].opcode() == 0
                        && read_u32(messages[0].payload(), 0) == 10
                    {
                        let Some(rid) = pool else {
                            return Err(
                                "final sync arrived before a shared-memory pool".to_string()
                            );
                        };
                        let mut expected: Vec<u8> = (0u8..=255).collect();
                        expected[4..12].fill(0xaa);
                        let actual = client
                            .resources()
                            .data(rid)
                            .ok_or_else(|| "pool disappeared".to_string())?;
                        if actual != expected {
                            return Err(format!(
                                "SHM mismatch: len {}, bytes 0..16 {:?}, expected {:?}",
                                actual.len(),
                                &actual[..16],
                                &expected[..16]
                            ));
                        }
                        if updates == 0 {
                            return Err("no shared-memory updates were received".to_string());
                        }
                    }
                    let frame = handle_messages(&mut client, &messages, &mut objects)?;
                    if !frame.is_empty() {
                        stream
                            .write_all(&frame)
                            .map_err(|error| error.to_string())?;
                        stream.flush().map_err(|error| error.to_string())?;
                    }
                }
                ClientEvent::ShmOpened { rid, size } => {
                    if size != 256 {
                        return Err(format!("unexpected pool size {size}"));
                    }
                    pool = Some(rid);
                }
                ClientEvent::ShmUpdated { .. } => updates += 1,
                _ => {}
            }
        }
    }
}

fn run_replication(waypipe: &Path, compression: &str) -> Result<(), String> {
    let compression = match compression {
        "none" => Compression::None,
        "lz4" => Compression::Lz4,
        "zstd" => Compression::Zstd,
        other => return Err(format!("unknown compression {other}")),
    };
    let runtime = TempRuntime::new();
    let listener = std::os::unix::net::UnixListener::bind(&runtime.channel)
        .map_err(|error| error.to_string())?;
    let (result_tx, result_rx) = mpsc::channel();

    let peer = thread::spawn(move || {
        let result = listener
            .accept()
            .map_err(|error| error.to_string())
            .and_then(|(stream, _)| serve(stream, compression));
        let _ = result_tx.send(result);
    });

    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/remote_shm_app.py");
    let mut child = Command::new(waypipe)
        .arg("--no-gpu")
        .arg("--compress")
        .arg(compression_name(compression))
        .arg("--socket")
        .arg(&runtime.channel)
        .arg("--display")
        .arg("waypane-test")
        .arg("server")
        .arg("--")
        .arg("python3")
        .arg(script)
        .env("XDG_RUNTIME_DIR", &runtime.root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| error.to_string())?;

    let result = match result_rx.recv_timeout(Duration::from_secs(25)) {
        Ok(result) => result,
        Err(error) => {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            return Err(format!(
                "waypipe peer timed out: {error}\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ));
        }
    };

    let output = child
        .wait_with_output()
        .map_err(|error| error.to_string())?;
    peer.join().unwrap();
    if let Err(error) = result {
        return Err(format!(
            "{error}\nwaypipe stdout: {}\nwaypipe stderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    if !output.status.success() {
        return Err(format!(
            "waypipe exited with {:?}\nstdout: {}\nstderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(())
}

fn compression_name(compression: Compression) -> &'static str {
    match compression {
        Compression::None => "none",
        Compression::Lz4 => "lz4",
        Compression::Zstd => "zstd",
    }
}

#[test]
#[ignore = "requires an upstream Waypipe binary built for this host"]
fn replicates_shm_and_keymap_with_upstream_waypipe_server() {
    let Some(waypipe) = std::env::var_os("WAYPIPE_TEST_BINARY") else {
        return;
    };
    let compressions =
        std::env::var("WAYPIPE_TEST_COMPRESSIONS").unwrap_or_else(|_| "none".to_string());
    for compression in compressions
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        run_replication(Path::new(&waypipe), compression)
            .unwrap_or_else(|error| panic!("{compression} replication failed: {error}"));
    }
}
