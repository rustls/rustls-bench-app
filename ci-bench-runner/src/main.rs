use std::str::FromStr;
use std::sync::Arc;
use std::{env, fs};

use anyhow::Context;
use sentry::types::Dsn;
use sqlx::{Connection, SqliteConnection};
use tokio::sync::Mutex;
use tracing_subscriber::prelude::*;

use ci_bench_runner::{server, AppConfig, CachedOctocrab, LocalBenchRunner, MIGRATOR};

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
        .with(tracing_subscriber::fmt::layer())
        .with(sentry_tracing::layer())
        .init();

    // We use a single-threaded runtime, since we expect a low volume of requests and we want the
    // application to run with as low overhead as possible
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    rt.block_on(async {
        // Initialize our GitHub client, which includes a background task to refresh authentication
        // tokens
        let octocrab = CachedOctocrab::new(None, &config).await?;

        // Initialize SQLite and run any pending DB migrations
        let mut sqlite =
            SqliteConnection::connect(&format!("sqlite:{}", config.path_to_db)).await?;
        MIGRATOR
            .run(&mut sqlite)
            .await
            .context("failed to apply DB migration")?;

        let (server, _) = server(
            config,
            octocrab,
            Arc::new(LocalBenchRunner),
            Arc::new(Mutex::new(sqlite)),
        );

        server.await.context("server crashed")?;

        Ok(())
    })
}

fn cwd() -> String {
    env::current_dir()
        .map(|d| d.display().to_string())
        .unwrap_or_else(|_| "<unknown>".to_string())
}
