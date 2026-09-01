mod registered_gears;

use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;
use toolkit::bootstrap::{AppConfig, run_server};

/// Standalone server for the users-info example gear
#[derive(Parser)]
#[command(name = "users-info-server")]
#[command(about = "Standalone server for users-info example gear")]
struct Cli {
    /// Path to configuration file
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Log verbosity level (-v debug, -vv trace)
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,

    /// Print effective configuration and exit
    #[arg(long)]
    print_config: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let mut config = AppConfig::load_or_default(cli.config.as_ref())?;
    config.apply_cli_overrides(cli.verbose);

    if cli.print_config {
        tracing::info!("Effective configuration:\n{}", config.to_yaml()?);
        return Ok(());
    }

    run_server(config).await
}
