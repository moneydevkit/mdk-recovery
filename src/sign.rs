//! Sign a `RecoveryPlan` into a broadcast-ready `Transaction`.
//!
//! Pure: takes a validated plan, returns a fully-witnessed
//! single-output sweep tx. Two witness shapes are emitted:
//!
//! - P2WPKH (BIP-84 + non-anchor `to_remote`): `[sig, pubkey]`,
//!   `nSequence = 0xfffffffd` so the sweep is RBF-able (BIP-125).
//! - P2WSH anchor `to_remote`: `[sig, redeem_script]`, `nSequence = 1`
//!   to satisfy the redeem's `1 OP_CHECKSEQUENCEVERIFY`. RBF is
//!   forfeited on this variant.
//!
//! Sighash is BIP-143 SIGHASH_ALL throughout, computed via
//! `SighashCache` for `O(n)` digesting across all inputs.

use bitcoin::absolute::LockTime;
use bitcoin::ecdsa;
use bitcoin::secp256k1::{All, Message, PublicKey, Secp256k1};
use bitcoin::sighash::{EcdsaSighashType, SighashCache};
use bitcoin::transaction::Version;
use bitcoin::{ScriptBuf, Sequence, Transaction, TxIn, TxOut, Witness};

use crate::plan::{RecoveryInput, RecoveryPlan};

/// BIP-125 opt-in RBF: any `nSequence < 0xfffffffe` signals replaceability.
const RBF_NSEQUENCE: Sequence = Sequence(0xfffffffd);

/// Anchor `to_remote` redeems require `nSequence >= 1` to clear
/// `1 OP_CHECKSEQUENCEVERIFY`. Use the minimum so the sweep confirms
/// in the next block once mined.
const ANCHOR_NSEQUENCE: Sequence = Sequence(1);

/// Sign every input of `plan` and return the broadcast-ready tx.
pub fn sign_plan(plan: &RecoveryPlan) -> Transaction {
    let secp = Secp256k1::new();

    let input = plan
        .inputs
        .iter()
        .map(|inp| TxIn {
            previous_output: inp.outpoint(),
            script_sig: ScriptBuf::new(),
            sequence: nsequence_for(inp),
            witness: Witness::new(),
        })
        .collect();

    let mut tx = Transaction {
        version: Version::TWO,
        lock_time: LockTime::ZERO,
        input,
        output: vec![TxOut {
            value: plan.estimated_output,
            script_pubkey: plan.destination.script_pubkey(),
        }],
    };

    let witnesses: Vec<Witness> = {
        let mut cache = SighashCache::new(&tx);
        plan.inputs
            .iter()
            .enumerate()
            .map(|(i, inp)| witness_for(&secp, &mut cache, i, inp))
            .collect()
    };

    for (i, witness) in witnesses.into_iter().enumerate() {
        tx.input[i].witness = witness;
    }
    tx
}

fn nsequence_for(inp: &RecoveryInput) -> Sequence {
    match inp {
        RecoveryInput::Bip84 { .. } | RecoveryInput::StaticRemoteKeyP2wpkh { .. } => RBF_NSEQUENCE,
        RecoveryInput::StaticRemoteKeyP2wshAnchor { .. } => ANCHOR_NSEQUENCE,
    }
}

fn witness_for(
    secp: &Secp256k1<All>,
    cache: &mut SighashCache<&Transaction>,
    input_index: usize,
    inp: &RecoveryInput,
) -> Witness {
    match inp {
        RecoveryInput::Bip84 {
            value,
            privkey,
            script_pubkey,
            ..
        }
        | RecoveryInput::StaticRemoteKeyP2wpkh {
            value,
            privkey,
            script_pubkey,
            ..
        } => {
            let sighash = cache
                .p2wpkh_signature_hash(input_index, script_pubkey, *value, EcdsaSighashType::All)
                .expect("input was validated as P2WPKH at plan construction");
            let signature =
                ecdsa::Signature::sighash_all(secp.sign_ecdsa(&Message::from(sighash), privkey));
            let pubkey = PublicKey::from_secret_key(secp, privkey);
            let mut witness = Witness::new();
            witness.push_ecdsa_signature(&signature);
            witness.push(pubkey.serialize());
            witness
        }
        RecoveryInput::StaticRemoteKeyP2wshAnchor {
            value,
            privkey,
            redeem_script,
            ..
        } => {
            let sighash = cache
                .p2wsh_signature_hash(input_index, redeem_script, *value, EcdsaSighashType::All)
                .expect("input index is in range by construction");
            let signature =
                ecdsa::Signature::sighash_all(secp.sign_ecdsa(&Message::from(sighash), privkey));
            let mut witness = Witness::new();
            witness.push_ecdsa_signature(&signature);
            witness.push(redeem_script.as_bytes());
            witness
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::derive::bip84::Bip84Chain;
    use crate::plan::{RecoveryInput, RecoveryPlan};
    use bitcoin::hashes::Hash;
    use bitcoin::secp256k1::SecretKey;
    use bitcoin::{
        Address, Amount, Network, OutPoint, ScriptBuf, Txid, WPubkeyHash, WScriptHash, address,
        opcodes, script::Builder,
    };
    use std::str::FromStr;

    /// Re-run the signer's sighash and verify each witness against
    /// the known pubkey. This catches sighash-preimage bugs (wrong
    /// script_code, wrong amount, wrong sighash type) and witness-
    /// shape bugs (wrong order, missing items, wrong sig encoding).
    fn assert_inputs_verify(plan: &RecoveryPlan, tx: &Transaction) {
        let secp = secp_signing();
        let mut cache = SighashCache::new(tx);

        for (i, inp) in plan.inputs.iter().enumerate() {
            let witness = &tx.input[i].witness;
            assert_eq!(witness.len(), 2, "witness must have exactly two items");

            let sig = ecdsa::Signature::from_slice(&witness[0]).expect("DER+sighash");
            assert_eq!(sig.sighash_type, EcdsaSighashType::All);

            match inp {
                RecoveryInput::Bip84 {
                    value,
                    privkey,
                    script_pubkey,
                    ..
                }
                | RecoveryInput::StaticRemoteKeyP2wpkh {
                    value,
                    privkey,
                    script_pubkey,
                    ..
                } => {
                    let pubkey = PublicKey::from_secret_key(&secp, privkey);
                    assert_eq!(
                        witness[1],
                        pubkey.serialize(),
                        "second witness item must be the pubkey"
                    );
                    assert_eq!(
                        *script_pubkey,
                        ScriptBuf::new_p2wpkh(&WPubkeyHash::hash(&pubkey.serialize())),
                        "spk must commit to HASH160(pubkey)"
                    );
                    let sighash = cache
                        .p2wpkh_signature_hash(i, script_pubkey, *value, EcdsaSighashType::All)
                        .expect("p2wpkh sighash");
                    secp.verify_ecdsa(&Message::from(sighash), &sig.signature, &pubkey)
                        .expect("ECDSA signature must verify");
                }
                RecoveryInput::StaticRemoteKeyP2wshAnchor {
                    value,
                    privkey,
                    redeem_script,
                    script_pubkey,
                    ..
                } => {
                    assert_eq!(
                        &witness[1],
                        redeem_script.as_bytes(),
                        "second witness item must be the redeem script"
                    );
                    assert_eq!(
                        *script_pubkey,
                        ScriptBuf::new_p2wsh(&WScriptHash::hash(redeem_script.as_bytes())),
                        "spk must commit to SHA256(redeem)"
                    );
                    assert_eq!(
                        tx.input[i].sequence, ANCHOR_NSEQUENCE,
                        "anchor inputs must satisfy 1 OP_CSV"
                    );
                    let pubkey = PublicKey::from_secret_key(&secp, privkey);
                    let sighash = cache
                        .p2wsh_signature_hash(i, redeem_script, *value, EcdsaSighashType::All)
                        .expect("p2wsh sighash");
                    secp.verify_ecdsa(&Message::from(sighash), &sig.signature, &pubkey)
                        .expect("ECDSA signature must verify");
                }
            }
        }
    }

    fn secp_signing() -> Secp256k1<All> {
        Secp256k1::new()
    }

    fn fixture_privkey(byte: u8) -> SecretKey {
        SecretKey::from_slice(&[byte; 32]).expect("valid secret key")
    }

    fn fixture_pubkey(privkey: &SecretKey) -> PublicKey {
        PublicKey::from_secret_key(&secp_signing(), privkey)
    }

    fn p2wpkh_spk_for(privkey: &SecretKey) -> ScriptBuf {
        ScriptBuf::new_p2wpkh(&WPubkeyHash::hash(&fixture_pubkey(privkey).serialize()))
    }

    fn anchor_redeem(privkey: &SecretKey) -> ScriptBuf {
        Builder::new()
            .push_slice(fixture_pubkey(privkey).serialize())
            .push_opcode(opcodes::all::OP_CHECKSIGVERIFY)
            .push_int(1)
            .push_opcode(opcodes::all::OP_CSV)
            .into_script()
    }

    fn anchor_p2wsh_spk(privkey: &SecretKey) -> ScriptBuf {
        ScriptBuf::new_p2wsh(&WScriptHash::hash(anchor_redeem(privkey).as_bytes()))
    }

    fn fixture_outpoint(vout: u32) -> OutPoint {
        OutPoint {
            txid: Txid::all_zeros(),
            vout,
        }
    }

    fn destination() -> Address<address::NetworkUnchecked> {
        Address::from_str("bc1qcr8te4kr609gcawutmrza0j4xv80jy8z306fyu").unwrap()
    }

    /// Build a single-input plan, sign it, and assert the witness
    /// verifies against the BIP-143 sighash for that input.
    fn assert_signed_plan_verifies(input: RecoveryInput) {
        let plan =
            RecoveryPlan::new(Network::Bitcoin, vec![input], destination(), 5).expect("valid plan");
        let tx = sign_plan(&plan);
        assert_inputs_verify(&plan, &tx);
    }

    #[test]
    fn signs_bip84_input() {
        let privkey = fixture_privkey(0x11);
        let input = RecoveryInput::Bip84 {
            outpoint: fixture_outpoint(0),
            value: Amount::from_sat(100_000),
            chain: Bip84Chain::External,
            idx: 0,
            privkey,
            script_pubkey: p2wpkh_spk_for(&privkey),
        };
        assert_signed_plan_verifies(input);
    }

    #[test]
    fn signs_static_remote_key_p2wpkh_input() {
        let privkey = fixture_privkey(0x22);
        let input = RecoveryInput::StaticRemoteKeyP2wpkh {
            outpoint: fixture_outpoint(0),
            value: Amount::from_sat(100_000),
            idx: 7,
            privkey,
            script_pubkey: p2wpkh_spk_for(&privkey),
        };
        assert_signed_plan_verifies(input);
    }

    #[test]
    fn signs_static_remote_key_anchor_input() {
        let privkey = fixture_privkey(0x33);
        let input = RecoveryInput::StaticRemoteKeyP2wshAnchor {
            outpoint: fixture_outpoint(0),
            value: Amount::from_sat(100_000),
            idx: 9,
            privkey,
            redeem_script: anchor_redeem(&privkey),
            script_pubkey: anchor_p2wsh_spk(&privkey),
        };
        assert_signed_plan_verifies(input);
    }

    #[test]
    fn signs_mixed_inputs() {
        let bip84_pk = fixture_privkey(0x44);
        let srk_pk = fixture_privkey(0x55);
        let anchor_pk = fixture_privkey(0x66);

        let inputs = vec![
            RecoveryInput::Bip84 {
                outpoint: fixture_outpoint(0),
                value: Amount::from_sat(50_000),
                chain: Bip84Chain::External,
                idx: 0,
                privkey: bip84_pk,
                script_pubkey: p2wpkh_spk_for(&bip84_pk),
            },
            RecoveryInput::StaticRemoteKeyP2wpkh {
                outpoint: fixture_outpoint(1),
                value: Amount::from_sat(60_000),
                idx: 3,
                privkey: srk_pk,
                script_pubkey: p2wpkh_spk_for(&srk_pk),
            },
            RecoveryInput::StaticRemoteKeyP2wshAnchor {
                outpoint: fixture_outpoint(2),
                value: Amount::from_sat(70_000),
                idx: 5,
                privkey: anchor_pk,
                redeem_script: anchor_redeem(&anchor_pk),
                script_pubkey: anchor_p2wsh_spk(&anchor_pk),
            },
        ];

        let plan =
            RecoveryPlan::new(Network::Bitcoin, inputs, destination(), 5).expect("valid plan");
        let tx = sign_plan(&plan);
        assert_inputs_verify(&plan, &tx);
    }

    #[test]
    fn anchor_input_uses_csv_nsequence() {
        let privkey = fixture_privkey(0x77);
        let plan = RecoveryPlan::new(
            Network::Bitcoin,
            vec![RecoveryInput::StaticRemoteKeyP2wshAnchor {
                outpoint: fixture_outpoint(0),
                value: Amount::from_sat(100_000),
                idx: 0,
                privkey,
                redeem_script: anchor_redeem(&privkey),
                script_pubkey: anchor_p2wsh_spk(&privkey),
            }],
            destination(),
            5,
        )
        .expect("valid plan");

        let tx = sign_plan(&plan);
        assert_eq!(tx.input[0].sequence, ANCHOR_NSEQUENCE);
    }

    #[test]
    fn p2wpkh_inputs_use_rbf_nsequence() {
        let privkey = fixture_privkey(0x88);
        let plan = RecoveryPlan::new(
            Network::Bitcoin,
            vec![RecoveryInput::Bip84 {
                outpoint: fixture_outpoint(0),
                value: Amount::from_sat(100_000),
                chain: Bip84Chain::External,
                idx: 0,
                privkey,
                script_pubkey: p2wpkh_spk_for(&privkey),
            }],
            destination(),
            5,
        )
        .expect("valid plan");

        let tx = sign_plan(&plan);
        assert_eq!(tx.input[0].sequence, RBF_NSEQUENCE);
    }

    #[test]
    fn output_matches_plan_estimate() {
        let privkey = fixture_privkey(0x99);
        let plan = RecoveryPlan::new(
            Network::Bitcoin,
            vec![RecoveryInput::Bip84 {
                outpoint: fixture_outpoint(0),
                value: Amount::from_sat(123_456),
                chain: Bip84Chain::External,
                idx: 0,
                privkey,
                script_pubkey: p2wpkh_spk_for(&privkey),
            }],
            destination(),
            5,
        )
        .expect("valid plan");

        let tx = sign_plan(&plan);
        assert_eq!(tx.output.len(), 1);
        assert_eq!(tx.output[0].value, plan.estimated_output);
        assert_eq!(tx.output[0].script_pubkey, plan.destination.script_pubkey());
        assert_eq!(tx.version, Version::TWO);
        assert_eq!(tx.lock_time, LockTime::ZERO);
    }
}
