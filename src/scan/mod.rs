//! Scan a seed for recoverable UTXOs: derive → fetch → re-key.
//!
//! Composes the derivation modules (`seed`, `derive::*`) with the
//! chain backend (`scan::esplora`) and the plan constructor
//! (`plan`). The entry point [`run`] is three lines: derive scripts,
//! fetch UTXOs, build the report.

pub mod esplora;
mod retry;
mod throttle;

use std::collections::HashMap;
use std::fmt;

use bip39::Mnemonic;
use bitcoin::address::NetworkUnchecked;
use bitcoin::{Address, Amount, Network, OutPoint, ScriptBuf};
use esplora_client::AsyncClient;
use serde::Serialize;

use crate::derive::bip84::{Bip84Chain, Bip84Entry, DEFAULT_GAP_LIMIT, bip84_entries};
use crate::derive::static_payment::{StaticPaymentEntry, static_payment_entries};
use crate::error::Result;
use crate::plan::{RecoveryInput, RecoveryPlan};
use crate::scan::esplora::{ESPLORA_RATE_PER_SEC, Throttle, Utxo, fetch_utxos};
use crate::seed::ldk_seed_and_master;

/// One static_payment derivation that had at least one UTXO.
#[derive(Debug, Clone, Serialize)]
pub struct StaticPaymentHit {
    pub entry: StaticPaymentEntry,
    pub utxos: Vec<Utxo>,
}

/// One BIP-84 derivation that had at least one UTXO.
#[derive(Debug, Clone, Serialize)]
pub struct Bip84Hit {
    pub entry: Bip84Entry,
    pub utxos: Vec<Utxo>,
}

/// Every derivation that produced UTXOs, grouped by source. Empty
/// vecs stay empty — every entry is a real hit.
#[derive(Debug, Clone, Serialize)]
pub struct ScanReport {
    pub network: Network,
    pub static_payment_p2wpkh: Vec<StaticPaymentHit>,
    pub static_payment_anchor: Vec<StaticPaymentHit>,
    pub bip84: Vec<Bip84Hit>,
}

/// Every script the seed can claim, paired with the entries that
/// bind each script back to its keys. Output of the offline
/// `derive` subcommand.
#[derive(Debug, Clone, Serialize)]
pub struct Derived {
    pub network: Network,
    pub static_entries: Vec<StaticPaymentEntry>,
    pub bip84: Vec<Bip84Entry>,
}

impl Derived {
    /// Flat list of every script_pubkey to query. Order doesn't
    /// matter — `build_scan_report` re-keys by script. When
    /// `include_anchors` is `false`, the 1000 P2WSH anchor scripts
    /// are skipped (roughly halves the request count for wallets
    /// without anchor channels).
    pub fn scripts(&self, include_anchors: bool) -> Vec<ScriptBuf> {
        let cap =
            self.static_entries.len() * if include_anchors { 2 } else { 1 } + self.bip84.len();
        let mut scripts = Vec::with_capacity(cap);
        for e in &self.static_entries {
            scripts.push(e.p2wpkh_spk.clone());
            if include_anchors {
                scripts.push(e.anchor_p2wsh_spk.clone());
            }
        }
        for e in &self.bip84 {
            scripts.push(e.script_pubkey.clone());
        }
        scripts
    }
}

/// Derive every script enumerable from the mnemonic: 1000
/// static_payment × 2 flavours + 2×`gap_limit` BIP-84.
pub fn derive_all(mnemonic: &Mnemonic, network: Network) -> Derived {
    let (ldk_seed, master) = ldk_seed_and_master(mnemonic, network);
    Derived {
        network,
        static_entries: static_payment_entries(&ldk_seed),
        bip84: bip84_entries(&master, network, DEFAULT_GAP_LIMIT),
    }
}

/// Derive scripts, fetch UTXOs, and re-key the response into a
/// [`ScanReport`]. `include_anchors` controls whether the 1000
/// P2WSH anchor scripts are queried; off by default since MDK does
/// not use anchor channels.
pub async fn run(
    client: &AsyncClient,
    mnemonic: &Mnemonic,
    network: Network,
    include_anchors: bool,
) -> Result<ScanReport> {
    let derived = derive_all(mnemonic, network);
    let throttle = throttle_for(network);
    let utxos = fetch_utxos(client, &derived.scripts(include_anchors), &throttle).await?;
    Ok(build_scan_report(derived, utxos))
}

/// Pick a throttle for `network`. Regtest hits a local server with
/// no cap, so we run unthrottled there to keep the test harness fast.
fn throttle_for(network: Network) -> Throttle {
    match network {
        Network::Regtest => Throttle::unlimited(),
        _ => Throttle::new(ESPLORA_RATE_PER_SEC),
    }
}

/// Route each non-empty `(spk, utxos)` pair back to its derivation
/// entry. Empty UTXO lists are dropped.
fn build_scan_report(derived: Derived, utxos: Vec<(ScriptBuf, Vec<Utxo>)>) -> ScanReport {
    let mut by_spk: HashMap<ScriptBuf, Vec<Utxo>> =
        utxos.into_iter().filter(|(_, v)| !v.is_empty()).collect();

    let mut static_payment_p2wpkh = Vec::new();
    let mut static_payment_anchor = Vec::new();
    for entry in derived.static_entries {
        if let Some(found) = by_spk.remove(&entry.p2wpkh_spk) {
            static_payment_p2wpkh.push(StaticPaymentHit {
                entry: entry.clone(),
                utxos: found,
            });
        }
        if let Some(found) = by_spk.remove(&entry.anchor_p2wsh_spk) {
            static_payment_anchor.push(StaticPaymentHit {
                entry,
                utxos: found,
            });
        }
    }

    let bip84_hits = derived
        .bip84
        .into_iter()
        .filter_map(|entry| {
            by_spk
                .remove(&entry.script_pubkey)
                .map(|utxos| Bip84Hit { entry, utxos })
        })
        .collect();

    ScanReport {
        network: derived.network,
        static_payment_p2wpkh,
        static_payment_anchor,
        bip84: bip84_hits,
    }
}

impl ScanReport {
    /// Number of UTXOs across every hit. Matches
    /// `into_recovery_inputs().len()` without consuming `self`, so
    /// callers can render a count alongside `total_value` before
    /// committing to a plan.
    pub fn input_count(&self) -> usize {
        let count_utxos =
            |hits: &[StaticPaymentHit]| -> usize { hits.iter().map(|h| h.utxos.len()).sum() };
        count_utxos(&self.static_payment_p2wpkh)
            + count_utxos(&self.static_payment_anchor)
            + self.bip84.iter().map(|h| h.utxos.len()).sum::<usize>()
    }

    /// Total swept value across every hit. Saturates on overflow —
    /// would require a >21M-BTC discovery, but keeps the function
    /// total so callers don't need a `Result`.
    pub fn total_value(&self) -> Amount {
        let sum_utxos = |utxos: &[Utxo]| -> u64 { utxos.iter().map(|u| u.value).sum() };
        let static_p2wpkh: u64 = self
            .static_payment_p2wpkh
            .iter()
            .map(|h| sum_utxos(&h.utxos))
            .sum();
        let static_anchor: u64 = self
            .static_payment_anchor
            .iter()
            .map(|h| sum_utxos(&h.utxos))
            .sum();
        let bip84: u64 = self.bip84.iter().map(|h| sum_utxos(&h.utxos)).sum();
        Amount::from_sat(
            static_p2wpkh
                .saturating_add(static_anchor)
                .saturating_add(bip84),
        )
    }

    /// One `RecoveryInput` per UTXO. The signer wants flat inputs,
    /// not nested groups.
    pub fn into_recovery_inputs(self) -> Vec<RecoveryInput> {
        let mut out = Vec::new();

        for hit in self.static_payment_p2wpkh {
            for utxo in hit.utxos {
                out.push(RecoveryInput::StaticRemoteKeyP2wpkh {
                    outpoint: outpoint_of(&utxo),
                    value: Amount::from_sat(utxo.value),
                    idx: hit.entry.idx,
                    privkey: hit.entry.secret_key,
                    script_pubkey: hit.entry.p2wpkh_spk.clone(),
                });
            }
        }

        for hit in self.static_payment_anchor {
            let redeem_script = hit.entry.anchor_redeem_script();
            for utxo in hit.utxos {
                out.push(RecoveryInput::StaticRemoteKeyP2wshAnchor {
                    outpoint: outpoint_of(&utxo),
                    value: Amount::from_sat(utxo.value),
                    idx: hit.entry.idx,
                    privkey: hit.entry.secret_key,
                    redeem_script: redeem_script.clone(),
                    script_pubkey: hit.entry.anchor_p2wsh_spk.clone(),
                });
            }
        }

        for hit in self.bip84 {
            for utxo in hit.utxos {
                out.push(RecoveryInput::Bip84 {
                    outpoint: outpoint_of(&utxo),
                    value: Amount::from_sat(utxo.value),
                    chain: hit.entry.chain,
                    idx: hit.entry.idx,
                    privkey: hit.entry.secret_key,
                    script_pubkey: hit.entry.script_pubkey.clone(),
                });
            }
        }

        out
    }

    /// Build a validated [`RecoveryPlan`] from this report.
    pub fn into_plan(
        self,
        destination: Address<NetworkUnchecked>,
        feerate_sat_vb: u64,
    ) -> Result<RecoveryPlan> {
        let network = self.network;
        let inputs = self.into_recovery_inputs();
        RecoveryPlan::new(network, inputs, destination, feerate_sat_vb)
    }
}

fn outpoint_of(utxo: &Utxo) -> OutPoint {
    OutPoint {
        txid: utxo.txid,
        vout: utxo.vout,
    }
}

impl fmt::Display for Derived {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Derived scripts (network: {})", self.network)?;
        writeln!(
            f,
            "  static_payment ({} entries):",
            self.static_entries.len()
        )?;
        for entry in &self.static_entries {
            writeln!(f, "    idx={} p2wpkh={}", entry.idx, entry.p2wpkh_spk)?;
            writeln!(f, "    idx={} anchor={}", entry.idx, entry.anchor_p2wsh_spk)?;
        }
        writeln!(f, "  BIP-84 ({} entries):", self.bip84.len())?;
        for entry in &self.bip84 {
            let chain = match entry.chain {
                Bip84Chain::External => "external",
                Bip84Chain::Internal => "internal",
            };
            writeln!(
                f,
                "    {chain}/{idx} {spk}",
                idx = entry.idx,
                spk = entry.script_pubkey
            )?;
        }
        Ok(())
    }
}

impl fmt::Display for ScanReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Scan report (network: {})", self.network)?;
        write_static_section(f, "static_payment P2WPKH", &self.static_payment_p2wpkh)?;
        write_static_section(
            f,
            "static_payment P2WSH anchor",
            &self.static_payment_anchor,
        )?;
        write_bip84_section(f, &self.bip84)?;
        writeln!(f, "Total recoverable: {}", self.total_value())
    }
}

fn write_static_section(
    f: &mut fmt::Formatter<'_>,
    title: &str,
    hits: &[StaticPaymentHit],
) -> fmt::Result {
    if hits.is_empty() {
        return writeln!(f, "  {title}: no hits");
    }
    writeln!(f, "  {title}:")?;
    for hit in hits {
        for utxo in &hit.utxos {
            writeln!(
                f,
                "    idx={idx} {tx}:{vout} value={value}",
                idx = hit.entry.idx,
                tx = utxo.txid,
                vout = utxo.vout,
                value = Amount::from_sat(utxo.value),
            )?;
        }
    }
    Ok(())
}

fn write_bip84_section(f: &mut fmt::Formatter<'_>, hits: &[Bip84Hit]) -> fmt::Result {
    if hits.is_empty() {
        return writeln!(f, "  BIP-84: no hits");
    }
    writeln!(f, "  BIP-84:")?;
    for hit in hits {
        let chain = match hit.entry.chain {
            Bip84Chain::External => "external",
            Bip84Chain::Internal => "internal",
        };
        for utxo in &hit.utxos {
            writeln!(
                f,
                "    {chain}/{idx} {tx}:{vout} value={value}",
                idx = hit.entry.idx,
                tx = utxo.txid,
                vout = utxo.vout,
                value = Amount::from_sat(utxo.value),
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::esplora::UtxoStatus;
    use bitcoin::hashes::Hash;
    use bitcoin::secp256k1::{PublicKey, Secp256k1};
    use bitcoin::{Address, Txid, WPubkeyHash};
    use std::str::FromStr;

    const LDK_SEED: [u8; 32] = [
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
        0xff, 0x10, 0x32, 0x54, 0x76, 0x98, 0xba, 0xdc, 0xfe, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab,
        0xcd, 0xef,
    ];
    const KNOWN_MNEMONIC: &str = "abandon abandon abandon abandon abandon abandon \
                                  abandon abandon abandon abandon abandon about";

    fn fixture_utxo(byte: u8, value: u64) -> Utxo {
        Utxo {
            txid: Txid::from_byte_array([byte; 32]),
            vout: 0,
            value,
            status: UtxoStatus {
                confirmed: true,
                block_height: Some(800_000),
            },
        }
    }

    fn synthetic_bip84_entry(idx: u32, chain: Bip84Chain) -> Bip84Entry {
        let secp = Secp256k1::new();
        let secret_key = bitcoin::secp256k1::SecretKey::from_slice(&[idx as u8 + 1; 32]).unwrap();
        let public_key = PublicKey::from_secret_key(&secp, &secret_key);
        let script_pubkey = ScriptBuf::new_p2wpkh(&WPubkeyHash::hash(&public_key.serialize()));
        Bip84Entry {
            chain,
            idx,
            secret_key,
            public_key,
            script_pubkey,
        }
    }

    /// Each hit must be paired with its own script's UTXOs. Guards
    /// against silently swapping `(entry_a, utxos_b)`.
    #[test]
    fn build_scan_report_pairs_each_hit_with_its_utxos() {
        let static_entries = static_payment_entries(&LDK_SEED);
        let entry_p2wpkh = static_entries[3].clone();
        let entry_anchor = static_entries[7].clone();
        let bip84_entry = synthetic_bip84_entry(0, Bip84Chain::External);

        let utxo_p2wpkh = fixture_utxo(0xa1, 100_000);
        let utxo_anchor = fixture_utxo(0xa2, 200_000);
        let utxo_bip84 = fixture_utxo(0xa3, 300_000);

        let utxos = vec![
            (entry_p2wpkh.p2wpkh_spk.clone(), vec![utxo_p2wpkh.clone()]),
            (
                entry_anchor.anchor_p2wsh_spk.clone(),
                vec![utxo_anchor.clone()],
            ),
            (bip84_entry.script_pubkey.clone(), vec![utxo_bip84.clone()]),
        ];

        let derived = Derived {
            network: Network::Bitcoin,
            static_entries,
            bip84: vec![bip84_entry],
        };
        let report = build_scan_report(derived, utxos);

        assert_eq!(report.static_payment_p2wpkh.len(), 1);
        assert_eq!(report.static_payment_p2wpkh[0].entry.idx, 3);
        assert_eq!(report.static_payment_p2wpkh[0].utxos, vec![utxo_p2wpkh]);

        assert_eq!(report.static_payment_anchor.len(), 1);
        assert_eq!(report.static_payment_anchor[0].entry.idx, 7);
        assert_eq!(report.static_payment_anchor[0].utxos, vec![utxo_anchor]);

        assert_eq!(report.bip84.len(), 1);
        assert_eq!(report.bip84[0].entry.idx, 0);
        assert_eq!(report.bip84[0].utxos, vec![utxo_bip84]);
    }

    /// Most scans return >2000 empty `(spk, [])` pairs and a
    /// handful of hits. Empties must not surface as hits.
    #[test]
    fn build_scan_report_drops_empty_results() {
        let static_entries = static_payment_entries(&LDK_SEED);
        let bip84 = vec![synthetic_bip84_entry(0, Bip84Chain::External)];

        // Only entry idx 5's P2WPKH script has UTXOs; everything else
        // is empty.
        let mut utxos: Vec<(ScriptBuf, Vec<Utxo>)> = static_entries
            .iter()
            .flat_map(|e| {
                [
                    (e.p2wpkh_spk.clone(), Vec::new()),
                    (e.anchor_p2wsh_spk.clone(), Vec::new()),
                ]
            })
            .chain(std::iter::once((
                bip84[0].script_pubkey.clone(),
                Vec::new(),
            )))
            .collect();
        utxos[10] = (
            static_entries[5].p2wpkh_spk.clone(),
            vec![fixture_utxo(0xb1, 50_000)],
        );

        let derived = Derived {
            network: Network::Bitcoin,
            static_entries,
            bip84,
        };
        let report = build_scan_report(derived, utxos);

        assert_eq!(report.static_payment_p2wpkh.len(), 1);
        assert_eq!(report.static_payment_p2wpkh[0].entry.idx, 5);
        assert!(report.static_payment_anchor.is_empty());
        assert!(report.bip84.is_empty());
    }

    /// Every UTXO produces one `RecoveryInput`, variant matching
    /// the hit's source. The anchor variant's redeem script must
    /// hash to its `script_pubkey`.
    #[test]
    fn into_recovery_inputs_round_trips_every_utxo() {
        use bitcoin::WScriptHash;

        let static_entries = static_payment_entries(&LDK_SEED);
        let p2wpkh_entry = static_entries[1].clone();
        let anchor_entry = static_entries[2].clone();
        let bip84_entry = synthetic_bip84_entry(0, Bip84Chain::Internal);

        let report = ScanReport {
            network: Network::Bitcoin,
            static_payment_p2wpkh: vec![StaticPaymentHit {
                entry: p2wpkh_entry.clone(),
                utxos: vec![fixture_utxo(1, 10_000), fixture_utxo(2, 20_000)],
            }],
            static_payment_anchor: vec![StaticPaymentHit {
                entry: anchor_entry.clone(),
                utxos: vec![fixture_utxo(3, 30_000)],
            }],
            bip84: vec![Bip84Hit {
                entry: bip84_entry.clone(),
                utxos: vec![fixture_utxo(4, 40_000)],
            }],
        };

        let inputs = report.into_recovery_inputs();
        assert_eq!(inputs.len(), 4);

        let mut p2wpkh_count = 0;
        let mut anchor_count = 0;
        let mut bip84_count = 0;
        for input in inputs {
            match input {
                RecoveryInput::StaticRemoteKeyP2wpkh {
                    idx, script_pubkey, ..
                } => {
                    assert_eq!(idx, p2wpkh_entry.idx);
                    assert_eq!(script_pubkey, p2wpkh_entry.p2wpkh_spk);
                    p2wpkh_count += 1;
                }
                RecoveryInput::StaticRemoteKeyP2wshAnchor {
                    idx,
                    redeem_script,
                    script_pubkey,
                    ..
                } => {
                    assert_eq!(idx, anchor_entry.idx);
                    assert_eq!(script_pubkey, anchor_entry.anchor_p2wsh_spk);
                    assert_eq!(
                        ScriptBuf::new_p2wsh(&WScriptHash::hash(redeem_script.as_bytes())),
                        anchor_entry.anchor_p2wsh_spk,
                    );
                    anchor_count += 1;
                }
                RecoveryInput::Bip84 {
                    chain,
                    idx,
                    script_pubkey,
                    ..
                } => {
                    assert_eq!(chain, bip84_entry.chain);
                    assert_eq!(idx, bip84_entry.idx);
                    assert_eq!(script_pubkey, bip84_entry.script_pubkey);
                    bip84_count += 1;
                }
            }
        }
        assert_eq!(p2wpkh_count, 2);
        assert_eq!(anchor_count, 1);
        assert_eq!(bip84_count, 1);
    }

    #[test]
    fn total_value_sums_every_utxo() {
        let static_entries = static_payment_entries(&LDK_SEED);
        let report = ScanReport {
            network: Network::Bitcoin,
            static_payment_p2wpkh: vec![StaticPaymentHit {
                entry: static_entries[0].clone(),
                utxos: vec![fixture_utxo(1, 100), fixture_utxo(2, 200)],
            }],
            static_payment_anchor: vec![StaticPaymentHit {
                entry: static_entries[1].clone(),
                utxos: vec![fixture_utxo(3, 300)],
            }],
            bip84: vec![Bip84Hit {
                entry: synthetic_bip84_entry(0, Bip84Chain::External),
                utxos: vec![fixture_utxo(4, 400)],
            }],
        };
        assert_eq!(report.total_value(), Amount::from_sat(1_000));
    }

    /// `include_anchors=true` must yield `2 * static + bip84`
    /// scripts. A drop here would silently exclude a flavour from
    /// the query.
    #[test]
    fn derived_scripts_cover_every_flavour_when_anchors_included() {
        let mnemonic = Mnemonic::from_str(KNOWN_MNEMONIC).unwrap();
        let derived = derive_all(&mnemonic, Network::Bitcoin);
        let scripts = derived.scripts(true);

        assert_eq!(
            scripts.len(),
            derived.static_entries.len() * 2 + derived.bip84.len()
        );
        assert!(scripts.contains(&derived.static_entries[7].p2wpkh_spk));
        assert!(scripts.contains(&derived.static_entries[42].anchor_p2wsh_spk));
        let target_bip84 = &derived
            .bip84
            .iter()
            .find(|e| e.chain == Bip84Chain::Internal && e.idx == 3)
            .expect("internal/3 exists for default gap_limit")
            .script_pubkey;
        assert!(scripts.contains(target_bip84));
    }

    /// `include_anchors=false` must drop every anchor P2WSH script
    /// while keeping P2WPKH and BIP-84.
    #[test]
    fn derived_scripts_skip_anchors_when_excluded() {
        let mnemonic = Mnemonic::from_str(KNOWN_MNEMONIC).unwrap();
        let derived = derive_all(&mnemonic, Network::Bitcoin);
        let scripts = derived.scripts(false);

        assert_eq!(
            scripts.len(),
            derived.static_entries.len() + derived.bip84.len()
        );
        assert!(scripts.contains(&derived.static_entries[7].p2wpkh_spk));
        for entry in &derived.static_entries {
            assert!(
                !scripts.contains(&entry.anchor_p2wsh_spk),
                "anchor script for idx {} must be skipped",
                entry.idx
            );
        }
    }

    /// End-to-end: a `ScanReport` with one BIP-84 hit must build a
    /// valid `RecoveryPlan` whose single input is the matching
    /// `RecoveryInput::Bip84`.
    #[test]
    fn into_plan_produces_valid_recovery_plan() {
        let mnemonic = Mnemonic::from_str(KNOWN_MNEMONIC).unwrap();
        let (_, master) = ldk_seed_and_master(&mnemonic, Network::Bitcoin);
        let bip84 = bip84_entries(&master, Network::Bitcoin, 1);
        let entry = bip84[0].clone();

        let report = ScanReport {
            network: Network::Bitcoin,
            static_payment_p2wpkh: Vec::new(),
            static_payment_anchor: Vec::new(),
            bip84: vec![Bip84Hit {
                entry,
                utxos: vec![fixture_utxo(0xee, 1_000_000)],
            }],
        };

        let destination = Address::from_str("bc1qcr8te4kr609gcawutmrza0j4xv80jy8z306fyu").unwrap();
        let plan = report.into_plan(destination, 5).expect("valid plan");
        assert_eq!(plan.network, Network::Bitcoin);
        assert_eq!(plan.inputs.len(), 1);
        assert!(matches!(plan.inputs[0], RecoveryInput::Bip84 { .. }));
    }
}
