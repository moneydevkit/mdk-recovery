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

## Installing

```
npx @moneydevkit/recovery
```

Pulls the prebuilt binary for your platform from npm and runs it. Or
build from source (ops, auditors):

```
cargo install --git https://github.com/moneydevkit/mdk-recovery
```

## Verifying a download

Every release is built and published by the project's CI, with a
verifiable record of where it came from.

- Installed from npm: `npm audit signatures`
- Downloaded from GitHub releases:
  `gh attestation verify <binary> --repo moneydevkit/mdk-recovery`

## Usage

Every flag is optional. The bare invocation `mdk-recovery` runs the
full interactive flow against mainnet.

```
mdk-recovery [--network bitcoin|testnet|signet|regtest]   default: bitcoin
             [--to <address>]                             else prompted
             [--feerate-sat-vb <n>]                       default: 5
             [--scan-anchors]                             opt-in
             [--mnemonic-stdin]                           force stdin read
             [--yes]                                      skip confirmation
             [--verbose]                                  per-input listing
             [--json]                                     scripted broadcast
```

One flat binary, no subcommands. On a TTY the run is:

1. Prompt for the mnemonic with no echo (via `rpassword`).
2. Scan the network's esplora for matching UTXOs.
3. Print `Found N output(s) totalling X BTC.`
4. Prompt `Where to? (onchain address): ` unless `--to` was given.
5. Print the To / Fee / Net summary block.
6. Prompt `Broadcast? [y/N] ` unless `--yes` was given.
7. Broadcast and print the mempool URL.

`--scan-anchors` extends the scan to the 1000 P2WSH anchor scripts.
Off by default since MDK does not open anchor channels; turning it
on roughly doubles the scan time.

`--verbose` appends a per-input listing (outpoint, source, value)
to the summary block. This is what an auditor reaches for when
they want to see exactly which UTXOs feed the sweep.

Off a TTY, or with `--mnemonic-stdin` on a TTY, the binary reads
the mnemonic from stdin to EOF instead of prompting. This is the
scripted / CI path.

`--json` emits one JSON object to stdout after broadcast
(`txid`, `raw_hex`, `explorer_url`). It is strictly
non-interactive: requires `--to`, `--yes`, and a mnemonic on stdin
(enforced at runtime so a TTY caller can't fall into the hidden
prompt by accident). All progress, prompts, and the human summary
go to stderr so the JSON stays pipeable.

### Confirmation

The `[y/N]` after the summary block is the only gate. The
destination renders in plain context above it; the user reads
back what they pasted and types `y`. Anything other than `y` or
`yes` (case insensitive) aborts cleanly with no broadcast, which
also covers the dry-run case: an auditor who wants to inspect the
plan without broadcasting just declines.

## Endpoints

Per-network esplora URLs are hard-coded in `cli::endpoint_for`:

| Network | Endpoint |
|---|---|
| Bitcoin | `https://esplora.moneydevkit.com/api` |
| Signet | `https://mutinynet.com/api` (the MDK target signet, not vanilla) |
| Testnet | `https://blockstream.info/testnet/api` |
| Regtest | `$MDK_RECOVERY_ESPLORA_URL` (test harness only) |

There is no CLI flag for arbitrary endpoint override — picking the
wrong one is a footgun (silently redirecting mainnet traffic) and
the per-network endpoints rarely move.

## Build and test

`just check` runs `nix flake check` which runs format/lint checks and tests.
The regtest integration tests spawn a real bitcoind via `corepc-node`
(binary path pinned through `BITCOIND_EXE`, fed by the nix flake's pinned `bitcoind`)
behind a small in-process esplora-shaped HTTP shim, fund the derived scripts
directly via bitcoind RPC, run the binary as a subprocess, and
assert the funds moved.

## Releasing

```
just release X.Y.Z        # branch + bump + commit + push + open PR
# review the release/vX.Y.Z PR and merge it
```

Merging a `release/*` PR is the whole release. CI reads the version
from `Cargo.toml`, creates and pushes the matching `vX.Y.Z` tag, builds
every platform binary at that commit, attests them, cuts the GitHub
release, and publishes to npm. There is no hand-pushed tag, so the tag
and the published version can't drift apart.

Pre-releases use a semver hyphen (`X.Y.Z-rc.1`, `X.Y.Z-beta.0`) and
publish under the npm `next` dist-tag.

## Deleting a bad release

If a release published broken artifacts, tear it down before cutting a
new one:

```
gh release delete vX.Y.Z --yes    # GitHub release
git push --delete origin vX.Y.Z   # remote tag
git tag -d vX.Y.Z                 # local tag
```

npm versions are immutable: you cannot republish `X.Y.Z` once it is
live, and unpublishing is heavily restricted. So a redo always means a
new version (bump the patch or the `-rc.N`), not the same one again.

If the failure was caught mid-run before npm published, you can keep
the version: delete the GitHub release and tag as above, fix the
problem, and re-merge the `release/*` PR (or re-point the tag at the
fixed commit).
