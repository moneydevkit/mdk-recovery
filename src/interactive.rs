//! Interactive TTY helpers.
//!
//! Two flavours of code live here:
//!
//! - **Effectful shells** (`prompt_mnemonic`, `prompt_destination`,
//!   `prompt_confirm`): thin wrappers around stdin / rpassword that
//!   handle one piece of user input each. Not unit-tested; small
//!   enough to read.
//! - **Pure renderers** (`render_summary`, `explorer_tx_url`): no
//!   I/O on inputs, write to a borrowed `Write` if anything.
//!   Unit-tested against fixed expectations so the user-facing
//!   strings can't drift silently.

use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::str::FromStr;

use bip39::Mnemonic;
use bitcoin::address::NetworkUnchecked;
use bitcoin::{Address, Amount, Network, Txid};

use crate::cli::parse_mnemonic;
use crate::error::{RecoveryError, Result};
use crate::plan::{RecoveryInput, RecoveryPlan};

/// Maximum retries when the user types an unparseable address.
/// Three is enough to recover from a fat-finger paste without
/// looping forever if stdin is a broken pipe feeding garbage.
const DESTINATION_RETRY_LIMIT: usize = 3;

/// Read a BIP-39 mnemonic from the user.
///
/// On a TTY (and unless `force_stdin` is set), prompts with
/// `rpassword` so the input doesn't echo. Off a TTY — or when the
/// CI path forces it — reads stdin to EOF instead so the caller can
/// pipe a mnemonic in.
pub fn prompt_mnemonic(force_stdin: bool) -> Result<Mnemonic> {
    let raw = if force_stdin || !io::stdin().is_terminal() {
        let mut buf = String::new();
        io::stdin().read_to_string(&mut buf)?;
        buf
    } else {
        rpassword::prompt_password("Enter your mnemonic: ")?
    };
    parse_mnemonic(&raw)
}

/// Prompt the user for a destination address and validate it
/// against `network`. Retries up to [`DESTINATION_RETRY_LIMIT`]
/// times on parse failure (typos); a network mismatch is fatal
/// because the user picked the wrong `--network`.
pub fn prompt_destination(network: Network) -> Result<Address> {
    let stdin = io::stdin();
    let mut stderr = io::stderr();
    let mut last_err = None;
    for _ in 0..DESTINATION_RETRY_LIMIT {
        write!(stderr, "Where to? (onchain address): ")?;
        stderr.flush()?;
        let mut line = String::new();
        stdin.lock().read_line(&mut line)?;
        match parse_destination(line.trim(), network) {
            Ok(addr) => return Ok(addr),
            Err(RecoveryError::InvalidDestination(msg)) => {
                writeln!(stderr, "invalid address: {msg}; try again")?;
                last_err = Some(RecoveryError::InvalidDestination(msg));
            }
            Err(other) => return Err(other),
        }
    }
    Err(last_err
        .unwrap_or_else(|| RecoveryError::InvalidDestination("no valid address provided".into())))
}

/// Parse + network-check `raw`. Split out so the prompt loop has
/// somewhere to call back into without re-doing the error mapping.
fn parse_destination(raw: &str, network: Network) -> Result<Address> {
    let unchecked = Address::<NetworkUnchecked>::from_str(raw)
        .map_err(|e| RecoveryError::InvalidDestination(e.to_string()))?;
    unchecked
        .require_network(network)
        .map_err(|_| RecoveryError::AddressNetworkMismatch { network })
}

/// Ask `prompt [y/N] `; return `true` only on `y` / `yes` (case
/// insensitive). Anything else — including empty input — is a no.
/// The `N` default matches the visible bracket so users can press
/// return to abort.
pub fn prompt_confirm(prompt: &str) -> Result<bool> {
    let mut stderr = io::stderr();
    write!(stderr, "{prompt} [y/N] ")?;
    stderr.flush()?;
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// Render the per-network mempool explorer URL for `txid`. Returns
/// `None` for regtest (no public viewer) and any non-standard
/// network variant.
pub fn explorer_tx_url(network: Network, txid: Txid) -> Option<String> {
    match network {
        Network::Bitcoin => Some(format!("https://mempool.space/tx/{txid}")),
        Network::Signet => Some(format!("https://mutinynet.com/tx/{txid}")),
        Network::Testnet => Some(format!("https://blockstream.info/testnet/tx/{txid}")),
        Network::Regtest => None,
        _ => None,
    }
}

/// Write the "Found N output(s) totalling X BTC." header. Split out
/// from the rest of the summary so it can render before the user is
/// asked for a destination — they often want to know whether the
/// scan found anything (and how much) before deciding to type an
/// address.
pub fn render_found_line<W: Write>(count: usize, total: Amount, mut w: W) -> io::Result<()> {
    writeln!(
        w,
        "Found {count} output{plural} totalling {total}.",
        plural = if count == 1 { "" } else { "s" },
        total = format_btc(total),
    )
}

/// Write the destination / fee / net block (and, under `verbose`, a
/// per-input listing) to `w`. Assumes [`render_found_line`] already
/// fired, so this picks up at `To:`.
pub fn render_destination_summary<W: Write>(
    plan: &RecoveryPlan,
    verbose: bool,
    mut w: W,
) -> io::Result<()> {
    writeln!(w, "To:   {}", plan.destination)?;
    writeln!(
        w,
        "Fee:  {} sats at {} sat/vB",
        plan.estimated_fee.to_sat(),
        plan.feerate_sat_vb,
    )?;
    writeln!(w, "Net:  {}", format_btc(plan.estimated_output))?;

    if verbose {
        writeln!(w)?;
        writeln!(w, "Inputs:")?;
        for inp in &plan.inputs {
            writeln!(
                w,
                "  {} {} ({} sats)",
                inp.outpoint(),
                input_label(inp),
                inp.value().to_sat(),
            )?;
        }
    }
    Ok(())
}

/// Format a satoshi amount as `X.XXXXXXXX BTC` using integer math.
/// Avoids `Amount::to_btc()`'s f64 round-trip; the user reads this
/// before approving a broadcast, so exact display matters.
fn format_btc(amount: Amount) -> String {
    let sats = amount.to_sat();
    let whole = sats / 100_000_000;
    let frac = sats % 100_000_000;
    format!("{whole}.{frac:08} BTC")
}

/// Short human label for a single recovery input, used in the
/// verbose summary block.
fn input_label(inp: &RecoveryInput) -> String {
    use crate::derive::bip84::Bip84Chain;
    match inp {
        RecoveryInput::Bip84 { chain, idx, .. } => {
            let chain_label = match chain {
                Bip84Chain::External => "receive",
                Bip84Chain::Internal => "change",
            };
            format!("bip84 {chain_label} #{idx}")
        }
        RecoveryInput::StaticRemoteKeyP2wpkh { idx, .. } => {
            format!("static_payment p2wpkh #{idx}")
        }
        RecoveryInput::StaticRemoteKeyP2wshAnchor { idx, .. } => {
            format!("static_payment p2wsh-anchor #{idx}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::derive::bip84::Bip84Chain;
    use crate::plan::RecoveryPlan;
    use bitcoin::hashes::Hash;
    use bitcoin::secp256k1::{Secp256k1, SecretKey};
    use bitcoin::{Address, OutPoint, ScriptBuf, WPubkeyHash};
    use std::str::FromStr;

    fn fixture_privkey() -> SecretKey {
        SecretKey::from_slice(&[0x42; 32]).expect("valid secret key")
    }

    fn fixture_p2wpkh_spk() -> ScriptBuf {
        let secp = Secp256k1::new();
        let pk = bitcoin::secp256k1::PublicKey::from_secret_key(&secp, &fixture_privkey());
        ScriptBuf::new_p2wpkh(&WPubkeyHash::hash(&pk.serialize()))
    }

    fn fixture_bip84_input(value_sats: u64, vout: u32) -> RecoveryInput {
        RecoveryInput::Bip84 {
            outpoint: OutPoint {
                txid: bitcoin::Txid::all_zeros(),
                vout,
            },
            value: Amount::from_sat(value_sats),
            chain: Bip84Chain::External,
            idx: 0,
            privkey: fixture_privkey(),
            script_pubkey: fixture_p2wpkh_spk(),
        }
    }

    fn mainnet_address() -> Address<NetworkUnchecked> {
        Address::from_str("bc1qcr8te4kr609gcawutmrza0j4xv80jy8z306fyu").unwrap()
    }

    /// The destination summary block must match the Martin mockup
    /// byte-for-byte. Lock it in so a stray `writeln!` reorder
    /// can't silently change what the user sees before approving
    /// a broadcast.
    #[test]
    fn render_destination_summary_matches_mockup() {
        let plan = RecoveryPlan::new(
            Network::Bitcoin,
            vec![
                fixture_bip84_input(200_000, 0),
                fixture_bip84_input(150_000, 1),
                fixture_bip84_input(132_911, 2),
            ],
            mainnet_address(),
            5,
        )
        .expect("valid plan");

        let mut out = Vec::new();
        render_destination_summary(&plan, false, &mut out).expect("render");
        let rendered = String::from_utf8(out).expect("utf8");

        assert!(
            rendered.starts_with("To:   bc1qcr8te4kr609gcawutmrza0j4xv80jy8z306fyu\n"),
            "header mismatch: {rendered:?}",
        );
        assert!(rendered.contains("\nFee:  "));
        assert!(rendered.contains(" sat/vB\n"));
        assert!(rendered.contains("\nNet:  0.004"));
    }

    /// The found line is what the user sees before the destination
    /// prompt; it must render the right plural and amount.
    #[test]
    fn render_found_line_plural_and_amount() {
        let mut out = Vec::new();
        render_found_line(3, Amount::from_sat(482_911), &mut out).expect("render");
        assert_eq!(
            String::from_utf8(out).expect("utf8"),
            "Found 3 outputs totalling 0.00482911 BTC.\n",
        );
    }

    /// Singular phrasing for a one-output find — small detail, but
    /// "Found 1 outputs" reads like a bug to anyone literate.
    #[test]
    fn render_found_line_uses_singular_for_one_output() {
        let mut out = Vec::new();
        render_found_line(1, Amount::from_sat(1_000_000), &mut out).expect("render");
        assert_eq!(
            String::from_utf8(out).expect("utf8"),
            "Found 1 output totalling 0.01000000 BTC.\n",
        );
    }

    /// Verbose mode appends a per-input listing. Don't pin every
    /// byte — the format may evolve — but assert the outpoint and
    /// the kind label show up so the auditor can map back to the
    /// scan.
    #[test]
    fn render_destination_summary_verbose_lists_inputs() {
        let plan = RecoveryPlan::new(
            Network::Bitcoin,
            vec![fixture_bip84_input(500_000, 7)],
            mainnet_address(),
            5,
        )
        .expect("valid plan");

        let mut out = Vec::new();
        render_destination_summary(&plan, true, &mut out).expect("render");
        let rendered = String::from_utf8(out).expect("utf8");
        assert!(rendered.contains("Inputs:"));
        assert!(rendered.contains("bip84 receive #0"));
        assert!(rendered.contains(":7"));
    }

    #[test]
    fn explorer_tx_url_per_network() {
        let txid = bitcoin::Txid::all_zeros();
        assert_eq!(
            explorer_tx_url(Network::Bitcoin, txid).as_deref(),
            Some(format!("https://mempool.space/tx/{txid}").as_str()),
        );
        assert_eq!(
            explorer_tx_url(Network::Signet, txid).as_deref(),
            Some(format!("https://mutinynet.com/tx/{txid}").as_str()),
        );
        assert_eq!(
            explorer_tx_url(Network::Testnet, txid).as_deref(),
            Some(format!("https://blockstream.info/testnet/tx/{txid}").as_str()),
        );
        assert_eq!(explorer_tx_url(Network::Regtest, txid), None);
    }

    #[test]
    fn format_btc_pads_fractional_to_eight_digits() {
        assert_eq!(format_btc(Amount::from_sat(0)), "0.00000000 BTC");
        assert_eq!(format_btc(Amount::from_sat(1)), "0.00000001 BTC");
        assert_eq!(format_btc(Amount::from_sat(482_911)), "0.00482911 BTC");
        assert_eq!(format_btc(Amount::from_sat(100_000_000)), "1.00000000 BTC");
    }
}
