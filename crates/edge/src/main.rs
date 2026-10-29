//! Entry point for the standalone `workforce-edge` service.

use std::path::PathBuf;
use std::sync::Arc;

use eyre::{Result, WrapErr};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use workforce_edge::config::EdgeConfig;

#[tokio::main]
async fn main() -> Result<()> {
    // rustls is built with both ring and aws-lc-rs here, so it cannot pick a
    // default provider itself; libraries that call plain
    // `ClientConfig::builder()` would panic on their first TLS connection.
    workforce_common::tls::install_default_crypto_provider();
    color_eyre::install()?;
    // Operator helper: print an argon2 hash for [admin] password_hash and exit.
    if std::env::args().any(|a| a == "--hash-password") {
        return workforce_edge::admin::hash_password_cli();
    }
    init_tracing();

    // `workforce-edge curate …` — the training-data curator (the GitHub Action
    // runs this in the edge image). Reuses this install's `[llm]` provider.
    let argv: Vec<String> = std::env::args().collect();
    if argv.get(1).map(String::as_str) == Some("curate") {
        // Self-contained: provider + model come from `--provider`/`--model`, creds
        // from the env — no baked config needed, so the base edge image runs it.
        let args = workforce_edge::curator::CurateArgs::from_args(&argv[2..])
            .wrap_err("parsing curate arguments")?;
        return workforce_edge::curator::run(args).await;
    }

    // `workforce-edge import-database …` — a self-hosted edge's database,
    // loaded into this deployment as one tenant (the cutover's step 4).
    if argv.get(1).map(String::as_str) == Some(workforce_edge::migrate::COMMAND) {
        return workforce_edge::migrate::run_cli(&argv[2..]).await;
    }

    // `workforce-edge memory …` — the class the edge kept its memories in
    // before it named its own ([weaviate] previous_collection).
    if argv.get(1).map(String::as_str) == Some(workforce_edge::previous::COMMAND) {
        return workforce_edge::previous::run_cli(&argv[2..]).await;
    }

    let config_path = config_path();
    let config = EdgeConfig::load(&config_path)
        .wrap_err_with(|| format!("loading edge config from {}", config_path.display()))?;

    if config.metrics.as_ref().is_some_and(|m| m.enabled) {
        workforce_edge::metrics::install_recorder();
    }

    tracing::info!(
        bind = %config.server.bind,
        "workforce-edge configured; connecting to its own data plane"
    );

    let infra = Arc::new(
        workforce_edge::boot::connect(&config)
            .await
            .wrap_err("connecting the edge data plane")?,
    );

    let state = workforce_edge::server::EdgeState::build(&config, infra)
        .await
        .wrap_err("building the edge tenants")?;

    workforce_edge::server::serve(&config, state).await
}

fn init_tracing() {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer())
        .init();
}

/// Resolve the config path from the first CLI argument, falling back to the
/// `WORKFORCE_EDGE_CONFIG` env var and then `edge.toml`.
fn config_path() -> PathBuf {
    std::env::args()
        .nth(1)
        .or_else(|| std::env::var("WORKFORCE_EDGE_CONFIG").ok())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("edge.toml"))
}
