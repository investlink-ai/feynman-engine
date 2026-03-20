use anyhow::{bail, Context, Result};
use bus::{MessageBus, RedisBus};
use config::{Config, File};
use serde::Deserialize;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use tonic::transport::Server;
use tonic_health::server::health_reporter;
use tonic_health::ServingStatus;
use tracing::info;

const DEFAULT_CONFIG_PATH: &str = "config/default.toml";

#[derive(Debug, Clone, PartialEq, Eq)]
struct CliArgs {
    config_path: PathBuf,
}

impl CliArgs {
    fn parse_from_env() -> Result<Self> {
        Self::parse(std::env::args().skip(1))
    }

    fn parse<I>(args: I) -> Result<Self>
    where
        I: IntoIterator<Item = String>,
    {
        let mut config_path = None;
        let mut args = args.into_iter();

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--config" => {
                    let value = args
                        .next()
                        .ok_or_else(|| anyhow::anyhow!("missing value for --config"))?;
                    config_path = Some(PathBuf::from(value));
                }
                other => bail!("unknown argument: {other}"),
            }
        }

        Ok(Self {
            config_path: config_path.unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH)),
        })
    }
}

#[derive(Debug, Deserialize)]
struct BootstrapConfig {
    engine: EngineConfig,
    redis: RedisConfig,
}

#[derive(Debug, Deserialize)]
struct EngineConfig {
    grpc_port: u16,
}

#[derive(Debug, Deserialize)]
struct RedisConfig {
    url: String,
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();
}

fn load_config(path: &Path) -> Result<BootstrapConfig> {
    let settings = Config::builder()
        .add_source(File::from(path).required(true))
        .build()
        .with_context(|| format!("failed to load config from {}", path.display()))?;

    settings
        .try_deserialize()
        .with_context(|| format!("failed to parse bootstrap config from {}", path.display()))
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli_args = CliArgs::parse_from_env()?;
    init_tracing();

    info!("Feynman Capital — Execution Engine");
    info!("Version: {}", env!("CARGO_PKG_VERSION"));
    info!("Rust version: {}", env!("CARGO_PKG_RUST_VERSION"));
    info!("Loading config from {}", cli_args.config_path.display());

    let bootstrap_config = load_config(&cli_args.config_path)?;

    let redis_bus = RedisBus::connect(&bootstrap_config.redis.url)
        .await
        .with_context(|| {
            format!(
                "failed to connect to Redis at {}",
                bootstrap_config.redis.url
            )
        })?;
    redis_bus.health_check().await.with_context(|| {
        format!(
            "Redis health check failed for {}",
            bootstrap_config.redis.url
        )
    })?;

    let bind_addr = SocketAddr::from(([0, 0, 0, 0], bootstrap_config.engine.grpc_port));
    let (mut health_reporter, health_service) = health_reporter();
    health_reporter
        .set_service_status("", ServingStatus::Serving)
        .await;

    info!("Redis reachable at {}", bootstrap_config.redis.url);
    info!(
        "gRPC health server listening on :{}",
        bootstrap_config.engine.grpc_port
    );

    let _redis_bus = redis_bus;

    Server::builder()
        .add_service(health_service)
        .serve(bind_addr)
        .await
        .context("gRPC server exited unexpectedly")
}

#[cfg(test)]
mod tests {
    use super::CliArgs;
    use std::path::PathBuf;

    #[test]
    fn parse_uses_default_config_path_when_flag_is_absent() {
        let cli_args = CliArgs::parse(Vec::<String>::new()).expect("default config path");
        assert_eq!(cli_args.config_path, PathBuf::from("config/default.toml"));
    }

    #[test]
    fn parse_accepts_explicit_config_path() {
        let cli_args = CliArgs::parse(vec!["--config".to_owned(), "/tmp/custom.toml".to_owned()])
            .expect("explicit config path");
        assert_eq!(cli_args.config_path, PathBuf::from("/tmp/custom.toml"));
    }

    #[test]
    fn parse_rejects_unknown_arguments() {
        let error =
            CliArgs::parse(vec!["--verbose".to_owned()]).expect_err("unknown arguments fail");
        assert!(error.to_string().contains("unknown argument"));
    }
}
