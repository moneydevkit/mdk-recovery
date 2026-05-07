//! BIP-84 (P2WPKH) derivation: enumerate the first `gap_limit`
//! external and internal addresses for the default account.
//!
//! Path: `m/84h / coin_type h / 0h / change / i`
//!
//! - `84h`: BIP-84 purpose constant.
//! - `coin_type`: 0 for mainnet, 1 for testnet/signet/regtest
//!   (SLIP-0044). mdk does not configure other coin types.
//! - account `0h`: default account; mdk does not surface multiple
//!   accounts.
//! - `change`: 0 external (receive), 1 internal (change).
//! - `i`: address index in `0..gap_limit`.
//!
//! As with `static_payment`, we keep the secret keys alongside the
//! script_pubkeys so the sweep stage can sign without re-deriving.

use bitcoin::Network;
use bitcoin::ScriptBuf;
use bitcoin::WPubkeyHash;
use bitcoin::bip32::{ChildNumber, Xpriv};
use bitcoin::hashes::Hash;
use bitcoin::secp256k1::{PublicKey, Secp256k1, SecretKey};
use serde::Serialize;

/// Default BIP-44 / BIP-84 address-gap limit. Wallets stop scanning
/// after this many consecutive unused addresses; we mirror the same
/// number for our pure-scan path.
pub const DEFAULT_GAP_LIMIT: u32 = 20;

/// Whether a BIP-84 address sits on the external (receive) chain or
/// the internal (change) chain.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Bip84Chain {
    External,
    Internal,
}

impl Bip84Chain {
    fn child_number(self) -> ChildNumber {
        let idx = match self {
            Self::External => 0,
            Self::Internal => 1,
        };
        ChildNumber::from_normal_idx(idx).expect("0 and 1 are valid normal indices")
    }
}

/// One BIP-84 derivation entry. The secret key is omitted from the
/// serde representation: `--json` callers should never accidentally
/// leak it through a pipe; the structured key material stays in
/// memory only.
#[derive(Debug, Clone, Serialize)]
pub struct Bip84Entry {
    pub chain: Bip84Chain,
    pub idx: u32,
    #[serde(skip)]
    pub secret_key: SecretKey,
    pub public_key: PublicKey,
    pub script_pubkey: ScriptBuf,
}

/// SLIP-0044 coin type for `network`. Bitcoin mainnet is 0; every
/// other Bitcoin network shares coin type 1, matching what BDK and
/// rust-lightning do.
fn coin_type(network: Network) -> u32 {
    match network {
        Network::Bitcoin => 0,
        _ => 1,
    }
}

/// Enumerate the first `gap_limit` BIP-84 entries for the default
/// account on each chain, external first then internal.
pub fn bip84_entries(master: &Xpriv, network: Network, gap_limit: u32) -> Vec<Bip84Entry> {
    let secp = Secp256k1::new();
    let purpose = ChildNumber::from_hardened_idx(84).expect("84 < 2^31");
    let coin = ChildNumber::from_hardened_idx(coin_type(network)).expect("coin type < 2^31");
    let account = ChildNumber::from_hardened_idx(0).expect("0 < 2^31");

    let account_xprv = master
        .derive_priv(&secp, &[purpose, coin, account])
        .expect("derivation never fails for a valid Xpriv");

    let mut out = Vec::with_capacity((gap_limit as usize) * 2);
    for chain in [Bip84Chain::External, Bip84Chain::Internal] {
        let chain_xprv = account_xprv
            .derive_priv(&secp, &chain.child_number())
            .expect("derivation never fails for a valid Xpriv");
        for idx in 0..gap_limit {
            let leaf_node =
                ChildNumber::from_normal_idx(idx).expect("idx < gap_limit always fits in u31");
            let leaf = chain_xprv
                .derive_priv(&secp, &leaf_node)
                .expect("derivation never fails for a valid Xpriv");
            let secret_key = leaf.private_key;
            let public_key = PublicKey::from_secret_key(&secp, &secret_key);
            let wpkh = WPubkeyHash::hash(&public_key.serialize());
            let script_pubkey = ScriptBuf::new_p2wpkh(&wpkh);
            out.push(Bip84Entry {
                chain,
                idx,
                secret_key,
                public_key,
                script_pubkey,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::Address;
    use std::str::FromStr;

    /// BIP-39 seed for the canonical mnemonic
    /// `abandon abandon abandon abandon abandon abandon
    ///  abandon abandon abandon abandon abandon about`
    /// with empty passphrase. This is the seed used by the BIP-84
    /// specification's "Test vectors" section, so the addresses
    /// derived below are externally verifiable against the spec.
    const KNOWN_SEED: [u8; 64] = [
        0x5e, 0xb0, 0x0b, 0xbd, 0xdc, 0xf0, 0x69, 0x08, 0x48, 0x89, 0xa8, 0xab, 0x91, 0x55, 0x56,
        0x81, 0x65, 0xf5, 0xc4, 0x53, 0xcc, 0xb8, 0x5e, 0x70, 0x81, 0x1a, 0xae, 0xd6, 0xf6, 0xda,
        0x5f, 0xc1, 0x9a, 0x5a, 0xc4, 0x0b, 0x38, 0x9c, 0xd3, 0x70, 0xd0, 0x86, 0x20, 0x6d, 0xec,
        0x8a, 0xa6, 0xc4, 0x3d, 0xae, 0xa6, 0x69, 0x0f, 0x20, 0xad, 0x3d, 0x8d, 0x48, 0xb2, 0xd2,
        0xce, 0x9e, 0x38, 0xe4,
    ];

    fn known_master() -> Xpriv {
        Xpriv::new_master(Network::Bitcoin, &KNOWN_SEED).expect("valid 64-byte seed")
    }

    fn expected_spk(addr: &str) -> ScriptBuf {
        Address::from_str(addr)
            .expect("valid bech32 address")
            .require_network(Network::Bitcoin)
            .expect("address is mainnet")
            .script_pubkey()
    }

    #[test]
    fn matches_bip84_spec_test_vectors() {
        // Three vectors lifted directly from BIP-84 §"Test vectors":
        // - m/84'/0'/0'/0/0 (first receive)
        // - m/84'/0'/0'/0/1 (second receive)
        // - m/84'/0'/0'/1/0 (first change)
        // If the path constants drift or the script wrap regresses,
        // at least one of these breaks.
        let entries = bip84_entries(&known_master(), Network::Bitcoin, 2);

        let receive_0 = &entries[0];
        assert_eq!(receive_0.chain, Bip84Chain::External);
        assert_eq!(receive_0.idx, 0);
        assert_eq!(
            receive_0.script_pubkey,
            expected_spk("bc1qcr8te4kr609gcawutmrza0j4xv80jy8z306fyu")
        );

        let receive_1 = &entries[1];
        assert_eq!(receive_1.chain, Bip84Chain::External);
        assert_eq!(receive_1.idx, 1);
        assert_eq!(
            receive_1.script_pubkey,
            expected_spk("bc1qnjg0jd8228aq7egyzacy8cys3knf9xvrerkf9g")
        );

        let change_0 = &entries[2];
        assert_eq!(change_0.chain, Bip84Chain::Internal);
        assert_eq!(change_0.idx, 0);
        assert_eq!(
            change_0.script_pubkey,
            expected_spk("bc1q8c6fshw2dlwun7ekn9qwf37cu2rn755upcp6el")
        );
    }

    #[test]
    fn gap_limit_shapes_output() {
        // Gap-limit `n` produces `2n` entries, external first.
        let entries = bip84_entries(&known_master(), Network::Bitcoin, 5);
        assert_eq!(entries.len(), 10);
        for (i, entry) in entries.iter().enumerate().take(5) {
            assert_eq!(entry.chain, Bip84Chain::External);
            assert_eq!(entry.idx, i as u32);
        }
        for (i, entry) in entries.iter().enumerate().skip(5) {
            assert_eq!(entry.chain, Bip84Chain::Internal);
            assert_eq!(entry.idx, (i - 5) as u32);
        }
        assert!(bip84_entries(&known_master(), Network::Bitcoin, 0).is_empty());
    }

    #[test]
    fn coin_type_partitions_mainnet_from_other_networks() {
        // Mainnet picks coin_type 0, every other network picks 1; the
        // resulting key sets must be disjoint at every index.
        let master = known_master();
        let mainnet = bip84_entries(&master, Network::Bitcoin, 3);
        let testnet = bip84_entries(&master, Network::Testnet, 3);
        let signet = bip84_entries(&master, Network::Signet, 3);
        let regtest = bip84_entries(&master, Network::Regtest, 3);
        for i in 0..mainnet.len() {
            assert_ne!(mainnet[i].script_pubkey, testnet[i].script_pubkey);
        }
        // Testnet, signet, and regtest all share coin_type 1, so their
        // derivations are identical.
        assert_eq!(testnet.len(), signet.len());
        for (t, (s, r)) in testnet.iter().zip(signet.iter().zip(regtest.iter())) {
            assert_eq!(t.script_pubkey, s.script_pubkey);
            assert_eq!(t.script_pubkey, r.script_pubkey);
        }
    }

    #[test]
    fn derivation_is_deterministic() {
        let a = bip84_entries(&known_master(), Network::Bitcoin, DEFAULT_GAP_LIMIT);
        let b = bip84_entries(&known_master(), Network::Bitcoin, DEFAULT_GAP_LIMIT);
        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(b.iter()) {
            assert_eq!(x.chain, y.chain);
            assert_eq!(x.idx, y.idx);
            assert_eq!(x.secret_key.secret_bytes(), y.secret_key.secret_bytes());
            assert_eq!(x.public_key, y.public_key);
            assert_eq!(x.script_pubkey, y.script_pubkey);
        }
    }
}
