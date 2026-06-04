#!/usr/bin/env node
// Locates the platform-specific binary subpackage installed via
// `optionalDependencies`, checks its SHA-256 against the manifest
// shipped in this meta package, and execs it. A mismatch aborts
// loudly before the binary runs.
//
// This check is tamper-evidence within the install closure: it
// confirms the resolved platform binary matches what this meta
// package was published with. The trust anchor that lives OUTSIDE
// the tarball is npm provenance — these packages are published from
// CI via trusted publishing with a signed provenance attestation
// (`npm audit signatures`). There is deliberately no embedded-pubkey
// minisign check here: a signature verified against a key shipped in
// the same tarball it signs would add nothing over this hash check.
'use strict';

const fs = require('node:fs');
const path = require('node:path');
const crypto = require('node:crypto');
const { execFileSync } = require('node:child_process');

// One package per supported platform. The binary inside each
// subpackage is named after its triple so a single SHA256SUMS
// manifest can disambiguate every entry by basename.
const PLATFORMS = {
  'darwin-arm64': '@moneydevkit/recovery-darwin-arm64',
  'darwin-x64': '@moneydevkit/recovery-darwin-x64',
  'linux-arm64': '@moneydevkit/recovery-linux-arm64',
  'linux-x64': '@moneydevkit/recovery-linux-x64',
  'win32-x64': '@moneydevkit/recovery-win32-x64',
};

function platformKey() {
  const arch = process.arch === 'arm64' ? 'arm64' : 'x64';
  return `${process.platform}-${arch}`;
}

function resolveBinary() {
  const key = platformKey();
  const pkg = PLATFORMS[key];
  if (!pkg) {
    throw new Error(
      `unsupported platform ${key}; supported: ${Object.keys(PLATFORMS).join(', ')}`,
    );
  }
  const ext = process.platform === 'win32' ? '.exe' : '';
  try {
    return require.resolve(`${pkg}/bin/mdk-recovery-${key}${ext}`);
  } catch (e) {
    throw new Error(
      `platform package ${pkg} not installed; check that npm did not skip optional dependencies`,
    );
  }
}

function expectedHash(manifestPath, binaryPath) {
  const manifest = fs.readFileSync(manifestPath, 'utf8');
  const fileName = path.basename(binaryPath);
  const entry = manifest
    .split('\n')
    .map(line => line.trim())
    .filter(line => line && !line.startsWith('#'))
    .map(line => line.split(/\s+/))
    .find(([, name]) => name === fileName);
  if (!entry) {
    throw new Error(`manifest has no entry for ${fileName}`);
  }
  return entry[0];
}

function actualHash(binaryPath) {
  return crypto.createHash('sha256').update(fs.readFileSync(binaryPath)).digest('hex');
}

function main() {
  const binary = resolveBinary();
  const manifest = path.join(__dirname, '..', 'manifest', 'SHA256SUMS');
  const expected = expectedHash(manifest, binary);
  const actual = actualHash(binary);
  if (actual !== expected) {
    throw new Error(
      `SHA-256 mismatch for ${path.basename(binary)}: expected ${expected}, got ${actual}`,
    );
  }
  execFileSync(binary, process.argv.slice(2), { stdio: 'inherit' });
}

try {
  main();
} catch (e) {
  if (e.status !== undefined) {
    process.exit(e.status);
  }
  process.stderr.write(`mdk-recovery: ${e.message}\n`);
  process.exit(1);
}
