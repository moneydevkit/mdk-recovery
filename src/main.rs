//! Flat interactive `mdk-recovery` binary.
//!
//! One run, one job: read a mnemonic, scan the seed's enumerable
//! scripts via the per-network esplora, and (after a `[y/N]`
//! summary block) broadcast a sweep to a destination address.

use std::io::{self, IsTerminal, Write};

use bitcoin::Address;
use bitcoin::address::NetworkUnchecked;
use clap::Parser;
use esplora_client::{AsyncClient, Builder};
use mdk_recovery::cli::endpoint_for;
use mdk_recovery::error::fmt_error_chain;
use mdk_recovery::interactive::{
    explorer_tx_url, prompt_confirm, prompt_destination, prompt_mnemonic,
    render_destination_summary, render_found_line,
};
use mdk_recovery::plan::DEFAULT_FEERATE_SAT_VB;
use mdk_recovery::scan::ScanReport;
use mdk_recovery::scan::esplora::broadcast;
use mdk_recovery::sign::SignedSweep;
use mdk_recovery::{RecoveryError, Result, scan};
use serde::Serialize;

#[derive(Parser)]
#[command(name = "mdk-recovery", version, about = "Seed-only LDK recovery")]
struct Cli {
    /// Bitcoin network to derive on and scan against.
    #[arg(
        long,
        value_parser = parse_network,
        default_value_t = bitcoin::Network::Bitcoin,
    )]
    network: bitcoin::Network,

    /// Destination address. Required under `--json`; on a TTY,
    /// omitting it triggers an interactive prompt instead.
    #[arg(long)]
    to: Option<Address<NetworkUnchecked>>,

    /// Feerate in sat/vB. Five is a moderate default that confirms
    /// in a few blocks under typical mempool load; raise it for a
    /// time-sensitive recovery.
    #[arg(long, default_value_t = DEFAULT_FEERATE_SAT_VB)]
    feerate_sat_vb: u64,

    /// Also query the 1000 static_payment P2WSH anchor scripts.
    /// Off by default since MDK does not open anchor channels;
    /// turning this on roughly doubles the scan time.
    #[arg(long)]
    scan_anchors: bool,

    /// Read the mnemonic from stdin to EOF instead of prompting on
    /// the controlling TTY. The CI / scripted path.
    #[arg(long)]
    mnemonic_stdin: bool,

    /// Skip the `[y/N]` confirmation. Required under `--json`.
    #[arg(long)]
    yes: bool,

    /// Print the per-input listing alongside the summary block.
    #[arg(long)]
    verbose: bool,

    /// Emit a single JSON object on stdout describing the broadcast
    /// transaction. Implies non-interactive: requires `--to`,
    /// `--yes`, and a mnemonic on stdin (the last is enforced at
    /// runtime since clap cannot see TTY state).
    #[arg(long, requires = "to", requires = "yes")]
    json: bool,
}

/// Stdout payload for `--json`. Matches the shape promised in
/// PLAN.md: txid + raw hex + (best-effort) explorer URL.
#[derive(Serialize)]
struct JsonReport {
    txid: String,
    raw_hex: String,
    explorer_url: Option<String>,
}

fn main() {
    let cli = Cli::parse();
    if let Err(msg) = validate_runtime_invariants(&cli) {
        eprintln!("error: {msg}");
        std::process::exit(2);
    }
    if let Err(e) = tokio_runtime().and_then(|rt| rt.block_on(run(cli))) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

/// Catch the one invariant clap cannot: `--json` plus a TTY stdin
/// without `--mnemonic-stdin` would silently fall into the hidden
/// prompt. The static `--json` ⇒ `--to` / `--yes` requirements are
/// enforced by clap's `requires` attribute on the field.
fn validate_runtime_invariants(cli: &Cli) -> std::result::Result<(), &'static str> {
    if cli.json && !cli.mnemonic_stdin && io::stdin().is_terminal() {
        return Err("--json requires the mnemonic on stdin (use --mnemonic-stdin or pipe it in)");
    }
    Ok(())
}

async fn run(cli: Cli) -> Result<()> {
    let mnemonic = prompt_mnemonic(cli.mnemonic_stdin)?;
    let client = build_client(cli.network)?;
    eprintln!("Scanning {}...", cli.network);
    let report = scan::run(&client, &mnemonic, cli.network, cli.scan_anchors).await?;

    if is_empty(&report) {
        eprintln!("Nothing to recover on {}.", cli.network);
        return Ok(());
    }

    if !cli.json {
        render_found_line(
            report.input_count(),
            report.total_value(),
            io::stderr().lock(),
        )
        .map_err(RecoveryError::from)?;
    }

    let destination = resolve_destination(&cli)?;
    let plan = report.into_plan(destination, cli.feerate_sat_vb)?;

    if !cli.json {
        render_destination_summary(&plan, cli.verbose, io::stderr().lock())
            .map_err(RecoveryError::from)?;
    }

    if !cli.yes && !prompt_confirm("Broadcast?")? {
        eprintln!("Aborted; no transaction broadcast.");
        return Ok(());
    }

    let (sweep, tx) = SignedSweep::from_plan(plan);
    broadcast(&client, &tx).await?;
    let explorer = explorer_tx_url(cli.network, sweep.txid);

    if cli.json {
        let payload = JsonReport {
            txid: sweep.txid.to_string(),
            raw_hex: sweep.raw_hex,
            explorer_url: explorer,
        };
        let json = serde_json::to_string_pretty(&payload)
            .map_err(|e| RecoveryError::Io(io::Error::other(e)))?;
        println!("{json}");
    } else {
        let mut out = io::stdout().lock();
        match explorer {
            Some(url) => writeln!(out, "Done. Track at: {url}").map_err(RecoveryError::from)?,
            None => writeln!(out, "Done. txid: {}", sweep.txid).map_err(RecoveryError::from)?,
        }
    }
    Ok(())
}

/// `ScanReport` carries three vecs of hits; empty across all three
/// means there's nothing to sweep. We check structure instead of
/// `total_value()` so a (defensive) zero-value UTXO would still
/// trigger a plan attempt and surface the dust failure cleanly.
fn is_empty(report: &ScanReport) -> bool {
    report.static_payment_p2wpkh.is_empty()
        && report.static_payment_anchor.is_empty()
        && report.bip84.is_empty()
}

/// Use `--to` if it's set; otherwise prompt. The prompt validates
/// the network up front so the user sees `network mismatch` before
/// the plan tries to build.
fn resolve_destination(cli: &Cli) -> Result<Address<NetworkUnchecked>> {
    if let Some(addr) = cli.to.clone() {
        return Ok(addr);
    }
    Ok(prompt_destination(cli.network)?.into_unchecked())
}

fn build_client(network: bitcoin::Network) -> Result<AsyncClient> {
    let url = endpoint_for(network)?;
    Builder::new(&url)
        .build_async()
        .map_err(|e| RecoveryError::Esplora(fmt_error_chain(&e)))
}

fn parse_network(raw: &str) -> std::result::Result<bitcoin::Network, String> {
    raw.parse()
        .map_err(|e: bitcoin::network::ParseNetworkError| e.to_string())
}

fn tokio_runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(RecoveryError::from)
}
