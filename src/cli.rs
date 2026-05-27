//! Helpers shared by every CLI subcommand: reading the mnemonic from
//! disk or stdin, picking the esplora endpoint for a network, and
//! choosing between human-readable and JSON output.
//!
//! Kept separate from `main.rs` so the helpers (which have actual
//! logic) can be tested without going through clap.

use std::fs;
use std::io::{self, Read};
use std::path::Path;
use std::str::FromStr;

use bip39::Mnemonic;
use bitcoin::Network;

use crate::error::{RecoveryError, Result};

/// Environment variable that overrides the regtest endpoint. Test
/// harnesses set this to the URL of a regtest esplora-electrs spawned
/// alongside the test bitcoind. Shipped binaries do not use regtest.
pub const REGTEST_ENV_VAR: &str = "MDK_RECOVERY_ESPLORA_URL";

/// Output format for subcommands that render a structured report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    /// Human-readable summary via the report's `Display` impl.
    Human,
    /// Pretty-printed JSON via serde.
    Json,
}

impl OutputFormat {
    /// Map the boolean `--json` flag to the variant.
    pub fn from_json_flag(json: bool) -> Self {
        if json { Self::Json } else { Self::Human }
    }
}

/// Read a mnemonic from `path`, treating `-` as stdin. The file or
/// stream is read end-to-end, trimmed, and parsed by `bip39::Mnemonic`.
/// We never accept the mnemonic on argv: that would leak it to `ps`.
pub fn read_mnemonic(path: &str) -> Result<Mnemonic> {
    let raw = if path == "-" {
        let mut buf = String::new();
        io::stdin().read_to_string(&mut buf)?;
        buf
    } else {
        fs::read_to_string(Path::new(path))?
    };
    parse_mnemonic(&raw)
}

/// Parse `raw` as a BIP-39 mnemonic, ignoring surrounding whitespace
/// and collapsing runs of spaces / tabs / newlines so files written
/// by editors with trailing newlines or wrapped lines still parse.
pub(crate) fn parse_mnemonic(raw: &str) -> Result<Mnemonic> {
    let normalised = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    Mnemonic::from_str(&normalised).map_err(|e| RecoveryError::InvalidMnemonic(e.to_string()))
}

/// Look up the esplora base URL for `network`. Mainnet points at the
/// MDK-operated esplora; signet points at mutinynet (the custom-
/// signet MDK actually targets, not vanilla signet); testnet keeps
/// the public blockstream endpoint. Regtest reads from
/// [`REGTEST_ENV_VAR`] so the test harness can point us at its own
/// esplora-electrs without exposing a CLI override flag.
pub fn endpoint_for(network: Network) -> Result<String> {
    match network {
        Network::Bitcoin => Ok("https://esplora.moneydevkit.com/api".into()),
        Network::Testnet => Ok("https://blockstream.info/testnet/api".into()),
        Network::Signet => Ok("https://mutinynet.com/api".into()),
        Network::Regtest => {
            std::env::var(REGTEST_ENV_VAR).map_err(|_| RecoveryError::RegtestEndpointMissing)
        }
        _ => Err(RecoveryError::UnsupportedNetwork { network }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whitespace tolerance: a mnemonic file with trailing newline,
    /// indentation, or wrapped lines must still parse.
    #[test]
    fn parse_mnemonic_normalises_whitespace() {
        let canonical = "abandon abandon abandon abandon abandon abandon \
                         abandon abandon abandon abandon abandon about";
        let cases = [
            canonical.to_string(),
            format!("{canonical}\n"),
            format!("  {canonical}  \n"),
            canonical.replace(' ', "\n"),
            canonical.replace(' ', "\t"),
        ];
        for raw in cases {
            parse_mnemonic(&raw).unwrap_or_else(|e| panic!("must parse {raw:?}: {e}"));
        }
    }

    #[test]
    fn parse_mnemonic_rejects_invalid_words() {
        let err = parse_mnemonic("not a real bip39 mnemonic at all").expect_err("must reject");
        assert!(matches!(err, RecoveryError::InvalidMnemonic(_)));
    }

    /// Mainnet/testnet/signet must resolve unconditionally; regtest
    /// must fail without the env var. Don't pollute the env in tests
    /// — use a guard to set/unset around the regtest assertion.
    #[test]
    fn endpoint_for_mainnet_signet_testnet_resolve() {
        assert!(
            endpoint_for(Network::Bitcoin)
                .unwrap()
                .contains("esplora.moneydevkit.com")
        );
        assert!(endpoint_for(Network::Testnet).unwrap().contains("testnet"));
        assert!(endpoint_for(Network::Signet).unwrap().contains("mutinynet"));
    }

    #[test]
    fn endpoint_for_regtest_requires_env_var() {
        // SAFETY: tests run in process; remove the var if a parallel
        // test set it. The other endpoint_for_* tests don't read
        // regtest, so this is local.
        unsafe { std::env::remove_var(REGTEST_ENV_VAR) };
        assert!(matches!(
            endpoint_for(Network::Regtest),
            Err(RecoveryError::RegtestEndpointMissing)
        ));
    }
}
