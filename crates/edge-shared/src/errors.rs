//! Common error type for edge crates.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum EdgeError {
    #[error("session not found: {0}")]
    SessionNotFound(String),

    #[error("invalid token: {0}")]
    InvalidToken(String),

    #[error("token revoked: jti={0}")]
    TokenRevoked(String),

    #[error("region mismatch: token={token}, edge={edge}")]
    RegionMismatch { token: String, edge: String },

    #[error("viewer cap reached for session {0}")]
    ViewerCapReached(String),

    #[error("port pool exhausted")]
    PortPoolExhausted,

    #[error("upstream unreachable: {0}")]
    UpstreamUnreachable(String),

    #[error("config error: {0}")]
    Config(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serde error: {0}")]
    Serde(#[from] serde_json::Error),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type EdgeResult<T> = Result<T, EdgeError>;
