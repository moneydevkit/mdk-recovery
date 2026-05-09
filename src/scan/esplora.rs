//! Esplora-backed UTXO scan and transaction broadcast.
//!
//! The pure orchestration ([`fetch_utxos_with`]) is split from the
//! HTTP plumbing ([`scripthash_utxos`], [`broadcast`]). Tests target
//! the orchestration with closures and the response schema with a
//! JSON fixture; the wire layer is trusted reqwest + serde.
//!
//! `esplora-client = 0.12` does not expose a `/utxo` helper, so we
//! piggy-back on its inner [`reqwest::Client`] for the GET. The
//! broadcast path uses the upstream wrapper directly.

use std::future::Future;

use bitcoin::hashes::{Hash, sha256};
use bitcoin::{ScriptBuf, Transaction, Txid};
use esplora_client::AsyncClient;
use futures::stream::{self, StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};

use crate::error::{RecoveryError, Result, fmt_error_chain};

/// One unspent output as returned by esplora's
/// `/scripthash/{hash}/utxo` endpoint.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Utxo {
    pub txid: Txid,
    pub vout: u32,
    pub value: u64,
    pub status: UtxoStatus,
}

/// Confirmation status for a UTXO. `block_height` is `None` while
/// the funding tx sits in the mempool.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct UtxoStatus {
    pub confirmed: bool,
    pub block_height: Option<u32>,
}

/// Fetch UTXOs for every script in `scripts` against `client` in
/// parallel. See [`fetch_utxos_with`] for the orchestration shape.
pub async fn fetch_utxos(
    client: &AsyncClient,
    scripts: &[ScriptBuf],
    max_concurrency: usize,
) -> Result<Vec<(ScriptBuf, Vec<Utxo>)>> {
    fetch_utxos_with(scripts, max_concurrency, |spk| async move {
        scripthash_utxos(client, &spk).await
    })
    .await
}

/// Pure orchestration: pair each script with its UTXO list, with at
/// most `max_concurrency` `fetch_one` futures in flight at once. The
/// returned vec is unordered relative to `scripts` since each entry
/// carries its own `ScriptBuf`.
async fn fetch_utxos_with<F, Fut>(
    scripts: &[ScriptBuf],
    max_concurrency: usize,
    fetch_one: F,
) -> Result<Vec<(ScriptBuf, Vec<Utxo>)>>
where
    F: Fn(ScriptBuf) -> Fut,
    Fut: Future<Output = Result<Vec<Utxo>>>,
{
    stream::iter(scripts.iter().cloned())
        .map(|spk| {
            let fut = fetch_one(spk.clone());
            async move { fut.await.map(|utxos| (spk, utxos)) }
        })
        .buffer_unordered(max_concurrency.max(1))
        .try_collect()
        .await
}

/// Broadcast a signed sweep. `Ok(())` means the backend accepted the
/// tx; mempool acceptance does not guarantee confirmation.
pub async fn broadcast(client: &AsyncClient, tx: &Transaction) -> Result<()> {
    client
        .broadcast(tx)
        .await
        .map_err(|e| RecoveryError::Esplora(fmt_error_chain(&e)))
}

async fn scripthash_utxos(client: &AsyncClient, spk: &ScriptBuf) -> Result<Vec<Utxo>> {
    let scripthash = sha256::Hash::hash(spk.as_bytes());
    let url = format!("{}/scripthash/{:x}/utxo", client.url(), scripthash);
    let response = client
        .client()
        .get(&url)
        .send()
        .await
        .map_err(|e| RecoveryError::Esplora(fmt_error_chain(&e)))?;
    if !response.status().is_success() {
        return Err(RecoveryError::Esplora(format!(
            "esplora returned status {} for {url}",
            response.status()
        )));
    }
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

    /// Each script must be paired with the result of its own fetch.
    /// The risk being guarded is silently swapping `(spk_a, utxos_b)`
    /// — neither serde nor `try_collect` would catch that.
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

        let result = fetch_utxos_with(&[spk_a.clone(), spk_b.clone()], 4, |spk| {
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

    /// `buffer_unordered(0)` deadlocks, so the wrapper floors at 1.
    /// Also pins that the cap is honoured rather than ignored — the
    /// high-water-mark of concurrently-in-flight futures must not
    /// exceed the cap we passed in.
    #[tokio::test]
    async fn fetch_utxos_with_caps_concurrent_fetches() {
        let scripts: Vec<ScriptBuf> = (0..16).map(fixture_spk).collect();
        let in_flight = AtomicUsize::new(0);
        let high_water = AtomicUsize::new(0);
        let cap = 4;

        fetch_utxos_with(&scripts, cap, |_| async {
            let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            high_water.fetch_max(now, Ordering::SeqCst);
            tokio::task::yield_now().await;
            in_flight.fetch_sub(1, Ordering::SeqCst);
            Ok(Vec::new())
        })
        .await
        .expect("ok");

        assert!(
            high_water.load(Ordering::SeqCst) <= cap,
            "saw {} concurrent fetches, cap was {cap}",
            high_water.load(Ordering::SeqCst)
        );
    }
}
