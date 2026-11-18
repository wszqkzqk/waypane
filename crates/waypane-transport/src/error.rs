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

    #[error("SSH authentication failed for user {0}")]
    AuthenticationFailed(String),

    #[error("no SSH authentication method configured; add --identity or --password-env")]
    MissingAuthentication,

    #[error("invalid remote configuration: {0}")]
    InvalidConfiguration(String),
}

pub type Result<T> = std::result::Result<T, Error>;
