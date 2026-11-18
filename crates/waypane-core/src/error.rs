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
