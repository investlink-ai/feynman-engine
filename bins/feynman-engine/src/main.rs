use anyhow::Result;
use std::path::PathBuf;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    info!("Feynman Capital — Execution Engine");
    info!("Version: {}", env!("CARGO_PKG_VERSION"));
    info!("Rust version: {}", env!("CARGO_PKG_RUST_VERSION"));

    // TODO: Parse CLI args
    // TODO: Load config
    // TODO: Initialize engine
    // TODO: Start gRPC server
    // TODO: Start dashboard server
    // TODO: Run event loop

    info!("Engine started (placeholder mode)");

    Ok(())
}
