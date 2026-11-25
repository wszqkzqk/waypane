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

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("SSH error: {0}")]
    Ssh(#[from] russh::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("host key verification failed for {host}:{port}: {reason}")]
    HostKeyVerification {
        host: String,
        port: u16,
        reason: String,
    },

    #[error("SSH authentication failed for {user}@{host}:{port} using {method}: {details}")]
    AuthenticationFailed {
        host: String,
        port: u16,
        user: String,
        method: String,
        details: String,
    },

    #[error("no SSH authentication method configured; add --identity or --password-env")]
    MissingAuthentication,

    #[error("invalid remote configuration: {0}")]
    InvalidConfiguration(String),
}

pub type Result<T> = std::result::Result<T, Error>;
