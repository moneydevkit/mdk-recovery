//! Esplora-backed UTXO scan and transaction broadcast.
//!
//! Orchestration ([`fetch_utxos_with`]) is split from HTTP plumbing
//! ([`scripthash_utxos`], [`broadcast`]). Transient failures (429,
//! 5xx, connection blips) are absorbed by [`with_retry`] in
//! [`crate::scan::retry`].
//!
//! `esplora-client = 0.12` has no `/utxo` helper, so we use its
//! inner [`reqwest::Client`] directly. Broadcast goes through the
//! upstream wrapper.

use std::future::Future;
use std::time::Duration;

use bitcoin::hashes::{Hash, sha256};
use bitcoin::{ScriptBuf, Transaction, Txid};
use esplora_client::AsyncClient;
use indicatif::{ProgressBar, ProgressStyle};
use serde::{Deserialize, Serialize};

use crate::error::{RecoveryError, Result, fmt_error_chain};
use crate::scan::retry::{RetryPolicy, with_retry};
pub use crate::scan::throttle::Throttle;

/// Esplora request rate. Matches the MDK endpoint's stock
/// `blockstream/esplora` nginx `limit_req=5r/s` per IP. Genuine
/// wobbles are absorbed by the retry budget.
pub const ESPLORA_RATE_PER_SEC: f64 = 5.0;

/// Per-script retry budget. Eight attempts doubling 500 ms → 30 s
/// gives ~63 s worst case — long enough to ride out sustained 429s.
const RETRY_POLICY: RetryPolicy = RetryPolicy {
    max_attempts: 8,
    base_delay: Duration::from_millis(500),
    max_delay: Duration::from_secs(30),
};

/// One unspent output from `/scripthash/{hash}/utxo`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Utxo {
    pub txid: Txid,
    pub vout: u32,
    pub value: u64,
    pub status: UtxoStatus,
}

/// `block_height` is `None` while the funding tx is in the mempool.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct UtxoStatus {
    pub confirmed: bool,
    pub block_height: Option<u32>,
}

/// Sequentially fetch UTXOs for every `scripts` entry against
/// `client`. Each fetch waits on the throttle first. Renders a
/// progress bar on stderr; stalls past `base_delay` signal server
/// backpressure.
pub async fn fetch_utxos(
    client: &AsyncClient,
    scripts: &[ScriptBuf],
    throttle: &Throttle,
) -> Result<Vec<(ScriptBuf, Vec<Utxo>)>> {
    let bar = scan_progress_bar(scripts.len() as u64);
    let result = fetch_utxos_with(scripts, |spk| {
        let bar = bar.clone();
        async move {
            let utxos = scripthash_utxos(client, &spk, throttle).await?;
            bar.inc(1);
            Ok(utxos)
        }
    })
    .await;
    match &result {
        Ok(_) => bar.finish_with_message("scan complete"),
        Err(_) => bar.abandon_with_message("scan failed"),
    }
    result
}

/// Stderr progress bar. Abandon (vs. clear) on failure leaves the
/// last-known position visible.
fn scan_progress_bar(total: u64) -> ProgressBar {
    let bar = ProgressBar::new(total);
    bar.set_style(
        ProgressStyle::with_template(
            "[{elapsed_precise}] [{wide_bar}] {pos}/{len} ({per_sec}, ETA {eta}) {msg}",
        )
        .expect("static template parses")
        .progress_chars("=>-"),
    );
    bar
}

/// Pair each script with its UTXO list, driving `fetch_one`
/// sequentially.
async fn fetch_utxos_with<F, Fut>(
    scripts: &[ScriptBuf],
    fetch_one: F,
) -> Result<Vec<(ScriptBuf, Vec<Utxo>)>>
where
    F: Fn(ScriptBuf) -> Fut,
    Fut: Future<Output = Result<Vec<Utxo>>>,
{
    let mut out = Vec::with_capacity(scripts.len());
    for spk in scripts {
        let utxos = fetch_one(spk.clone()).await?;
        out.push((spk.clone(), utxos));
    }
    Ok(out)
}

/// Broadcast a signed sweep. Mempool acceptance does not guarantee
/// confirmation.
pub async fn broadcast(client: &AsyncClient, tx: &Transaction) -> Result<()> {
    client
        .broadcast(tx)
        .await
        .map_err(|e| RecoveryError::Esplora(fmt_error_chain(&e)))
}

/// Fetch one script's UTXOs. Each attempt waits on the throttle
/// before firing. A body-decode error after a 2xx surfaces as
/// permanent — schema drift or a torn read, neither worth burning
/// the retry budget on.
async fn scripthash_utxos(
    client: &AsyncClient,
    spk: &ScriptBuf,
    throttle: &Throttle,
) -> Result<Vec<Utxo>> {
    let scripthash = sha256::Hash::hash(spk.as_bytes());
    let url = format!("{}/scripthash/{:x}/utxo", client.url(), scripthash);
    let response = with_retry(RETRY_POLICY, || async {
        throttle.acquire().await;
        client.client().get(&url).send().await
    })
    .await?;
    response
        .json::<Vec<Utxo>>()
        .await
        .map_err(|e| RecoveryError::Esplora(fmt_error_chain(&e)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::Address;
    use bitcoin::Network;
    use std::str::FromStr;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn fixture_spk(byte: u8) -> ScriptBuf {
        ScriptBuf::new_p2wpkh(&bitcoin::WPubkeyHash::hash(&[byte; 33]))
    }

    fn mainnet_spk() -> ScriptBuf {
        Address::from_str("bc1qcr8te4kr609gcawutmrza0j4xv80jy8z306fyu")
            .unwrap()
            .require_network(Network::Bitcoin)
            .unwrap()
            .script_pubkey()
    }

    /// Guards against swapping `(spk_a, utxos_b)` — neither serde
    /// nor `try_collect` would catch that.
    #[tokio::test]
    async fn fetch_utxos_with_pairs_each_script_with_its_result() {
        let spk_a = mainnet_spk();
        let spk_b = fixture_spk(0x42);
        let utxo_a = Utxo {
            txid: Txid::from_byte_array([0x01; 32]),
            vout: 0,
            value: 100_000,
            status: UtxoStatus {
                confirmed: true,
                block_height: Some(800_000),
            },
        };
        let utxo_b = Utxo {
            txid: Txid::from_byte_array([0x02; 32]),
            vout: 1,
            value: 250_000,
            status: UtxoStatus {
                confirmed: false,
                block_height: None,
            },
        };

        let result = fetch_utxos_with(&[spk_a.clone(), spk_b.clone()], |spk| {
            let utxo_a = utxo_a.clone();
            let utxo_b = utxo_b.clone();
            let spk_a = spk_a.clone();
            async move {
                if spk == spk_a {
                    Ok(vec![utxo_a])
                } else {
                    Ok(vec![utxo_b])
                }
            }
        })
        .await
        .expect("ok");

        let by_spk: std::collections::HashMap<_, _> = result.into_iter().collect();
        assert_eq!(by_spk.get(&spk_a), Some(&vec![utxo_a]));
        assert_eq!(by_spk.get(&spk_b), Some(&vec![utxo_b]));
    }

    /// Pins sequential execution — buffered concurrency would defeat
    /// throttle-driven backpressure.
    #[tokio::test]
    async fn fetch_utxos_with_runs_sequentially() {
        let scripts: Vec<ScriptBuf> = (0..16).map(fixture_spk).collect();
        let in_flight = AtomicUsize::new(0);
        let high_water = AtomicUsize::new(0);

        fetch_utxos_with(&scripts, |_| async {
            let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            high_water.fetch_max(now, Ordering::SeqCst);
            tokio::task::yield_now().await;
            in_flight.fetch_sub(1, Ordering::SeqCst);
            Ok(Vec::new())
        })
        .await
        .expect("ok");

        assert_eq!(high_water.load(Ordering::SeqCst), 1);
    }
}
