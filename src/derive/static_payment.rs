//! v2 `static_payment` derivation: enumerate the 1000 possible
//! commitment-output script_pubkeys our counterparty can pay us to when
//! they force-close a channel opened with `v2_remote_key_derivation`.
//!
//! Mirrors `KeysManager::possible_v2_counterparty_closed_balance_spks`
//! (rust-lightning 0.2). The derivation is:
//!
//! 1. `inner_master = Xpriv::new_master(_, ldk_seed)` — KeysManager
//!    rebuilds its own xprv from the 32-byte LDK seed; the network
//!    argument is irrelevant since only `secret_bytes` propagate.
//! 2. `static_branch = inner_master / 8h`.
//! 3. For idx in 0..1000: `key = static_branch / idx_h`; pair the
//!    pubkey with both commitment-output shapes (non-anchor P2WPKH
//!    and anchors-zero-fee-HTLC P2WSH) using upstream's
//!    `get_countersigner_payment_script`.
//!
//! We keep the secret keys alongside the scripts so the sweep stage
//! can sign without re-deriving.

use bitcoin::Network;
use bitcoin::ScriptBuf;
use bitcoin::bip32::{ChildNumber, Xpriv};
use bitcoin::secp256k1::{PublicKey, Secp256k1, SecretKey};
use lightning::ln::chan_utils::get_countersigner_payment_script;
use lightning::sign::STATIC_PAYMENT_KEY_COUNT;
use lightning::types::features::ChannelTypeFeatures;

/// Hardened path index for the LDK static_payment branch (`m/8h`),
/// mirroring `STATIC_PAYMENT_KEY_INDEX` in `KeysManager::new`. Upstream
/// keeps this private to `KeysManager::new`'s body, so we redeclare it.
const STATIC_PAYMENT_KEY_INDEX: u32 = 8;

/// One static_payment derivation entry: the index, the keypair, and
/// the two possible commitment-output script shapes (the only two LDK
/// emits today: P2WPKH for non-anchor channels, P2WSH for anchor
/// channels).
#[derive(Debug, Clone)]
pub struct StaticPaymentEntry {
    pub idx: u16,
    pub secret_key: SecretKey,
    pub public_key: PublicKey,
    /// Non-anchor `to_remote`: `OP_0 <HASH160(pubkey)>`.
    pub p2wpkh_spk: ScriptBuf,
    /// Anchor `to_remote`: P2WSH wrapping
    /// `<pubkey> OP_CHECKSIGVERIFY 1 OP_CHECKSEQUENCEVERIFY`.
    pub anchor_p2wsh_spk: ScriptBuf,
}

/// Enumerate all `STATIC_PAYMENT_KEY_COUNT` v2 static_payment entries
/// for a given LDK seed.
pub fn static_payment_entries(ldk_seed: &[u8; 32]) -> Vec<StaticPaymentEntry> {
    let secp = Secp256k1::new();
    // Network only tags the xprv's serialization byte; KeysManager
    // hard-codes Testnet here for the same reason. We pick Bitcoin
    // for parity with the rest of this crate's mainnet-leaning code,
    // but the result of any subsequent derive_priv is identical.
    let inner_master = Xpriv::new_master(Network::Bitcoin, ldk_seed)
        .expect("32-byte LDK seed always yields a valid Xpriv");
    let static_branch = inner_master
        .derive_priv(
            &secp,
            &ChildNumber::from_hardened_idx(STATIC_PAYMENT_KEY_INDEX)
                .expect("8 is a valid hardened index"),
        )
        .expect("derivation never fails for a valid Xpriv");

    let static_remote_features = ChannelTypeFeatures::only_static_remote_key();
    let mut anchor_features = ChannelTypeFeatures::only_static_remote_key();
    anchor_features.set_anchors_zero_fee_htlc_tx_required();

    (0..STATIC_PAYMENT_KEY_COUNT)
        .map(|idx| {
            let child = static_branch
                .derive_priv(
                    &secp,
                    &ChildNumber::from_hardened_idx(u32::from(idx))
                        .expect("idx < STATIC_PAYMENT_KEY_COUNT is always a valid hardened index"),
                )
                .expect("derivation never fails for a valid Xpriv");
            let secret_key = child.private_key;
            let public_key = PublicKey::from_secret_key(&secp, &secret_key);
            let p2wpkh_spk = get_countersigner_payment_script(&static_remote_features, &public_key);
            let anchor_p2wsh_spk = get_countersigner_payment_script(&anchor_features, &public_key);
            StaticPaymentEntry {
                idx,
                secret_key,
                public_key,
                p2wpkh_spk,
                anchor_p2wsh_spk,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lightning::sign::KeysManager;

    /// Arbitrary fixture LDK seed. The exact bytes are not magic; we
    /// only need a stable seed to run our derivation alongside an
    /// upstream `KeysManager` and assert byte-equality.
    const LDK_SEED: [u8; 32] = [
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
        0xff, 0x10, 0x32, 0x54, 0x76, 0x98, 0xba, 0xdc, 0xfe, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab,
        0xcd, 0xef,
    ];

    #[test]
    fn matches_upstream_possible_v2_counterparty_closed_balance_spks() {
        // Anchor on the upstream KeysManager: build it with the same
        // LDK seed, ask it for the 2000 script_pubkeys, and assert our
        // entries reproduce the same flat sequence in the same order
        // (P2WPKH then P2WSH-anchor, idx 0 → idx 999). If the upstream
        // derivation drifts (e.g. they re-order the pair, change the
        // path, or swap script flavours), this fires.
        let secp = Secp256k1::new();
        // starting_time_*, v2_remote_key_derivation do not affect the
        // static_payment_key derivation; pass any values.
        let keys_manager = KeysManager::new(&LDK_SEED, 0, 0, true);
        let upstream = keys_manager.possible_v2_counterparty_closed_balance_spks(&secp);
        assert_eq!(upstream.len(), usize::from(STATIC_PAYMENT_KEY_COUNT) * 2);

        let ours = static_payment_entries(&LDK_SEED);
        assert_eq!(ours.len(), usize::from(STATIC_PAYMENT_KEY_COUNT));

        for (i, entry) in ours.iter().enumerate() {
            assert_eq!(entry.idx, i as u16);
            assert_eq!(entry.p2wpkh_spk, upstream[i * 2]);
            assert_eq!(entry.anchor_p2wsh_spk, upstream[i * 2 + 1]);
        }
    }

    #[test]
    fn p2wpkh_and_anchor_scripts_are_distinct() {
        // Cheap structural check: every entry produces two different
        // script_pubkeys (one P2WPKH, one P2WSH). Catches an obvious
        // class of typos where both fields end up populated from the
        // same builder.
        let entries = static_payment_entries(&LDK_SEED);
        for entry in &entries {
            assert!(entry.p2wpkh_spk.is_p2wpkh());
            assert!(entry.anchor_p2wsh_spk.is_p2wsh());
            assert_ne!(entry.p2wpkh_spk, entry.anchor_p2wsh_spk);
        }
    }

    #[test]
    fn derivation_is_deterministic() {
        let a = static_payment_entries(&LDK_SEED);
        let b = static_payment_entries(&LDK_SEED);
        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(b.iter()) {
            assert_eq!(x.idx, y.idx);
            assert_eq!(x.secret_key.secret_bytes(), y.secret_key.secret_bytes());
            assert_eq!(x.public_key, y.public_key);
            assert_eq!(x.p2wpkh_spk, y.p2wpkh_spk);
            assert_eq!(x.anchor_p2wsh_spk, y.anchor_p2wsh_spk);
        }
    }
}
