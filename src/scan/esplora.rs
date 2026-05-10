//! Esplora-backed UTXO scan and transaction broadcast.
//!
//! The pure orchestration ([`fetch_utxos_with`]) is split from the
//! HTTP plumbing ([`scripthash_utxos`], [`broadcast`]). Transient
//! failures (429, 5xx, connection blips) are absorbed by the
//! [`with_retry`] wrapper in [`crate::scan::retry`], which classifies
//! the HTTP response and loops with exponential backoff.
//!
//! `esplora-client = 0.12` does not expose a `/utxo` helper, so we
//! piggy-back on its inner [`reqwest::Client`] for the GET. The
//! broadcast path uses the upstream wrapper directly.

use std::future::Future;
use std::time::Duration;

use bitcoin::hashes::{Hash, sha256};
use bitcoin::{ScriptBuf, Transaction, Txid};
use esplora_client::AsyncClient;
use futures::stream::{self, StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};

use crate::error::{RecoveryError, Result, fmt_error_chain};
use crate::scan::retry::{RetryPolicy, with_retry};
pub use crate::scan::throttle::RateLimiter;

/// Sustained request budget against the public esplora endpoint, in
/// requests per second. Sized at 80 % of the server's 5 r/s cap so a
/// little client-side jitter doesn't punch through into 429 territory.
pub const ESPLORA_RATE_PER_SEC: f64 = 4.0;

/// Burst budget against the public esplora endpoint, in tokens. Below
/// the server's `burst=10` to leave some headroom.
pub const ESPLORA_BURST: f64 = 8.0;

/// Retry budget for a single script's UTXO fetch. Five attempts
/// with a 500 ms base doubling to 8 s gives a per-script worst case
/// of ~15.5 s before we surface the failure — long enough to ride
/// out a rate-limit burst or a brief upstream wobble, short enough
/// that a wedged server doesn't hang the scan.
const RETRY_POLICY: RetryPolicy = RetryPolicy {
    max_attempts: 5,
    base_delay: Duration::from_millis(500),
    max_delay: Duration::from_secs(8),
};

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
/// parallel. When `limiter` is `Some`, each fetch acquires a token
/// first; when `None`, fetches fire as fast as `max_concurrency`
/// allows. See [`fetch_utxos_with`] for the orchestration shape.
pub async fn fetch_utxos(
    client: &AsyncClient,
    scripts: &[ScriptBuf],
    max_concurrency: usize,
    limiter: Option<&RateLimiter>,
) -> Result<Vec<(ScriptBuf, Vec<Utxo>)>> {
    fetch_utxos_with(scripts, max_concurrency, move |spk| async move {
        scripthash_utxos(client, &spk, limiter).await
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

/// Fetch one script's UTXOs. The retry wrapper owns the request/
/// status classification; here we only spell out the URL, acquire a
/// limiter token per attempt, and decode the body on success. The
/// limiter is re-acquired on every attempt so retries draw from the
/// same per-IP budget as fresh requests — otherwise a burst of 429s
/// would all bypass the bucket and stampede the server.
///
/// A body-decode error after a 2xx response surfaces as permanent.
/// In practice this means schema drift (which would never recover
/// anyway) or a torn body read mid-stream (rare enough that losing
/// one script per occurrence is the right tradeoff against retrying
/// what could be a parse miss for the entire budget).
async fn scripthash_utxos(
    client: &AsyncClient,
    spk: &ScriptBuf,
    limiter: Option<&RateLimiter>,
) -> Result<Vec<Utxo>> {
    let scripthash = sha256::Hash::hash(spk.as_bytes());
    let url = format!("{}/scripthash/{:x}/utxo", client.url(), scripthash);
    let response = with_retry(RETRY_POLICY, || async {
        if let Some(l) = limiter {
            l.acquire().await;
        }
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
