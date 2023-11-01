#[cfg(test)]
mod test;

mod db;
mod event_queue;
mod github;
mod job;
mod runner;

use anyhow::Context;
use std::future::Future;
use std::net::SocketAddr;
use std::ops::DerefMut;
use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use sqlx::migrate::Migrator;
use sqlx::SqliteConnection;
use tokio::sync::Mutex;
use tower_http::trace::TraceLayer;
use tracing::{error, info, trace};
use uuid::Uuid;

pub use crate::db::Db;
use crate::event_queue::EventQueue;
use crate::github::verify_webhook_signature;
pub use crate::github::CachedOctocrab;
use crate::runner::BenchRunner;
pub use crate::runner::LocalBenchRunner;

/// The application's state, accessible when handling requests
struct AppState {
    config: Arc<AppConfig>,
    event_queue: EventQueue,
    db: Db,
}

/// The application's configuration
#[derive(Deserialize)]
pub struct AppConfig {
    /// Base URL used for the GitHub API (used to mock out the API in tests)
    pub github_api_url_override: Option<String>,
    /// Base URL of the application (used to generate links)
    pub app_base_url: String,
    /// Local directory where job output will be stored
    pub job_output_dir: PathBuf,
    /// Path to the SQLite database used for persistence
    pub path_to_db: String,
    /// Secret used by GitHub to sign webhook payloads
    pub webhook_secret: String,
    /// ID of the GitHub App we are authenticated as
    pub github_app_id: u64,
    /// Private key of the GitHub App we are authenticated as
    pub github_app_key: String,
    /// GitHub repository owner (typically rustls)
    pub github_repo_owner: String,
    /// GitHub repository name (typically rustls)
    pub github_repo_name: String,
    /// Sentry DSN
    pub sentry_dsn: String,
    /// Port where the application should listen (defaults to 0 if unset)
    pub port: Option<u16>,
}

/// Creates a new instance of the HTTP server and returns the address at which it is listening
pub async fn server(
    config: Arc<AppConfig>,
    bench_runner: Arc<dyn BenchRunner>,
    sqlite: Arc<Mutex<SqliteConnection>>,
) -> anyhow::Result<(impl Future<Output = Result<(), hyper::Error>>, SocketAddr)> {
    let port = config.port.unwrap_or(0);

    // Run any pending DB migrations
    let mut sqlite_locked = sqlite.lock().await;
    MIGRATOR
        .run(sqlite_locked.deref_mut())
        .await
        .context("failed to apply DB migration")?;
    drop(sqlite_locked);

    // Set up dependencies
    let octocrab = CachedOctocrab::new(&config).await?;
    let db = Db::with_connection(sqlite);
    let event_queue = EventQueue::new(config.clone(), db.clone(), bench_runner, octocrab);

    // Create the application's state, accessible when handling requests
    let state = Arc::new(AppState {
        config,
        event_queue,
        db,
    });

    // Set up the axum application
    let app = Router::new()
        .route("/webhooks/github", post(handle_github_webhook))
        .route("/jobs/:id", get(get_job_status))
        .route(
            "/comparisons/:commits/cachegrind-diff/:scenario",
            get(get_cachegrind_diff),
        )
        .with_state(state)
        .layer(TraceLayer::new_for_http());

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let server = axum::Server::bind(&addr).serve(app.into_make_service());
    let addr = server.local_addr();

    info!("listening on port {}", addr.port());
    Ok((server, addr))
}

/// Returns status information about the job
async fn get_job_status(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> axum::response::Result<Response> {
    let status = state
        .event_queue
        .job_status(id)
        .await
        .map_err(|_| "Internal server error")?;

    let response = match status {
        None => (StatusCode::NOT_FOUND, "Not found").into_response(),
        Some(status) => Json(status).into_response(),
    };

    Ok(response)
}

/// Returns the cachegrind diff between the specified commits, for the provided scenario
async fn get_cachegrind_diff(
    State(state): State<Arc<AppState>>,
    Path((compared_commits, scenario_name)): Path<(String, String)>,
) -> axum::response::Result<String> {
    // Extract commit hashes from URL
    let mut commit_parts = compared_commits.split(':');
    let baseline_commit = commit_parts.next().ok_or("malformed URL")?;
    let candidate_commit = commit_parts.next().ok_or("malformed URL")?;
    if commit_parts.next().is_some() {
        Err((StatusCode::BAD_REQUEST, "malformed URL"))?;
    }

    let result = state
        .db
        .cachegrind_diff(baseline_commit, candidate_commit, &scenario_name)
        .await
        .map_err(|_| "internal server error")?;

    let result = result.ok_or((
        StatusCode::NOT_FOUND,
        "comparison not found for the provided commit hashes and scenario",
    ))?;

    Ok(result)
}

/// Handles an incoming GitHub webhook
async fn handle_github_webhook(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    trace!("incoming webhook");

    let Some(signature) = headers.get(WEBHOOK_SIGNATURE_HEADER) else {
        trace!("{WEBHOOK_SIGNATURE_HEADER} header is missing, ignoring event");
        return StatusCode::BAD_REQUEST;
    };

    let Ok(signature) = signature.to_str() else {
        trace!("{WEBHOOK_SIGNATURE_HEADER} has an invalid header value, ignoring event");
        return StatusCode::BAD_REQUEST;
    };

    if !verify_webhook_signature(&body, signature, &state.config.webhook_secret) {
        trace!("Invalid signature, ignoring event");
        return StatusCode::BAD_REQUEST;
    }

    let Some(event) = headers.get(WEBHOOK_EVENT_HEADER) else {
        trace!("{WEBHOOK_EVENT_HEADER} header is missing, ignoring event");
        return StatusCode::BAD_REQUEST;
    };

    let Ok(event) = event.to_str() else {
        trace!("{WEBHOOK_EVENT_HEADER} has an invalid header value, ignoring event");
        return StatusCode::BAD_REQUEST;
    };

    // Events are enqueued and processed sequentially in the background
    match state.event_queue.enqueue(event, body).await {
        Ok(Some(event_id)) => {
            trace!("enqueued webhook event `{event}` with id `{event_id}`");
            StatusCode::OK
        }
        Ok(None) => {
            error!("unsupported webhook event: {event}");
            StatusCode::BAD_REQUEST
        }
        Err(e) => {
            error!(
                cause = e.to_string(),
                "unable to enqueue webhook event: {event}"
            );
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

/// The HTTP header containing the SHA256 signature of the GitHub webhook payload
pub static WEBHOOK_SIGNATURE_HEADER: &str = "X-Hub-Signature-256";

/// The HTTP header containing the name of the event that triggered the GitHub webhook
pub static WEBHOOK_EVENT_HEADER: &str = "X-GitHub-Event";

/// Identifies a specific commit in a repository
#[derive(Clone)]
pub struct CommitIdentifier {
    /// The URL at which the repository can be cloned
    pub clone_url: String,
    /// The branch we are interested in (for reporting purposes)
    pub branch_name: String,
    /// The specific commit we are interested in
    pub commit_sha: String,
}

/// Migrator for our SQLite database
pub static MIGRATOR: Migrator = sqlx::migrate!();
