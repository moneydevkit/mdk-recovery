//! End-to-end regtest tests for `mdk-recovery`. Each test spawns a
//! real bitcoind, fronts it with a tiny mock esplora, funds an
//! address derived from a known mnemonic, runs the recovery binary
//! as a subprocess, and asserts the funds moved.

use std::time::Duration;

use bip39::Mnemonic;
use bitcoin::address::NetworkUnchecked;
use bitcoin::{Address, Amount, Network};
use serde_json::json;

mod common;
use common::{MockEsplora, TestBitcoind, recovery_command};

/// Standard BIP-39 vector mnemonic. Same one mdkd uses; deterministic
/// derivations across runs.
const TEST_MNEMONIC: &str = "abandon abandon abandon abandon abandon abandon \
                             abandon abandon abandon abandon abandon about";

/// Smoke test: fund a BIP-84 receive address derived from the seed,
/// invoke `mdk-recovery sweep --broadcast`, and assert the swept
/// funds land at a destination outside the seed's wallet.
#[tokio::test]
async fn bip84_funds_are_swept() {
    let network = Network::Regtest;
    let bitcoind = TestBitcoind::new().await;
    let mock = MockEsplora::start(bitcoind.rpc.clone()).await;

    let mnemonic = Mnemonic::parse(TEST_MNEMONIC).expect("parse mnemonic");
    let derived = mdk_recovery::scan::derive_all(&mnemonic, network);

    let receive = derived.bip84.first().expect("at least one BIP-84 entry");
    let receive_spk = receive.script_pubkey.clone();
    let receive_addr = Address::from_script(&receive_spk, network).expect("script -> address");

    mock.register_script(receive_spk).await;

    let funded = Amount::from_sat(1_000_000);
    bitcoind.fund(&receive_addr, funded).await;

    let dest_raw = bitcoind.rpc.call("getnewaddress", json!([])).await;
    let dest_str = dest_raw.as_str().expect("getnewaddress -> string");
    let dest: Address<NetworkUnchecked> = dest_str.parse().expect("parse dest");
    let dest = dest.require_network(network).expect("network match");

    let mnemonic_file = tempfile::NamedTempFile::new().expect("tempfile");
    std::fs::write(mnemonic_file.path(), TEST_MNEMONIC).expect("write mnemonic");

    let url = mock.url();
    let dest_str = dest.to_string();
    let mnemonic_path = mnemonic_file.path().to_path_buf();
    tokio::task::spawn_blocking(move || {
        recovery_command(&url)
            .args([
                "sweep",
                "--mnemonic-file",
                mnemonic_path.to_str().unwrap(),
                "--network",
                "regtest",
                "--to",
                &dest_str,
                "--feerate-sat-vb",
                "5",
                "--broadcast",
            ])
            .write_stdin(format!("{dest_str}\n"))
            .timeout(Duration::from_secs(60))
            .assert()
            .success();
    })
    .await
    .expect("recovery subprocess panicked");

    bitcoind.mine(1).await;

    let landed = bitcoind.balance_at(&dest.script_pubkey()).await;
    assert!(
        landed > Amount::ZERO,
        "destination must have received the swept funds; balance was zero"
    );
    assert!(
        landed < funded,
        "fees must reduce the swept amount: landed {landed}, funded {funded}"
    );
}
