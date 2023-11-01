use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use tempfile::TempDir;
use tracing::trace;

use crate::event_queue::JobContext;
use crate::github::api::PushEvent;
use crate::job::read_results;
use crate::runner::write_logs_for_run;
use crate::CommitIdentifier;

/// Handle a push to main
///
/// Runs the benchmarks for the head commit and stores the results in the database so they can be
/// used later (e.g. for deriving the significance threshold)
pub async fn bench_main(ctx: JobContext<'_>) -> anyhow::Result<()> {
    // Ideally, we'd use WebhookEvent::try_from_header_and_body from `octocrab`, but it doesn't have
    // the `repository` field on the payload, which we need.
    let Ok(payload) = serde_json::from_slice::<PushEvent>(ctx.event_payload) else {
        bail!("Invalid JSON payload, ignoring event");
    };

    if payload.deleted {
        trace!("Ignoring push event for deleted ref");
        return Ok(());
    }

    if payload.git_ref != "refs/heads/main" {
        trace!("Ignoring push event for non-main ref: {}", payload.git_ref);
        return Ok(());
    }

    // Run the benchmarks on the main branch
    let job_output_dir = ctx.job_output_dir.clone();
    let bench_runner = ctx.bench_runner.clone();
    let icounts_path = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let base_repo = TempDir::new().context("Unable to create temp dir")?;
        let base_repo_path = base_repo.path().to_owned();
        let mut logs = Vec::new();

        let result = bench_runner.checkout_and_run_benchmarks(
            &CommitIdentifier {
                clone_url: payload.repository.clone_url,
                branch_name: "main".to_string(),
                commit_sha: payload.after,
            },
            &base_repo_path,
            &job_output_dir,
            &mut logs,
        );

        let mut s = String::new();
        write_logs_for_run(&mut s, &logs);
        fs::write(ctx.job_output_dir.join("logs.md"), s).context("unable to write job logs")?;

        result.with_context(|| {
            format!(
                "Unable to run benchmarks for main branch. Check the logs at {} for more details.",
                job_output_dir.display()
            )
        })?;

        Ok(icounts_path(&job_output_dir))
    })
    .await
    .context("tokio task crashed unexpectedly")??;

    // Store the benchmark results in the database
    let icounts =
        read_results(&icounts_path).context("failed to read instruction counts from file")?;
    ctx.db
        .store_run_results(icounts.into_iter().collect())
        .await
        .context("failed to store benchmark results")?;

    Ok(())
}

fn icounts_path(base: &Path) -> PathBuf {
    base.join("results/icounts.csv")
}
