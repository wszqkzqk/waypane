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
    #[error(transparent)]
    Transport(#[from] waypipe_client::Error),

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("unknown object {object_id} ({interface})")]
    UnknownObject {
        object_id: u32,
        interface: &'static str,
    },

    #[error("invalid opcode {opcode} for {interface}@{object_id}")]
    InvalidOpcode {
        object_id: u32,
        interface: &'static str,
        opcode: u16,
    },

    #[error("malformed {interface}.{message}: {reason}")]
    MalformedMessage {
        interface: &'static str,
        message: &'static str,
        reason: String,
    },
}

impl Error {
    pub(crate) fn malformed(
        (interface, method): (
            &'static crate::wayland_backend::protocol::Interface,
            &'static crate::wayland_backend::protocol::MessageDesc,
        ),
        reason: impl Into<String>,
    ) -> Self {
        Self::MalformedMessage {
            interface: interface.name,
            message: method.name,
            reason: reason.into(),
        }
    }

    pub(crate) fn protocol(message: impl Into<String>) -> Self {
        Self::Protocol(message.into())
    }
}

pub type Result<T> = std::result::Result<T, Error>;
