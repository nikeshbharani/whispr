use anyhow::Result;
use clap::Parser;
use whispr::ui::cli::{self, Cli};

#[tokio::main]
async fn main() -> Result<()> {
    // Logs go to stderr and stay out of the way of the TUI unless RUST_LOG is set.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .try_init();

    let cli = Cli::parse();
    cli::run(cli).await
}
