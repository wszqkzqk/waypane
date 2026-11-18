mod compositor;
mod connection;
mod error;
pub mod protocol;
mod state;
mod types;
#[cfg(not(unix))]
mod wayland_ffi;

#[cfg(not(unix))]
mod wayland_backend {
    pub mod protocol {
        pub use crate::wayland_ffi::*;
    }
}

#[cfg(unix)]
pub use wayland_backend;

pub use compositor::Compositor;
pub use connection::{Connection, CoreBatch};
pub use error::{Error, Result};
pub use types::*;
