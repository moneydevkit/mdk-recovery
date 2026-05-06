use clap::{Parser, Subcommand};
use mdk_recovery::{RecoveryError, Result};

#[derive(Parser)]
#[command(name = "mdk-recovery", version, about = "Seed-only LDK recovery")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print 1000 P2WPKH + 1000 P2WSH-anchor static_payment scripts and
    /// BIP-84 addresses with derivation indices. No I/O.
    Derive,
    /// Pure scan via the per-network esplora endpoint. Read-only.
    Scan,
    /// Build a recovery plan and render JSON + human summary.
    Plan,
    /// Build, sign, and (with `--broadcast`) submit the sweep transaction.
    Sweep,
}

fn main() {
    if let Err(e) = run(Cli::parse()) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    Err(match cli.cmd {
        Cmd::Derive => RecoveryError::NotImplemented("derive"),
        Cmd::Scan => RecoveryError::NotImplemented("scan"),
        Cmd::Plan => RecoveryError::NotImplemented("plan"),
        Cmd::Sweep => RecoveryError::NotImplemented("sweep"),
    })
}
