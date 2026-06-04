# @moneydevkit/recovery

Seed-only sweep tool for [MoneyDevKit] LDK clients. Given the MDK
mnemonic of a node, it enumerates every on-chain output the seed can
claim, queries an esplora endpoint for matching UTXOs, builds a single
signed sweep transaction, and optionally broadcasts it.

[MoneyDevKit]: https://moneydevkit.com

```
npx @moneydevkit/recovery
```

That bare invocation runs the full interactive flow against mainnet:
it prompts for the mnemonic (no echo), scans for recoverable funds,
prompts for a destination address, prints a To / Fee / Net summary,
and broadcasts after a `[y/N]` confirmation.

## What it recovers

- **`to_remote` outputs from LSP force-closes** of v2
  static_remote_key channels — both the P2WPKH (non-anchor) and
  P2WSH anchor commitment-output flavours, across the 1000
  enumerable key indices.
- **BIP-84 on-chain funds** at `m/84h/{0,1}h/0h/{0,1}/i` with the
  default 20-address gap limit per chain.

Both are pure derivations from the seed — no prior wallet state,
channel monitor, or VSS access required.

## What it does not recover

- **Our-side force-close outputs** (`to_local`, HTLC, justice). These
  need the encrypted `ChannelMonitor` from VSS; a future VSS-backed
  mode will cover them.
- **Channels opened by ldk-node ≤ 0.6.x** (v1 per-channel script, not
  enumerable from seed).
- **In-flight HTLCs at the time of close.**

If the seed is gone, nothing recovers anything.

## Integrity

Published from CI with provenance, so you can confirm where the
package came from:

```
npm audit signatures
```

## Source, docs, and from-source builds

Full documentation, the derivation paths, the regtest test harness,
and `cargo install` instructions live in the repository:
<https://github.com/moneydevkit/mdk-recovery>.
