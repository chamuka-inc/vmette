use thiserror::Error;

/// All errors surfaced by [`crate::run`] and the snapshot helpers.
#[derive(Debug, Error)]
pub enum Error {
    #[error("config invalid: {0}")]
    InvalidConfig(String),

    #[error("VM start failed: {0}")]
    StartFailed(String),

    #[error("snapshot restore failed: {0}")]
    RestoreFailed(String),

    #[error("snapshot save failed: {0}")]
    SaveFailed(String),

    #[error("snapshot/restore is not supported on this architecture (Apple Silicon only)")]
    SnapshotUnsupported,

    #[error("timeout after {0}s")]
    Timeout(u32),

    #[error("vsock unavailable: {0}")]
    Vsock(String),

    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
}
