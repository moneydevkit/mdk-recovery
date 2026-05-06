//! BIP-39 mnemonic → LDK seed + BIP-32 master extended key.
//!
//! Mirrors what ldk-node does at startup when no BIP-39 passphrase is
//! configured (mdk does not support passphrases): the BIP-39 seed bytes
//! become the entropy for `Xpriv::new_master`, and the resulting master
//! key's secret bytes are then handed to `KeysManager` as the LDK seed.

use bip39::Mnemonic;
use bitcoin::Network;
use bitcoin::bip32::Xpriv;

/// Derive the 32-byte LDK seed and BIP-32 master extended private key
/// from a BIP-39 mnemonic.
///
/// The mnemonic must already be parsed; word-list validation is the
/// caller's responsibility. The function is total: BIP-39 seeds are
/// always 64 bytes, which is well within `Xpriv::new_master`'s accepted
/// 16-64 byte range, and the inner HMAC-SHA512 cannot fail.
pub fn ldk_seed_and_master(mnemonic: &Mnemonic, network: Network) -> ([u8; 32], Xpriv) {
    let seed_bytes = mnemonic.to_seed("");
    let master = Xpriv::new_master(network, &seed_bytes)
        .expect("BIP-39 seed (64 bytes) always yields a valid Xpriv");
    let ldk_seed = master.private_key.secret_bytes();
    (ldk_seed, master)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    /// A standard 12-word BIP-39 fixture used across the ecosystem.
    const KNOWN_MNEMONIC: &str = "abandon abandon abandon abandon abandon abandon \
                                  abandon abandon abandon abandon abandon about";

    /// BIP-39 seed for `KNOWN_MNEMONIC` with empty passphrase, i.e.
    /// PBKDF2-HMAC-SHA512(password = mnemonic, salt = "mnemonic", c =
    /// 2048, dkLen = 64). Reproducible with any conforming BIP-39
    /// implementation; treat it as externally verifiable ground truth.
    const KNOWN_SEED_EMPTY_PASSPHRASE: [u8; 64] = [
        0x5e, 0xb0, 0x0b, 0xbd, 0xdc, 0xf0, 0x69, 0x08, 0x48, 0x89, 0xa8, 0xab, 0x91, 0x55, 0x56,
        0x81, 0x65, 0xf5, 0xc4, 0x53, 0xcc, 0xb8, 0x5e, 0x70, 0x81, 0x1a, 0xae, 0xd6, 0xf6, 0xda,
        0x5f, 0xc1, 0x9a, 0x5a, 0xc4, 0x0b, 0x38, 0x9c, 0xd3, 0x70, 0xd0, 0x86, 0x20, 0x6d, 0xec,
        0x8a, 0xa6, 0xc4, 0x3d, 0xae, 0xa6, 0x69, 0x0f, 0x20, 0xad, 0x3d, 0x8d, 0x48, 0xb2, 0xd2,
        0xce, 0x9e, 0x38, 0xe4,
    ];

    fn known_mnemonic() -> Mnemonic {
        Mnemonic::from_str(KNOWN_MNEMONIC).expect("known mnemonic is valid BIP-39")
    }

    #[test]
    fn composes_to_seed_and_xpriv_new_master() {
        // Anchor the test on the externally verifiable BIP-39 seed bytes,
        // then assert that our function produces the same Xpriv that
        // `Xpriv::new_master(network, &seed)` does. If either step drifts
        // (e.g. someone accidentally introduces a passphrase, or swaps
        // the order of the composition), this fires.
        let expected_master = Xpriv::new_master(Network::Bitcoin, &KNOWN_SEED_EMPTY_PASSPHRASE)
            .expect("known-good 64-byte seed");

        let (ldk_seed, master) = ldk_seed_and_master(&known_mnemonic(), Network::Bitcoin);

        assert_eq!(master, expected_master);
        assert_eq!(ldk_seed, expected_master.private_key.secret_bytes());
    }

    #[test]
    fn ldk_seed_is_network_independent() {
        // The 32-byte LDK seed is the first half of HMAC-SHA512("Bitcoin
        // seed", bip39_seed); the network argument only tags the Xpriv's
        // serialization version byte. Every network must produce the same
        // LDK seed.
        let mnemonic = known_mnemonic();
        let (mainnet, _) = ldk_seed_and_master(&mnemonic, Network::Bitcoin);
        let (testnet, _) = ldk_seed_and_master(&mnemonic, Network::Testnet);
        let (signet, _) = ldk_seed_and_master(&mnemonic, Network::Signet);
        let (regtest, _) = ldk_seed_and_master(&mnemonic, Network::Regtest);
        assert_eq!(mainnet, testnet);
        assert_eq!(mainnet, signet);
        assert_eq!(mainnet, regtest);
    }
}
