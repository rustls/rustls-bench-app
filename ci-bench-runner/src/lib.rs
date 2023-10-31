#[cfg(test)]
mod test;

mod db;
mod event_queue;
mod github;
mod job;
mod runner;

use std::future::Future;
use std::net::SocketAddr;
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
use tracing::{debug, error, info};
use uuid::Uuid;

pub use crate::db::Db;
use crate::event_queue::EventQueue;
use crate::github::verify_webhook_signature;
pub use crate::github::CachedOctocrab;
use crate::runner::BenchRunner;
pub use crate::runner::LocalBenchRunner;

struct AppState {
    config: Arc<AppConfig>,
    event_queue: EventQueue,
    db: Db,
}

#[derive(Deserialize)]
pub struct AppConfig {
    pub app_base_url: String,
    pub job_output_dir: PathBuf,
    pub path_to_db: String,
    pub webhook_secret: String,
    pub github_app_id: u64,
    pub github_app_key: String,
    pub github_repo_owner: String,
    pub github_repo_name: String,
    pub sentry_dsn: String,
    pub port: Option<u16>,
}

fn app_state(
    config: Arc<AppConfig>,
    octocrab: CachedOctocrab,
    bench_runner: Arc<dyn BenchRunner>,
    sqlite: Arc<Mutex<SqliteConnection>>,
) -> Arc<AppState> {
    let db = Db::with_connection(sqlite);
    let event_queue = EventQueue::new(config.clone(), db.clone(), bench_runner, octocrab);
    let state = AppState {
        config,
        event_queue,
        db,
    };
    Arc::new(state)
}

pub fn server(
    config: Arc<AppConfig>,
    octocrab: CachedOctocrab,
    bench_runner: Arc<dyn BenchRunner>,
    sqlite: Arc<Mutex<SqliteConnection>>,
) -> (impl Future<Output = ()>, SocketAddr) {
    let port = config.port.unwrap_or(0);
    let state = app_state(config, octocrab, bench_runner, sqlite);
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
    debug!("listening on port {}", addr.port());
    let future = async move {
        server.await.unwrap();
    };
    (future, addr)
}

/// Returns the status of the job
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

/// Returns the cachegrind diff of the job
async fn get_cachegrind_diff(
    State(state): State<Arc<AppState>>,
    Path((compared_commits, scenario_name)): Path<(String, String)>,
) -> axum::response::Result<String> {
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
    debug!("incoming webhook");

    let Some(signature) = headers.get(WEBHOOK_SIGNATURE_HEADER) else {
        debug!("{WEBHOOK_SIGNATURE_HEADER} is missing, ignoring event");
        return StatusCode::BAD_REQUEST;
    };

    let Ok(signature) = signature.to_str() else {
        debug!("{WEBHOOK_SIGNATURE_HEADER} has an invalid header value, ignoring event");
        return StatusCode::BAD_REQUEST;
    };

    if !verify_webhook_signature(&body, signature, &state.config.webhook_secret) {
        debug!("Invalid signature, ignoring event");
        return StatusCode::BAD_REQUEST;
    }

    let Some(event) = headers.get(WEBHOOK_EVENT_HEADER) else {
        return StatusCode::BAD_REQUEST;
    };

    let Ok(event) = event.to_str() else {
        debug!("{WEBHOOK_EVENT_HEADER} has an invalid header value, ignoring event");
        return StatusCode::BAD_REQUEST;
    };

    match state.event_queue.enqueue(event, body).await {
        Ok(Some(event_id)) => {
            info!("enqueued webhook event `{event}` with id `{event_id}`");
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

#[derive(Clone)]
pub struct RepoAndSha {
    pub clone_url: String,
    pub branch_name: String,
    pub commit_sha: String,
}

pub static MIGRATOR: Migrator = sqlx::migrate!();
