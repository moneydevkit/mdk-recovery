# mdk-recovery

Seed-only sweep tool for [MoneyDevKit] LDK clients. Given the MDK
mnemonic of a node, `mdk-recovery` enumerates every on-chain output
the seed can claim, queries an esplora endpoint for matching UTXOs,
builds a single signed sweep transaction, and optionally broadcasts it.

[MoneyDevKit]: https://moneydevkit.com

## What it recovers

- **`to_remote` outputs from LSP force-closes** of v2
  static_remote_key channels. LDK 0.2 derives the counterparty
  payment key off `m/8h/idx_h` from the inner xprv built on top of
  the LDK seed; the 1000 enumerable indices cover every channel a
  client has ever held in either commitment-output flavour:
  - P2WPKH (non-anchor channels)
  - P2WSH wrapping `<pubkey> OP_CHECKSIGVERIFY 1 OP_CSV` (anchor
    channels)
- **BIP-84 on-chain funds** at `m/84h/{0,1}h/0h/{0,1}/i` with the
  default 20-address gap limit per chain.

That's it. Both paths are pure derivations from the seed — no
prior wallet state, channel monitor, or VSS access required.

## What it does not recover

- **Our-side force-close outputs** (`to_local` after `to_self_delay`,
  HTLC outputs, justice transactions). These need the
  per-channel-derived `delayed_payment_basepoint` and HTLC preimages
  from the encrypted `ChannelMonitor` in VSS. A future
  VSS-backed recovery mode will cover them.
- **Channels opened by ldk-node ≤ 0.6.x.** Their `to_remote` sits at
  a per-channel-derived v1 script that is not enumerable from seed.
  Operators rotate these off cooperatively before relying on this
  tool.
- **In-flight HTLCs at the time of close.** Same VSS dependency as
  our-side force-close.

If the seed is gone, nothing recovers anything.

## Subcommands

All four take `--mnemonic-file <path>` (`-` reads stdin) and
`--network {bitcoin,testnet,signet,regtest}`.

| Subcommand | Effect |
|---|---|
| `derive` | Print every script the seed can claim. No I/O. |
| `scan` | Query the esplora endpoint for matching UTXOs. Read-only. |
| `plan` | Scan and render the signed-sweep plan. No signing, no broadcast. |
| `sweep` | Build, sign, and (with `--broadcast`) submit the sweep transaction. |

`sweep` and `plan` accept `--to <address>` and
`--feerate-sat-vb <n>` (default 5). Every reporting subcommand
supports `--json` for pretty-printed JSON suitable for `jq`. Secret
keys are stripped from the JSON output unconditionally.

### Broadcast confirmation flow

`sweep --broadcast` prints the destination to stderr and prompts on
stdin to retype it. The reply is constant-time compared with the
flag value; a mismatch aborts before broadcast. The prompt is on
stderr so JSON output to stdout stays pipeable. The check is
defence against a typo in `--to`, not against an adversary with
shell access — by then the seed is already in their hands.

## Endpoints

Mainnet, testnet, and signet hard-code blockstream.info. Regtest
reads `MDK_RECOVERY_ESPLORA_URL` from the environment so test
harnesses can point the binary at a local indexer; shipped binaries
have no use for regtest. There is no CLI flag for arbitrary
endpoint override — choosing one is a footgun (silently
redirecting mainnet traffic) and the public endpoints have been
stable for a decade.

## Build and test

`just check` runs `nix flake check` which runs format/lint checks and tests.
The regtest integration tests spawn a real bitcoind via `corepc-node`
(binary path pinned through `BITCOIND_EXE`, fed by the nix flake's pinned `bitcoind`)
behind a small in-process esplora-shaped HTTP shim, fund the derived scripts
directly via bitcoind RPC, run the binary as a subprocess, and
assert the funds moved.

## Version requirements

- Rust 2024 edition (rustc 1.85+).
- Bitcoin Core 29.0 for the regtest harness (the version
  `corepc-node`'s `29_0` feature targets). The nix flake pins
  this; ad-hoc builds must match.
- The 1000 v2 static_payment scripts are pinned byte-for-byte
  against `lightning::sign::KeysManager::possible_v2_counterparty_closed_balance_spks`
  in `lightning = 0.2`.
