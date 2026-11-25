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

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use waypipe_client::Compression;

use crate::error::{Error, Result};

static SOCKET_COUNTER: AtomicU64 = AtomicU64::new(1);

pub const MAX_REMOTE_SOCKET_BYTES: usize = 100;
pub const REMOTE_DIRECTORY_COMMAND: &str = "if [ -n \"${TMPDIR:-}\" ] && [ -d \"$TMPDIR\" ] && [ -w \"$TMPDIR\" ]; then printf %s \"$TMPDIR\"; elif [ -n \"${XDG_RUNTIME_DIR:-}\" ] && [ -d \"$XDG_RUNTIME_DIR\" ] && [ -w \"$XDG_RUNTIME_DIR\" ]; then printf %s \"$XDG_RUNTIME_DIR\"; elif [ -d /tmp ] && [ -w /tmp ]; then printf %s /tmp; else exit 3; fi";

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
    login_shell: bool,
}

impl RemoteWaypipe {
    pub fn new(compression: Compression, command: Vec<String>) -> Result<Self> {
        if command.is_empty() {
            return Err(Error::InvalidConfiguration(
                "remote command must not be empty".to_string(),
            ));
        }
        Ok(Self::with_unique_socket(compression, command, false))
    }

    pub fn login_shell(compression: Compression) -> Self {
        Self::with_unique_socket(compression, Vec::new(), true)
    }

    fn with_unique_socket(
        compression: Compression,
        command: Vec<String>,
        login_shell: bool,
    ) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let sequence = SOCKET_COUNTER.fetch_add(1, Ordering::Relaxed);
        Self {
            socket: RemoteSocket::Auto {
                file_name: format!("wp-{:x}-{nonce:x}-{sequence:x}.sock", std::process::id()),
            },
            binary: "waypipe".to_string(),
            compression,
            command,
            login_shell,
        }
    }

    pub fn is_login_shell(&self) -> bool {
        self.login_shell
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

    fn command_prefix(&self, socket_path: &str) -> Vec<String> {
        let compression = match self.compression {
            Compression::None => "none",
            Compression::Lz4 => "lz4",
            Compression::Zstd => "zstd",
        };
        let runtime_dir = socket_path.rsplit_once('/').map_or("/", |(directory, _)| {
            if directory.is_empty() {
                "/"
            } else {
                directory
            }
        });
        vec![
            "env".to_string(),
            format!("XDG_RUNTIME_DIR={}", shell_quote(runtime_dir)),
            shell_quote(&self.binary),
            "--no-gpu".to_string(),
            "--compress".to_string(),
            compression.to_string(),
            "--socket".to_string(),
            shell_quote(socket_path),
        ]
    }

    pub fn command_line(&self, socket_path: &str) -> String {
        let mut parts = self.command_prefix(socket_path);
        parts.push("server".to_string());
        parts.push("--".to_string());
        parts.extend(self.command.iter().map(|argument| shell_quote(argument)));
        parts.join(" ")
    }

    pub fn login_shell_command_line(&self, socket_path: &str) -> Result<String> {
        if !self.login_shell || !self.command.is_empty() {
            return Err(Error::InvalidConfiguration(
                "login-shell command requires a RemoteWaypipe::login_shell configuration"
                    .to_string(),
            ));
        }
        let mut parts = self.command_prefix(socket_path);
        parts.extend([
            "--login-shell".to_string(),
            "--unlink-socket".to_string(),
            "server".to_string(),
        ]);
        Ok(parts.join(" "))
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
            "env XDG_RUNTIME_DIR=/tmp waypipe --no-gpu --compress lz4 --socket /tmp/waypane.sock server -- env GTK_THEME=Adwaita:dark 'my app;$(false)'"
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
        assert_eq!(
            remote.command_line(&path),
            format!("env XDG_RUNTIME_DIR=/data/data/com.termux/files/usr/tmp waypipe --no-gpu --compress zstd --socket {path} server -- foot")
        );
    }

    #[test]
    fn builds_login_shell_command_separately_from_program_commands() {
        let remote = RemoteWaypipe::login_shell(Compression::Zstd)
            .with_remote_socket("/tmp/waypane.sock")
            .unwrap();
        assert!(remote.is_login_shell());
        assert_eq!(
            remote
                .login_shell_command_line("/tmp/waypane.sock")
                .unwrap(),
            "env XDG_RUNTIME_DIR=/tmp waypipe --no-gpu --compress zstd --socket /tmp/waypane.sock --login-shell --unlink-socket server"
        );

        let program = RemoteWaypipe::new(Compression::Lz4, vec!["foot".into()]).unwrap();
        assert!(program
            .login_shell_command_line("/tmp/waypane.sock")
            .is_err());
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
