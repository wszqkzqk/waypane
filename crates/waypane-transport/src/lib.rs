mod error;
mod pump;
mod remote;
mod ssh;

pub use error::{Error, Result};
pub use pump::pump;
pub use remote::{shell_quote, RemoteWaypipe};
#[cfg(unix)]
pub use ssh::connect_local;
pub use ssh::{Authentication, CommandEvent, SshConfig, SshSession};
