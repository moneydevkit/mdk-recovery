//! Error type for `mdk-recovery`.

use std::error::Error;
use std::fmt::Write;

use bitcoin::{Amount, Network};

/// Render `err` and every `source()` in its chain joined by `": "`.
/// Used at the boundary where opaque backend errors (reqwest, esplora-
/// client) are flattened into [`RecoveryError::Esplora`] so a 429
/// surfaces as `"connection closed: connection reset by peer"` rather
/// than `"error sending request for url (…)"`.
pub fn fmt_error_chain(err: &dyn Error) -> String {
    let mut out = err.to_string();
    let mut cur = err.source();
    while let Some(src) = cur {
        write!(out, ": {src}").expect("writing to String never fails");
        cur = src.source();
    }
    out
}

#[derive(Debug, thiserror::Error)]
pub enum RecoveryError {
    #[error("destination address is not valid for network {network:?}")]
    AddressNetworkMismatch { network: Network },

    #[error("total input value would overflow")]
    AmountOverflow,

    #[error("recovery plan has no inputs to sweep")]
    EmptyInputs,

    #[error("esplora backend error: {0}")]
    Esplora(String),

    #[error("estimated fee {fee} exceeds total input value {total_in}")]
    FeeExceedsValue { fee: Amount, total_in: Amount },

    #[error("invalid destination address: {0}")]
    InvalidDestination(String),

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

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Layer {
        msg: &'static str,
        source: Option<Box<dyn Error>>,
    }

    impl std::fmt::Display for Layer {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.msg)
        }
    }

    impl Error for Layer {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            self.source.as_deref()
        }
    }

    /// The chain walker must surface every nested cause; the whole
    /// point of the helper is that 429s and connect-resets stop being
    /// hidden behind reqwest's outer "error sending request" wrapper.
    #[test]
    fn fmt_error_chain_joins_every_source() {
        let inner = Layer {
            msg: "connection reset",
            source: None,
        };
        let middle = Layer {
            msg: "transport error",
            source: Some(Box::new(inner)),
        };
        let outer = Layer {
            msg: "esplora call failed",
            source: Some(Box::new(middle)),
        };
        assert_eq!(
            fmt_error_chain(&outer),
            "esplora call failed: transport error: connection reset"
        );
    }

    #[test]
    fn fmt_error_chain_handles_singleton() {
        let solo = Layer {
            msg: "solo",
            source: None,
        };
        assert_eq!(fmt_error_chain(&solo), "solo");
    }
}
