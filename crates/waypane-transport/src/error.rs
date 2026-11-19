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

    #[error("SSH {stage} timed out after {timeout:?} for {user}@{host}:{port}")]
    Timeout {
        user: String,
        host: String,
        port: u16,
        stage: &'static str,
        timeout: std::time::Duration,
    },

    #[error("invalid remote configuration: {0}")]
    InvalidConfiguration(String),
}

pub type Result<T> = std::result::Result<T, Error>;
