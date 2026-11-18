mod error;
mod pump;
mod remote;
mod ssh;

pub use error::{Error, Result};
pub use pump::pump;
pub use remote::{shell_quote, RemoteSocket, RemoteWaypipe, MAX_REMOTE_SOCKET_BYTES};
#[cfg(unix)]
pub use ssh::LocalListener;
pub use ssh::{
    Authentication, CommandEvent, HostKeyChannel, HostKeyDecision, HostKeyPolicy, HostKeyPrompt,
    SshConfig, SshSession,
};
