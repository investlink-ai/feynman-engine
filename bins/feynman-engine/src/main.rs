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

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
struct EngineConfig {
    mode: String,
    dry_run: bool,
    grpc_port: u16,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
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

    let config = settings
        .try_deserialize()
        .with_context(|| format!("failed to parse bootstrap config from {}", path.display()))?;

    apply_env_overrides(config, |key| std::env::var(key).ok())
}

fn apply_env_overrides<F>(mut config: BootstrapConfig, env: F) -> Result<BootstrapConfig>
where
    F: Fn(&str) -> Option<String>,
{
    match (env("FEYNMAN_MODE"), env("ENGINE_MODE")) {
        (Some(feynman_mode), Some(engine_mode)) if feynman_mode != engine_mode => {
            bail!(
                "conflicting engine mode env vars: FEYNMAN_MODE={}, ENGINE_MODE={}",
                feynman_mode,
                engine_mode
            );
        }
        (Some(feynman_mode), _) => config.engine.mode = feynman_mode,
        (_, Some(engine_mode)) => config.engine.mode = engine_mode,
        (None, None) => {}
    }

    if let Some(dry_run) = env("ENGINE_DRY_RUN") {
        config.engine.dry_run = parse_bool_env("ENGINE_DRY_RUN", &dry_run)?;
    }

    if let Some(grpc_port) = env("ENGINE_GRPC_PORT") {
        config.engine.grpc_port = parse_u16_env("ENGINE_GRPC_PORT", &grpc_port)?;
    }

    if let Some(redis_url) = env("REDIS_URL") {
        if redis_url.trim().is_empty() {
            bail!("REDIS_URL must not be empty");
        }
        config.redis.url = redis_url;
    }

    Ok(config)
}

fn parse_bool_env(key: &str, value: &str) -> Result<bool> {
    value
        .parse()
        .with_context(|| format!("{key} must be a boolean, got {value}"))
}

fn parse_u16_env(key: &str, value: &str) -> Result<u16> {
    value
        .parse()
        .with_context(|| format!("{key} must be a valid u16, got {value}"))
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
    info!("Engine mode: {}", bootstrap_config.engine.mode);
    info!("dry_run: {}", bootstrap_config.engine.dry_run);

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
    use super::{apply_env_overrides, BootstrapConfig, CliArgs, EngineConfig, RedisConfig};
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

    #[test]
    fn env_overrides_apply_runtime_bootstrap_values() {
        let config = apply_env_overrides(sample_config(), |key| match key {
            "FEYNMAN_MODE" => Some("paper".to_owned()),
            "ENGINE_DRY_RUN" => Some("false".to_owned()),
            "ENGINE_GRPC_PORT" => Some("60000".to_owned()),
            "REDIS_URL" => Some("redis://127.0.0.1:6380".to_owned()),
            _ => None,
        })
        .expect("env overrides apply");

        assert_eq!(config.engine.mode, "paper");
        assert!(!config.engine.dry_run);
        assert_eq!(config.engine.grpc_port, 60_000);
        assert_eq!(config.redis.url, "redis://127.0.0.1:6380");
    }

    #[test]
    fn env_overrides_reject_conflicting_mode_env_vars() {
        let error = apply_env_overrides(sample_config(), |key| match key {
            "FEYNMAN_MODE" => Some("paper".to_owned()),
            "ENGINE_MODE" => Some("live".to_owned()),
            _ => None,
        })
        .expect_err("conflicting mode env vars fail");

        assert!(error
            .to_string()
            .contains("conflicting engine mode env vars"));
    }

    #[test]
    fn env_overrides_reject_invalid_booleans() {
        let error = apply_env_overrides(sample_config(), |key| match key {
            "ENGINE_DRY_RUN" => Some("sometimes".to_owned()),
            _ => None,
        })
        .expect_err("invalid dry-run env value fails");

        assert!(error
            .to_string()
            .contains("ENGINE_DRY_RUN must be a boolean"));
    }

    fn sample_config() -> BootstrapConfig {
        BootstrapConfig {
            engine: EngineConfig {
                mode: "live".to_owned(),
                dry_run: true,
                grpc_port: 50_051,
            },
            redis: RedisConfig {
                url: "redis://redis:6379".to_owned(),
            },
        }
    }
}
