use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use waypipe_client::Compression;

use crate::error::{Error, Result};

static SOCKET_COUNTER: AtomicU64 = AtomicU64::new(1);

pub const MAX_REMOTE_SOCKET_BYTES: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteSocket {
    Auto { file_name: String },
    Exact(String),
}

#[derive(Debug, Clone)]
pub struct RemoteWaypipe {
    pub socket: RemoteSocket,
    pub binary: String,
    pub compression: Compression,
    pub command: Vec<String>,
}

impl RemoteWaypipe {
    pub fn new(compression: Compression, command: Vec<String>) -> Result<Self> {
        if command.is_empty() {
            return Err(Error::InvalidConfiguration(
                "remote command must not be empty".to_string(),
            ));
        }
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let sequence = SOCKET_COUNTER.fetch_add(1, Ordering::Relaxed);
        Ok(Self {
            socket: RemoteSocket::Auto {
                file_name: format!("wp-{:x}-{nonce:x}-{sequence:x}.sock", std::process::id()),
            },
            binary: "waypipe".to_string(),
            compression,
            command,
        })
    }

    pub fn with_binary(mut self, binary: impl Into<String>) -> Self {
        self.binary = binary.into();
        self
    }

    pub fn with_remote_socket(mut self, path: impl Into<String>) -> Result<Self> {
        let path = path.into();
        validate_remote_socket(&path)?;
        self.socket = RemoteSocket::Exact(path);
        Ok(self)
    }

    pub fn socket_path_in(&self, directory: &str) -> Result<String> {
        match &self.socket {
            RemoteSocket::Exact(path) => Ok(path.clone()),
            RemoteSocket::Auto { file_name } => {
                if !directory.starts_with('/') || directory.contains(['\0', '\r', '\n']) {
                    return Err(Error::InvalidConfiguration(format!(
                        "remote socket directory is not a clean absolute path: {directory:?}"
                    )));
                }
                let directory = directory.trim_end_matches('/');
                let path = if directory.is_empty() {
                    format!("/{file_name}")
                } else {
                    format!("{directory}/{file_name}")
                };
                validate_remote_socket(&path)?;
                Ok(path)
            }
        }
    }

    pub fn command_line(&self, socket_path: &str) -> String {
        let compression = match self.compression {
            Compression::None => "none",
            Compression::Lz4 => "lz4",
            Compression::Zstd => "zstd",
        };
        let mut parts = vec![
            shell_quote(&self.binary),
            "--no-gpu".to_string(),
            "--compress".to_string(),
            compression.to_string(),
            "--socket".to_string(),
            shell_quote(socket_path),
            "server".to_string(),
            "--".to_string(),
        ];
        parts.extend(self.command.iter().map(|argument| shell_quote(argument)));
        parts.join(" ")
    }
}

pub fn validate_remote_socket(path: &str) -> Result<()> {
    if !path.starts_with('/')
        || path.len() > MAX_REMOTE_SOCKET_BYTES
        || path.contains(['\0', '\r', '\n'])
    {
        return Err(Error::InvalidConfiguration(format!(
            "remote socket must be an absolute path of at most {MAX_REMOTE_SOCKET_BYTES} bytes without control characters: {path:?}"
        )));
    }
    Ok(())
}

pub fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&byte))
    {
        return value.to_owned();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_quoted_headless_command() {
        let mut remote = RemoteWaypipe::new(
            Compression::Lz4,
            vec![
                "env".into(),
                "GTK_THEME=Adwaita:dark".into(),
                "my app;$(false)".into(),
            ],
        )
        .unwrap()
        .with_remote_socket("/tmp/waypane.sock")
        .unwrap();
        remote.binary = "waypipe".into();
        assert_eq!(
            remote.command_line("/tmp/waypane.sock"),
            "waypipe --no-gpu --compress lz4 --socket /tmp/waypane.sock server -- env GTK_THEME=Adwaita:dark 'my app;$(false)'"
        );
    }

    #[test]
    fn joins_unique_socket_name_to_termux_tmpdir() {
        let remote = RemoteWaypipe::new(Compression::Zstd, vec!["foot".into()]).unwrap();
        let path = remote
            .socket_path_in("/data/data/com.termux/files/usr/tmp")
            .unwrap();
        assert!(path.starts_with("/data/data/com.termux/files/usr/tmp/wp-"));
        assert!(path.ends_with(".sock"));
        assert!(path.len() <= MAX_REMOTE_SOCKET_BYTES);
    }

    #[test]
    fn validates_socket_paths_and_quotes_arguments() {
        let remote = RemoteWaypipe::new(Compression::None, vec!["true".into()]).unwrap();
        assert!(remote.clone().with_remote_socket("relative.sock").is_err());
        assert!(remote
            .clone()
            .with_remote_socket(format!("/tmp/{}", "x".repeat(101)))
            .is_err());
        assert!(remote
            .with_remote_socket("/data/data/com.termux/files/usr/tmp/waypipe")
            .is_ok());
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }
}
