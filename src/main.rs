use bitcoin::Address;
use bitcoin::address::NetworkUnchecked;
use clap::{Args, Parser, Subcommand};
use esplora_client::{AsyncClient, Builder};
use mdk_recovery::cli::{OutputFormat, confirm_destination, endpoint_for, read_mnemonic};
use mdk_recovery::error::fmt_error_chain;
use mdk_recovery::plan::DEFAULT_FEERATE_SAT_VB;
use mdk_recovery::scan::esplora::broadcast;
use mdk_recovery::sign::SignedSweep;
use mdk_recovery::{RecoveryError, Result, scan};

/// Maximum number of esplora requests in flight at once during a
/// scan. Sized to sit inside the burst budget of blockstream/esplora's
/// stock nginx config (`burst=10` on `/api/`). The per-second budget
/// is a property of the public endpoint, not a knob the operator
/// usefully tunes, so this stays out of the CLI surface.
const SCAN_CONCURRENCY: usize = 8;

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

    /// Also query the 1000 static_payment P2WSH anchor scripts. Off
    /// by default since MDK does not open anchor channels; turning
    /// this on roughly doubles the script count and scan time. Only
    /// useful if anchor-channel close outputs may have been paid to
    /// this seed by another implementation.
    #[arg(long)]
    scan_anchors: bool,

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

/// Args shared by `plan` and `sweep`: scan parameters plus where to
/// send the swept funds and at what feerate.
#[derive(Args)]
struct SweepCommonArgs {
    #[command(flatten)]
    common: CommonArgs,

    /// Destination address for the sweep. Must validate against
    /// `--network`. Bech32 / P2WPKH / P2WSH / P2SH / P2PKH all OK;
    /// P2TR is supported only for the dust threshold lookup.
    #[arg(long)]
    to: Address<NetworkUnchecked>,

    /// Feerate in sat/vB. Defaults to a moderate value; raise it if
    /// the mempool is congested or the recovery is time-sensitive.
    #[arg(long, default_value_t = DEFAULT_FEERATE_SAT_VB)]
    feerate_sat_vb: u64,

    /// Also query the 1000 static_payment P2WSH anchor scripts. Off
    /// by default since MDK does not open anchor channels; turning
    /// this on roughly doubles the script count and scan time. Only
    /// useful if anchor-channel close outputs may have been paid to
    /// this seed by another implementation.
    #[arg(long)]
    scan_anchors: bool,

    /// Emit the report as pretty-printed JSON instead of the
    /// human-readable summary.
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct PlanArgs {
    #[command(flatten)]
    sweep: SweepCommonArgs,
}

#[derive(Args)]
struct SweepArgs {
    #[command(flatten)]
    sweep: SweepCommonArgs,

    /// After signing, prompt for the destination address again and
    /// broadcast the sweep through the per-network esplora endpoint.
    /// Without this flag the signed transaction is rendered but not
    /// submitted.
    #[arg(long)]
    broadcast: bool,
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
    /// Build a recovery plan and render JSON + human summary. No
    /// signing, no broadcast.
    Plan(PlanArgs),
    /// Build, sign, and (with `--broadcast`) submit the sweep transaction.
    Sweep(SweepArgs),
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
        Cmd::Plan(args) => tokio_runtime()?.block_on(run_plan(args)),
        Cmd::Sweep(args) => tokio_runtime()?.block_on(run_sweep(args)),
    }
}

fn run_derive(args: DeriveArgs) -> Result<()> {
    let mnemonic = read_mnemonic(&args.common.mnemonic_file)?;
    let report = scan::derive_all(&mnemonic, args.common.network);
    render(&report, OutputFormat::from_json_flag(args.json))
}

async fn run_scan(args: ScanArgs) -> Result<()> {
    let mnemonic = read_mnemonic(&args.common.mnemonic_file)?;
    let client = build_client(args.common.network)?;
    let report = scan::run(
        &client,
        &mnemonic,
        args.common.network,
        SCAN_CONCURRENCY,
        args.scan_anchors,
    )
    .await?;
    render(&report, OutputFormat::from_json_flag(args.json))
}

async fn run_plan(args: PlanArgs) -> Result<()> {
    let plan = build_plan(&args.sweep).await?;
    render(&plan, OutputFormat::from_json_flag(args.sweep.json))
}

async fn run_sweep(args: SweepArgs) -> Result<()> {
    let plan = build_plan(&args.sweep).await?;
    let (sweep, tx) = SignedSweep::from_plan(plan);

    if args.broadcast {
        confirm_destination(&sweep.plan.destination)?;
        let client = build_client(args.sweep.common.network)?;
        broadcast(&client, &tx).await?;
    }

    render(&sweep, OutputFormat::from_json_flag(args.sweep.json))
}

/// Common scan + plan-construction path used by `plan` and `sweep`.
async fn build_plan(args: &SweepCommonArgs) -> Result<mdk_recovery::plan::RecoveryPlan> {
    let mnemonic = read_mnemonic(&args.common.mnemonic_file)?;
    let client = build_client(args.common.network)?;
    let report = scan::run(
        &client,
        &mnemonic,
        args.common.network,
        SCAN_CONCURRENCY,
        args.scan_anchors,
    )
    .await?;
    report.into_plan(args.to.clone(), args.feerate_sat_vb)
}

fn build_client(network: bitcoin::Network) -> Result<AsyncClient> {
    let url = endpoint_for(network)?;
    Builder::new(&url)
        .build_async()
        .map_err(|e| RecoveryError::Esplora(fmt_error_chain(&e)))
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
