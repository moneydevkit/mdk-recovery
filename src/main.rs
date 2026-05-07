use clap::{Args, Parser, Subcommand};
use esplora_client::Builder;
use mdk_recovery::cli::{OutputFormat, endpoint_for, read_mnemonic};
use mdk_recovery::scan;
use mdk_recovery::{RecoveryError, Result};

/// Common options for every subcommand that needs a mnemonic and a
/// network: identical clap surface, no copy-paste in each variant.
#[derive(Args)]
struct CommonArgs {
    /// Path to a file containing the BIP-39 mnemonic. Use `-` to
    /// read from stdin. The mnemonic is never accepted on argv.
    #[arg(long)]
    mnemonic_file: String,

    /// Bitcoin network to derive on and (where applicable) scan.
    #[arg(long, value_parser = parse_network)]
    network: bitcoin::Network,
}

#[derive(Args)]
struct ScanArgs {
    #[command(flatten)]
    common: CommonArgs,

    /// Maximum number of esplora requests in flight at once.
    #[arg(long, default_value_t = 20)]
    max_concurrency: usize,

    /// Emit the report as pretty-printed JSON instead of the
    /// human-readable summary.
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct DeriveArgs {
    #[command(flatten)]
    common: CommonArgs,

    /// Emit the report as pretty-printed JSON instead of the
    /// human-readable summary.
    #[arg(long)]
    json: bool,
}

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
    Derive(DeriveArgs),
    /// Pure scan via the per-network esplora endpoint. Read-only.
    Scan(ScanArgs),
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
    match cli.cmd {
        Cmd::Derive(args) => run_derive(args),
        Cmd::Scan(args) => tokio_runtime()?.block_on(run_scan(args)),
        Cmd::Plan => Err(RecoveryError::NotImplemented("plan")),
        Cmd::Sweep => Err(RecoveryError::NotImplemented("sweep")),
    }
}

fn run_derive(args: DeriveArgs) -> Result<()> {
    let mnemonic = read_mnemonic(&args.common.mnemonic_file)?;
    let report = scan::derive_all(&mnemonic, args.common.network);
    render(&report, OutputFormat::from_json_flag(args.json))
}

async fn run_scan(args: ScanArgs) -> Result<()> {
    let mnemonic = read_mnemonic(&args.common.mnemonic_file)?;
    let url = endpoint_for(args.common.network)?;
    let client = Builder::new(&url)
        .build_async()
        .map_err(|e| RecoveryError::Esplora(e.to_string()))?;
    let report = scan::run(
        &client,
        &mnemonic,
        args.common.network,
        args.max_concurrency,
    )
    .await?;
    render(&report, OutputFormat::from_json_flag(args.json))
}

/// Print `report` in the requested format. JSON goes through serde
/// pretty-print; human goes through `Display`. Both write to stdout
/// so the caller can pipe.
fn render<T>(report: &T, format: OutputFormat) -> Result<()>
where
    T: serde::Serialize + std::fmt::Display,
{
    match format {
        OutputFormat::Human => println!("{report}"),
        OutputFormat::Json => {
            let json = serde_json::to_string_pretty(report)
                .map_err(|e| RecoveryError::Io(std::io::Error::other(e)))?;
            println!("{json}");
        }
    }
    Ok(())
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
