//! OoP binary for calculator gear.
//!
//! This binary runs the calculator gear as an out-of-process service,
//! registering with the DirectoryService and exposing the gRPC service.
//!
//! Configuration is loaded from:
//! 1. --config CLI argument (passed by master host)
//! 2. MODULE_CONFIG_PATH environment variable (fallback)

mod registered_gears;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    use clap::Parser;
    use toolkit::bootstrap::oop::{OopRunOptions, run_oop_with_options};

    /// OoP calculator gear
    #[derive(Parser)]
    #[command(name = "calculator-oop")]
    struct Cli {
        /// Path to configuration file
        #[arg(short, long)]
        config: Option<std::path::PathBuf>,

        /// Log verbosity level (-v debug, -vv trace)
        #[arg(short, long, action = clap::ArgAction::Count)]
        verbose: u8,
    }

    let cli = Cli::parse();

    // Use CLI config if provided, otherwise fall back to Default (which checks MODULE_CONFIG_PATH env var)
    let opts = OopRunOptions {
        gear_name: "calculator".to_string(),
        verbose: cli.verbose,
        config_path: cli.config,
        ..Default::default()
    };

    run_oop_with_options(opts).await
}
