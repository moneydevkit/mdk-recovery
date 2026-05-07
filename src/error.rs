//! Error type for `mdk-recovery`.

use bitcoin::{Amount, Network};

#[derive(Debug, thiserror::Error)]
pub enum RecoveryError {
    #[error("destination address is not valid for network {network:?}")]
    AddressNetworkMismatch { network: Network },

    #[error("total input value would overflow")]
    AmountOverflow,

    #[error("destination confirmation did not match")]
    DestinationConfirmationMismatch,

    #[error("recovery plan has no inputs to sweep")]
    EmptyInputs,

    #[error("esplora backend error: {0}")]
    Esplora(String),

    #[error("estimated fee {fee} exceeds total input value {total_in}")]
    FeeExceedsValue { fee: Amount, total_in: Amount },

    #[error("invalid mnemonic: {0}")]
    InvalidMnemonic(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("not yet implemented: {0}")]
    NotImplemented(&'static str),

    #[error("estimated output {output} is below dust threshold {dust}")]
    OutputBelowDust { output: Amount, dust: Amount },

    #[error(
        "regtest endpoint requires the MDK_RECOVERY_ESPLORA_URL environment variable to be set"
    )]
    RegtestEndpointMissing,

    #[error("destination uses a script type with no defined dust threshold")]
    UnsupportedDestinationScript,

    #[error("network {network:?} is not supported by this binary")]
    UnsupportedNetwork { network: Network },
}

pub type Result<T> = std::result::Result<T, RecoveryError>;
