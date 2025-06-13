//! Main ress executable.

use clap::Parser;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::EnvFilter;
use zk_ress::{cli::ZkRessArgs, launch::NodeLauncher};

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let orig_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        orig_hook(panic_info);
        std::process::exit(1);
    }));

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::builder().with_default_directive(LevelFilter::INFO.into()).from_env_lossy(),
        )
        .init();

    NodeLauncher::new(ZkRessArgs::parse()).launch().await?;
    Ok(())
}
