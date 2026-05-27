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

    let spk = derived
        .bip84
        .first()
        .expect("at least one BIP-84 entry")
        .script_pubkey
        .clone();
    fund_script(&bitcoind, &mock, &spk, Amount::from_sat(1_000_000), network).await;

    let dest = sweep_to_fresh_address(&bitcoind, &mock.url(), TEST_MNEMONIC, network, false).await;
    bitcoind.mine(1).await;

    assert_dest_received(&bitcoind, &dest, Amount::from_sat(1_000_000)).await;
}

/// Same shape as the BIP-84 test, but the funded UTXO sits at the
/// first enumerable static_remote_key P2WPKH script. Confirms we
/// scan, sign, and broadcast against the v2 close-output path.
#[tokio::test]
async fn static_payment_p2wpkh_funds_are_swept() {
    let network = Network::Regtest;
    let bitcoind = TestBitcoind::new().await;
    let mock = MockEsplora::start(bitcoind.rpc.clone()).await;

    let mnemonic = Mnemonic::parse(TEST_MNEMONIC).expect("parse mnemonic");
    let derived = mdk_recovery::scan::derive_all(&mnemonic, network);

    let spk = derived
        .static_entries
        .first()
        .expect("at least one static_payment entry")
        .p2wpkh_spk
        .clone();
    fund_script(&bitcoind, &mock, &spk, Amount::from_sat(1_000_000), network).await;

    let dest = sweep_to_fresh_address(&bitcoind, &mock.url(), TEST_MNEMONIC, network, false).await;
    bitcoind.mine(1).await;

    assert_dest_received(&bitcoind, &dest, Amount::from_sat(1_000_000)).await;
}

/// Mixed sweep: fund a BIP-84 receive, a static_remote_key P2WPKH,
/// and a static_remote_key P2WSH-anchor in the same wallet, run a
/// single `sweep --broadcast`, and assert the combined value lands
/// at the destination. Exercises the full set of input shapes the
/// signer supports in one transaction.
#[tokio::test]
async fn mixed_inputs_are_swept_in_one_tx() {
    let network = Network::Regtest;
    let bitcoind = TestBitcoind::new().await;
    let mock = MockEsplora::start(bitcoind.rpc.clone()).await;

    let mnemonic = Mnemonic::parse(TEST_MNEMONIC).expect("parse mnemonic");
    let derived = mdk_recovery::scan::derive_all(&mnemonic, network);

    let bip84_spk = derived
        .bip84
        .first()
        .expect("at least one BIP-84 entry")
        .script_pubkey
        .clone();
    let static_p2wpkh_spk = derived
        .static_entries
        .first()
        .expect("at least one static_payment entry")
        .p2wpkh_spk
        .clone();
    let anchor_spk = derived
        .static_entries
        .first()
        .expect("at least one static_payment entry")
        .anchor_p2wsh_spk
        .clone();

    let funded = Amount::from_sat(1_000_000);
    fund_script(&bitcoind, &mock, &bip84_spk, funded, network).await;
    fund_script(&bitcoind, &mock, &static_p2wpkh_spk, funded, network).await;
    fund_script(&bitcoind, &mock, &anchor_spk, funded, network).await;

    // Anchor channels are off by default; this test funds an anchor
    // script so the sweep must opt in.
    let dest = sweep_to_fresh_address(&bitcoind, &mock.url(), TEST_MNEMONIC, network, true).await;
    bitcoind.mine(1).await;

    let total = funded * 3;
    assert_dest_received(&bitcoind, &dest, total).await;
}

/// Register `spk` with the mock esplora so the scan finds it, then
/// fund the corresponding address from bitcoind.
async fn fund_script(
    bitcoind: &TestBitcoind,
    mock: &MockEsplora,
    spk: &bitcoin::ScriptBuf,
    amount: Amount,
    network: Network,
) {
    let addr = Address::from_script(spk, network).expect("script -> address");
    mock.register_script(spk.clone()).await;
    bitcoind.fund(&addr, amount).await;
}

/// Run the flat `mdk-recovery` binary against a fresh bitcoind
/// address. The mnemonic is piped on stdin (via `--mnemonic-stdin`)
/// and `--yes` skips the interactive confirmation. `scan_anchors`
/// toggles the matching CLI flag for tests that fund anchor
/// scripts. Returns the destination so the caller can assert
/// against its post-sweep balance.
async fn sweep_to_fresh_address(
    bitcoind: &TestBitcoind,
    mock_url: &str,
    mnemonic: &str,
    network: Network,
    scan_anchors: bool,
) -> Address {
    let dest_raw = bitcoind.rpc.call("getnewaddress", json!([])).await;
    let dest: Address<NetworkUnchecked> = dest_raw
        .as_str()
        .expect("getnewaddress -> string")
        .parse()
        .expect("parse dest");
    let dest = dest.require_network(network).expect("network match");

    let url = mock_url.to_string();
    let dest_str = dest.to_string();
    let mnemonic_str = mnemonic.to_string();
    tokio::task::spawn_blocking(move || {
        let mut args: Vec<&str> = vec![
            "--network",
            "regtest",
            "--to",
            &dest_str,
            "--feerate-sat-vb",
            "5",
            "--mnemonic-stdin",
            "--yes",
        ];
        if scan_anchors {
            args.push("--scan-anchors");
        }
        recovery_command(&url)
            .args(&args)
            .write_stdin(mnemonic_str)
            .timeout(Duration::from_secs(60))
            .assert()
            .success();
    })
    .await
    .expect("recovery subprocess panicked");

    dest
}

/// Assert the destination received between zero and `funded` —
/// strictly less than `funded` so the fee must have been paid.
async fn assert_dest_received(bitcoind: &TestBitcoind, dest: &Address, funded: Amount) {
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
