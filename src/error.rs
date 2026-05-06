//! Error type for `mdk-recovery`.

#[derive(Debug, thiserror::Error)]
pub enum RecoveryError {
    #[error("not yet implemented: {0}")]
    NotImplemented(&'static str),
}

pub type Result<T> = std::result::Result<T, RecoveryError>;
