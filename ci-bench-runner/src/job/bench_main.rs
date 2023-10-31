use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use tempfile::TempDir;
use tracing::{debug, error};

use crate::event_queue::JobContext;
use crate::github::PushEvent;
use crate::job::read_results;
use crate::RepoAndSha;

pub async fn bench_main(ctx: JobContext<'_>) {
    match bench_main_inner(ctx).await {
        Ok(r) => r,
        Err(e) => {
            error!(
                cause = e.to_string(),
                "Error running benchmarks for main branch"
            );
        }
    }
}

pub async fn bench_main_inner(ctx: JobContext<'_>) -> anyhow::Result<()> {
    // Ideally, we'd use WebhookEvent::try_from_header_and_body from `octocrab`, but it doesn't have
    // the `repository` field on the payload, which we need.
    let Ok(payload) = serde_json::from_slice::<PushEvent>(ctx.event_payload) else {
        bail!("Invalid JSON payload, ignoring event");
    };

    if payload.deleted {
        debug!("Ignoring push event for deleted ref");
        return Ok(());
    } else if payload.git_ref != "refs/heads/main" {
        debug!("Ignoring push event for ref {}", payload.git_ref);
        return Ok(());
    }

    let job_output_dir = ctx.job_output_dir.clone();
    let bench_runner = ctx.bench_runner.clone();
    let icounts_path = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let base_repo = TempDir::new().context("Unable to create temp dir")?;

        let base_repo_path = base_repo.path().to_owned();
        let mut logs = Vec::new();
        bench_runner.checkout_and_run_benchmarks(
            &RepoAndSha {
                clone_url: payload.repository.clone_url,
                branch_name: "main".to_string(),
                commit_sha: payload.after,
            },
            &base_repo_path,
            &job_output_dir,
            &mut logs,
        ).with_context(|| format!("Unable to run benchmarks for main branch. Check the logs at {} for more details.", job_output_dir.display()))?;

        Ok(icounts_path(&job_output_dir))
    }).await.context("tokio task crashed unexpectedly")??;

    let icounts =
        read_results(&icounts_path).context("failed to read instruction counts from file")?;

    // There is probably an incantation to pass an iterator over `icounts `instead of collecting
    // into a vec, but my last attempt resulted in a flood of impenetrable async errors
    ctx.db
        .store_run_results(icounts.into_iter().collect())
        .await
        .context("failed to store benchmark results")?;

    Ok(())
}

fn icounts_path(base: &Path) -> PathBuf {
    base.join("results/icounts.csv")
}
