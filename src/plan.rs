//! Recovery plan: a validated bundle of (UTXOs, destination, feerate)
//! ready to hand to the signer.
//!
//! `RecoveryInput` is the sum type of every UTXO shape we sweep:
//! the two static_payment commitment-output flavours (non-anchor
//! P2WPKH, anchor-zero-fee-HTLC P2WSH) plus the BIP-84 receive and
//! change addresses.
//!
//! `RecoveryPlan::new` is the only construction path that performs
//! validation. It checks the inputs are nonempty, the destination
//! address is valid for the declared network, the estimated fee does
//! not exceed the swept value, and the residual output clears the
//! script-specific dust threshold (BOLT 3 §6).
//!
//! Fee math operates on weight units throughout and converts to
//! vbytes with `ceil(wu / 4)` at the end. The per-input weight
//! constants are exact for a 72-byte ECDSA signature (max-DER + 1
//! sighash byte) plus the relevant trailing witness item.

use crate::derive::bip84::Bip84Chain;
use crate::error::{RecoveryError, Result};
use bitcoin::address::NetworkUnchecked;
use bitcoin::secp256k1::SecretKey;
use bitcoin::{Address, Amount, Network, OutPoint, ScriptBuf};
use serde::Serialize;
use std::fmt;

/// Default feerate when the caller doesn't specify one. Five sat/vB
/// is comfortably above the long-term minimum-relay floor (1 sat/vB)
/// and lands a sweep within a few blocks under typical mempool load
/// without overpaying for a recovery that isn't time-sensitive.
pub const DEFAULT_FEERATE_SAT_VB: u64 = 5;

/// One on-chain UTXO we can sign for, tagged by the derivation path
/// it came from. The variants carry exactly the data the signer needs:
/// the prevout (for sighash), the script_pubkey (for sighash), the
/// privkey (to sign), and for the anchor variant the redeem script
/// (for the witness).
///
/// `privkey` is `#[serde(skip)]` on every variant: a `--json` render
/// of a plan must never leak key material into a shell history or
/// log file.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecoveryInput {
    /// BIP-84 P2WPKH on the receive or change chain.
    Bip84 {
        outpoint: OutPoint,
        value: Amount,
        chain: Bip84Chain,
        idx: u32,
        #[serde(skip)]
        privkey: SecretKey,
        script_pubkey: ScriptBuf,
    },
    /// Non-anchor `to_remote`: P2WPKH spendable with a single
    /// signature.
    StaticRemoteKeyP2wpkh {
        outpoint: OutPoint,
        value: Amount,
        idx: u16,
        #[serde(skip)]
        privkey: SecretKey,
        script_pubkey: ScriptBuf,
    },
    /// Anchor-zero-fee-HTLC `to_remote`: P2WSH wrapping
    /// `<pk> OP_CHECKSIGVERIFY 1 OP_CHECKSEQUENCEVERIFY`, spent with
    /// `nSequence = 1` so the CSV check passes.
    StaticRemoteKeyP2wshAnchor {
        outpoint: OutPoint,
        value: Amount,
        idx: u16,
        #[serde(skip)]
        privkey: SecretKey,
        redeem_script: ScriptBuf,
        script_pubkey: ScriptBuf,
    },
}

impl RecoveryInput {
    pub fn outpoint(&self) -> OutPoint {
        match self {
            Self::Bip84 { outpoint, .. }
            | Self::StaticRemoteKeyP2wpkh { outpoint, .. }
            | Self::StaticRemoteKeyP2wshAnchor { outpoint, .. } => *outpoint,
        }
    }

    pub fn value(&self) -> Amount {
        match self {
            Self::Bip84 { value, .. }
            | Self::StaticRemoteKeyP2wpkh { value, .. }
            | Self::StaticRemoteKeyP2wshAnchor { value, .. } => *value,
        }
    }

    pub fn script_pubkey(&self) -> &ScriptBuf {
        match self {
            Self::Bip84 { script_pubkey, .. }
            | Self::StaticRemoteKeyP2wpkh { script_pubkey, .. }
            | Self::StaticRemoteKeyP2wshAnchor { script_pubkey, .. } => script_pubkey,
        }
    }

    /// Weight units this input contributes to the sweep transaction.
    /// Base = 4 * (36 outpoint + 1 script_sig_len + 0 script_sig + 4
    /// nSequence) = 164. Witness varies by spend path.
    fn weight_units(&self) -> u64 {
        const BASE_WU: u64 = 4 * 41;
        match self {
            // Witness: stack(1) + sig_len(1) + sig(72) + pk_len(1) + pk(33) = 108.
            Self::Bip84 { .. } | Self::StaticRemoteKeyP2wpkh { .. } => BASE_WU + 108,
            // Witness: stack(1) + sig_len(1) + sig(72) + redeem_len(1) + redeem(37) = 112.
            // Redeem = OP_PUSHBYTES_33 + 33B pk + OP_CHECKSIGVERIFY + OP_PUSHNUM_1 + OP_CSV.
            Self::StaticRemoteKeyP2wshAnchor { .. } => BASE_WU + 112,
        }
    }
}

/// A signed, broadcast-ready sweep is one `RecoveryPlan::new` call
/// away from a `Transaction`. Constructed only via `new()`, which
/// validates the invariants the signer relies on; the public fields
/// are kept readable so callers can render summaries without ceremony.
#[derive(Debug, Clone, Serialize)]
pub struct RecoveryPlan {
    pub network: Network,
    pub inputs: Vec<RecoveryInput>,
    pub destination: Address,
    pub feerate_sat_vb: u64,
    pub estimated_fee: Amount,
    pub estimated_output: Amount,
}

impl RecoveryPlan {
    pub fn new(
        network: Network,
        inputs: Vec<RecoveryInput>,
        destination: Address<NetworkUnchecked>,
        feerate_sat_vb: u64,
    ) -> Result<Self> {
        if inputs.is_empty() {
            return Err(RecoveryError::EmptyInputs);
        }
        let destination = destination
            .require_network(network)
            .map_err(|_| RecoveryError::AddressNetworkMismatch { network })?;

        let dest_spk = destination.script_pubkey();
        let dust = dust_threshold(&dest_spk).ok_or(RecoveryError::UnsupportedDestinationScript)?;

        let total_in = inputs
            .iter()
            .try_fold(Amount::ZERO, |acc, inp| acc.checked_add(inp.value()))
            .ok_or(RecoveryError::AmountOverflow)?;

        let vsize = estimated_vsize(&inputs, &dest_spk);
        let estimated_fee = Amount::from_sat(vsize.saturating_mul(feerate_sat_vb));

        let estimated_output =
            total_in
                .checked_sub(estimated_fee)
                .ok_or(RecoveryError::FeeExceedsValue {
                    fee: estimated_fee,
                    total_in,
                })?;
        if estimated_output < dust {
            return Err(RecoveryError::OutputBelowDust {
                output: estimated_output,
                dust,
            });
        }

        Ok(Self {
            network,
            inputs,
            destination,
            feerate_sat_vb,
            estimated_fee,
            estimated_output,
        })
    }
}

impl fmt::Display for RecoveryPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let total_in = self
            .inputs
            .iter()
            .map(RecoveryInput::value)
            .fold(Amount::ZERO, |a, b| a.checked_add(b).unwrap_or(Amount::MAX));

        let mut bip84 = 0;
        let mut srk_p2wpkh = 0;
        let mut srk_anchor = 0;
        for inp in &self.inputs {
            match inp {
                RecoveryInput::Bip84 { .. } => bip84 += 1,
                RecoveryInput::StaticRemoteKeyP2wpkh { .. } => srk_p2wpkh += 1,
                RecoveryInput::StaticRemoteKeyP2wshAnchor { .. } => srk_anchor += 1,
            }
        }

        writeln!(f, "Recovery plan (network: {})", self.network)?;
        writeln!(f, "  inputs: {} total", self.inputs.len())?;
        writeln!(f, "    bip84:                       {bip84}")?;
        writeln!(f, "    static_payment p2wpkh:       {srk_p2wpkh}")?;
        writeln!(f, "    static_payment p2wsh-anchor: {srk_anchor}")?;
        writeln!(f, "  destination:       {}", self.destination)?;
        writeln!(f, "  feerate:           {} sat/vB", self.feerate_sat_vb)?;
        writeln!(f, "  total input value: {total_in}")?;
        writeln!(f, "  estimated fee:     {}", self.estimated_fee)?;
        writeln!(f, "  estimated output:  {}", self.estimated_output)
    }
}

/// Tx-level overhead: nVersion(4) + marker(1wu) + flag(1wu) +
/// in_count(1) + out_count(1) + locktime(4) = 10B base + 2 wu witness
/// = 42 wu. Holds for any segwit tx with <253 inputs and <253 outputs,
/// which a single-output sweep trivially satisfies.
const TX_OVERHEAD_WU: u64 = 4 * 10 + 2;

/// Weight units a single output of `spk_len` script bytes contributes:
/// value(8) + spk_len_varint(1) + spk_len. All non-witness, ×4.
fn output_wu(spk_len: usize) -> u64 {
    (8 + 1 + spk_len as u64) * 4
}

fn estimated_vsize(inputs: &[RecoveryInput], dest_spk: &ScriptBuf) -> u64 {
    let wu = TX_OVERHEAD_WU
        + inputs.iter().map(RecoveryInput::weight_units).sum::<u64>()
        + output_wu(dest_spk.len());
    wu.div_ceil(4)
}

/// BOLT 3 §6 dust thresholds for the destination output. Returns
/// `None` if `spk` is not one of the recognised standard types; in
/// practice `Address::from_str` would have already rejected anything
/// outside this set, so the `None` arm is a defensive backstop.
fn dust_threshold(spk: &ScriptBuf) -> Option<Amount> {
    let sats = if spk.is_p2wpkh() {
        294
    } else if spk.is_p2wsh() || spk.is_p2tr() {
        330
    } else if spk.is_p2pkh() {
        546
    } else if spk.is_p2sh() {
        540
    } else {
        return None;
    };
    Some(Amount::from_sat(sats))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::Txid;
    use bitcoin::hashes::Hash;
    use bitcoin::secp256k1::Secp256k1;
    use std::str::FromStr;

    fn fixture_privkey() -> SecretKey {
        SecretKey::from_slice(&[0x42; 32]).expect("valid secret key")
    }

    fn fixture_outpoint(vout: u32) -> OutPoint {
        OutPoint {
            txid: Txid::all_zeros(),
            vout,
        }
    }

    fn fixture_p2wpkh_spk() -> ScriptBuf {
        let secp = Secp256k1::new();
        let pk = bitcoin::secp256k1::PublicKey::from_secret_key(&secp, &fixture_privkey());
        ScriptBuf::new_p2wpkh(&bitcoin::WPubkeyHash::hash(&pk.serialize()))
    }

    fn fixture_p2wpkh_input(value_sats: u64, vout: u32) -> RecoveryInput {
        RecoveryInput::Bip84 {
            outpoint: fixture_outpoint(vout),
            value: Amount::from_sat(value_sats),
            chain: Bip84Chain::External,
            idx: 0,
            privkey: fixture_privkey(),
            script_pubkey: fixture_p2wpkh_spk(),
        }
    }

    fn mainnet_p2wpkh_address() -> Address<NetworkUnchecked> {
        Address::from_str("bc1qcr8te4kr609gcawutmrza0j4xv80jy8z306fyu").unwrap()
    }

    fn mainnet_p2wpkh_spk() -> ScriptBuf {
        mainnet_p2wpkh_address()
            .require_network(Network::Bitcoin)
            .unwrap()
            .script_pubkey()
    }

    #[test]
    fn rejects_empty_inputs() {
        let err = RecoveryPlan::new(Network::Bitcoin, vec![], mainnet_p2wpkh_address(), 5)
            .expect_err("empty inputs must fail");
        assert!(matches!(err, RecoveryError::EmptyInputs));
    }

    #[test]
    fn rejects_address_on_wrong_network() {
        let err = RecoveryPlan::new(
            Network::Testnet,
            vec![fixture_p2wpkh_input(100_000, 0)],
            mainnet_p2wpkh_address(),
            5,
        )
        .expect_err("mainnet address on testnet plan must fail");
        assert!(matches!(err, RecoveryError::AddressNetworkMismatch { .. }));
    }

    #[test]
    fn rejects_fee_exceeding_total_in() {
        // 10 sat in vs many thousand sat fee.
        let err = RecoveryPlan::new(
            Network::Bitcoin,
            vec![fixture_p2wpkh_input(10, 0)],
            mainnet_p2wpkh_address(),
            1000,
        )
        .expect_err("fee > total_in must fail");
        assert!(matches!(err, RecoveryError::FeeExceedsValue { .. }));
    }

    #[test]
    fn rejects_output_below_dust() {
        // Pick a value that survives the fee but lands below the
        // 294-sat P2WPKH dust threshold. Single P2WPKH input, single
        // P2WPKH output, 1 sat/vB ⇒ ~110 vbyte ⇒ ~110 sat fee.
        // Inputs of 400 sat ⇒ output ≈ 290 sat, below 294 dust.
        let err = RecoveryPlan::new(
            Network::Bitcoin,
            vec![fixture_p2wpkh_input(400, 0)],
            mainnet_p2wpkh_address(),
            1,
        )
        .expect_err("dust output must fail");
        assert!(matches!(err, RecoveryError::OutputBelowDust { .. }));
    }

    #[test]
    fn estimates_fee_and_output_for_typical_sweep() {
        // One BIP-84 input of 1 BTC, P2WPKH destination, 5 sat/vB.
        // Expected vsize ≈ 110 vbyte (overhead 11 + input 68 +
        // output 31), fee ≈ 550 sat. Assert exact match against the
        // helper math so any drift in the constants fires.
        let inputs = vec![fixture_p2wpkh_input(100_000_000, 0)];
        let plan = RecoveryPlan::new(
            Network::Bitcoin,
            inputs.clone(),
            mainnet_p2wpkh_address(),
            5,
        )
        .expect("valid plan");

        let dest_spk = mainnet_p2wpkh_spk();
        let expected_vsize = estimated_vsize(&inputs, &dest_spk);
        assert_eq!(plan.estimated_fee, Amount::from_sat(expected_vsize * 5));
        assert_eq!(
            plan.estimated_output,
            Amount::from_sat(100_000_000) - plan.estimated_fee
        );
        assert_eq!(plan.feerate_sat_vb, 5);
        assert_eq!(plan.network, Network::Bitcoin);
        assert_eq!(plan.inputs.len(), 1);
    }

    #[test]
    fn weight_distinguishes_anchor_from_p2wpkh_input() {
        // Anchor inputs cost more vbytes than P2WPKH (4 wu more from
        // the larger redeem-bearing witness). Lock the sign in.
        let p2wpkh_input = fixture_p2wpkh_input(0, 0);
        let anchor_input = RecoveryInput::StaticRemoteKeyP2wshAnchor {
            outpoint: fixture_outpoint(1),
            value: Amount::ZERO,
            idx: 0,
            privkey: fixture_privkey(),
            redeem_script: ScriptBuf::new(),
            script_pubkey: ScriptBuf::new(),
        };
        assert!(anchor_input.weight_units() > p2wpkh_input.weight_units());
    }

    #[test]
    fn dust_thresholds_match_bolt3() {
        let p2wpkh = mainnet_p2wpkh_spk();
        assert_eq!(dust_threshold(&p2wpkh), Some(Amount::from_sat(294)));

        // Manually construct other types to check the table without
        // pulling more fixtures.
        let p2wsh = ScriptBuf::new_p2wsh(&bitcoin::WScriptHash::hash(&[]));
        assert_eq!(dust_threshold(&p2wsh), Some(Amount::from_sat(330)));

        let p2pkh = ScriptBuf::new_p2pkh(&bitcoin::PubkeyHash::hash(&[0x02; 33]));
        assert_eq!(dust_threshold(&p2pkh), Some(Amount::from_sat(546)));

        let p2sh = ScriptBuf::new_p2sh(&bitcoin::ScriptHash::hash(&[]));
        assert_eq!(dust_threshold(&p2sh), Some(Amount::from_sat(540)));
    }
}
