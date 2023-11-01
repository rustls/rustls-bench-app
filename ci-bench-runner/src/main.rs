use std::str::FromStr;
use std::sync::Arc;
use std::{env, fs};

use anyhow::Context;
use sentry::types::Dsn;
use sqlx::{Connection, SqliteConnection};
use tokio::sync::Mutex;
use tracing::Level;
use tracing_subscriber::filter::Targets;
use tracing_subscriber::prelude::*;

use ci_bench_runner::{server, AppConfig, LocalBenchRunner};

fn main() -> anyhow::Result<()> {
    // Load the application's configuration
    let config_bytes = fs::read("config.json")
        .with_context(|| format!("unable to load config (CWD = {})", cwd()))?;
    let config: AppConfig = serde_json::from_slice(&config_bytes)
        .with_context(|| format!("unable to parse config (CWD = {})", cwd()))?;
    let config = Arc::new(config);

    // Initialize Sentry. Their [docs](https://docs.sentry.io/platforms/rust/) say we should run
    // this before the async runtime is initialized
    let _guard = sentry::init(sentry::ClientOptions {
        dsn: Some(Dsn::from_str(&config.sentry_dsn)?),
        traces_sample_rate: 1.0,
        ..sentry::ClientOptions::default()
    });

    // Initialize tracing, dumping traces to stdout as well as to Sentry
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_filter(filter()))
        .with(sentry_tracing::layer().with_filter(filter()))
        .init();

    // We use a single-threaded runtime, since we expect a low volume of requests and we want the
    // application to run with as low overhead as possible
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    rt.block_on(async {
        let sqlite = SqliteConnection::connect(&format!("sqlite:{}", config.path_to_db)).await?;

        // Initialize the server
        let (server, _) = server(
            config,
            Arc::new(LocalBenchRunner),
            Arc::new(Mutex::new(sqlite)),
        )
        .await
        .context("unable to initialize server")?;

        // Listen
        server.await.context("server crashed")?;

        Ok(())
    })
}

fn filter() -> Targets {
    Targets::default()
        .with_target("ci_bench_runner", Level::TRACE)
        .with_target("sqlx", Level::DEBUG)
        .with_target("octocrab", Level::DEBUG)
        .with_default(Level::INFO)
}

fn cwd() -> String {
    env::current_dir()
        .map(|d| d.display().to_string())
        .unwrap_or_else(|_| "<unknown>".to_string())
}
