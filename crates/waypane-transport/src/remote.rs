use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use waypipe_client::Compression;

use crate::error::{Error, Result};

static SOCKET_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone)]
pub struct RemoteWaypipe {
    pub socket_path: String,
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
            socket_path: format!(
                "/tmp/wp-{:x}-{nonce:x}-{sequence:x}.sock",
                std::process::id()
            ),
            binary: "waypipe".to_string(),
            compression,
            command,
        })
    }

    pub fn with_binary(mut self, binary: impl Into<String>) -> Self {
        self.binary = binary.into();
        self
    }

    pub fn command_line(&self) -> String {
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
            shell_quote(&self.socket_path),
            "server".to_string(),
            "--".to_string(),
        ];
        parts.extend(self.command.iter().map(|argument| shell_quote(argument)));
        parts.join(" ")
    }
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
        .unwrap();
        remote.socket_path = "/tmp/waypane.sock".to_string();
        assert_eq!(
            remote.command_line(),
            "waypipe --no-gpu --compress lz4 --socket /tmp/waypane.sock server -- env GTK_THEME=Adwaita:dark 'my app;$(false)'"
        );
    }

    #[test]
    fn quotes_empty_and_single_quote_arguments() {
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }
}
