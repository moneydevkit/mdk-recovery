# @moneydevkit/recovery

`npx @moneydevkit/recovery` invokes the prebuilt `mdk-recovery` binary
matching the host platform. The binary itself ships in a sibling
package (`@moneydevkit/recovery-{os}-{cpu}`) selected via
`optionalDependencies`; the meta package's `bin/cli.js` resolves it,
verifies its SHA-256 against `manifest/SHA256SUMS`, and execs it.

The manifest is signed with minisign at release time; runtime
verification of that signature is a follow-up once the release-key
management story is finalised. Until then the SHA-256 check still
pins the binary against the manifest npm distributed.

## Building locally

The npm packages are not built by `cargo` or `nix`. The CI release
workflow runs the platform-matrix builds, writes each binary into
`npm/platform/<os>-<cpu>/bin/`, generates the manifest, signs it,
and publishes both the meta and platform packages. Local
development uses `cargo install --git ...` instead.
