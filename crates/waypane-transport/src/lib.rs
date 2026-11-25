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

mod error;
mod pump;
mod remote;
mod ssh;

pub use error::{Error, Result};
pub use pump::pump;
pub use remote::{
    shell_quote, RemoteSocket, RemoteWaypipe, MAX_REMOTE_SOCKET_BYTES, REMOTE_DIRECTORY_COMMAND,
};
#[cfg(unix)]
pub use ssh::LocalListener;
pub use ssh::{
    Authentication, CommandEvent, HostKeyChannel, HostKeyDecision, HostKeyPolicy, HostKeyPrompt,
    SshConfig, SshSession,
};
