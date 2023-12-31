use std::fs;

use anyhow::{bail, Context};
use bencher_client::json::DateTime;
use tempfile::TempDir;
use tracing::{trace, warn};

use super::{icounts_path, read_icount_results, read_walltime_results, walltimes_path};
use crate::db::ScenarioKind;
use crate::event_queue::JobContext;
use crate::github::api::PushEvent;
use crate::runner::write_logs_for_run;
use crate::CommitIdentifier;

pub static MAIN_BRANCH: &str = "main";

/// Handle a push to main
///
/// Runs the benchmarks for the head commit and stores the results in the database so they can be
/// used later (e.g. for deriving the significance threshold)
pub async fn bench_main(ctx: JobContext<'_>) -> anyhow::Result<()> {
    // Ideally, we'd use WebhookEvent::try_from_header_and_body from `octocrab`, but it doesn't have
    // the `repository` field on the payload, which we need.
    let Ok(payload) = serde_json::from_slice::<PushEvent>(ctx.event_payload) else {
        bail!("invalid JSON payload, ignoring event");
    };

    if payload.deleted {
        trace!("ignoring push event for deleted ref");
        return Ok(());
    }

    if payload.git_ref != "refs/heads/main" {
        trace!("ignoring push event for non-main ref: {}", payload.git_ref);
        return Ok(());
    }

    let benchmark_run_start = DateTime::now();

    // Run the benchmarks on the main branch
    let job_output_dir = ctx.job_output_dir.clone();
    let bench_runner = ctx.bench_runner.clone();
    let commit_sha = payload.after.clone();
    tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        fs::create_dir_all(&job_output_dir).context("unable to create job output dir")?;
        let base_repo = TempDir::new().context("unable to create temp dir")?;
        let base_repo_path = base_repo.path().to_owned();
        let mut logs = Vec::new();

        let result = bench_runner.checkout_and_run_benchmarks(
            &CommitIdentifier {
                clone_url: payload.repository.clone_url,
                branch_name: MAIN_BRANCH.to_string(),
                commit_sha,
            },
            &base_repo_path,
            &job_output_dir,
            &mut logs,
        );

        let mut s = String::new();
        write_logs_for_run(&mut s, &logs);
        fs::write(job_output_dir.join("logs.md"), s).context("unable to write job logs")?;

        result.with_context(|| {
            format!(
                "unable to run benchmarks for main branch. Check the logs at {} for more details.",
                job_output_dir.display()
            )
        })
    })
    .await
    .context("tokio task crashed unexpectedly")??;

    let benchmark_run_end = DateTime::now();

    // Get the benchmark results back from the filesystem
    let icounts = read_icount_results(&icounts_path(&ctx.job_output_dir))
        .context("failed to read instruction counts from file")?;
    let walltimes = read_walltime_results(&walltimes_path(&ctx.job_output_dir))
        .context("failed to read walltimes from file")?;

    // Persist results in the DB and in bencher.dev
    let results = icounts
        .iter()
        .map(|(scenario, result)| (scenario.clone(), ScenarioKind::Icount, *result))
        .chain(
            walltimes
                .iter()
                .map(|(scenario, result)| (scenario.clone(), ScenarioKind::Walltime, *result)),
        )
        .collect();
    ctx.db
        .store_run_results(results)
        .await
        .context("failed to store benchmark results")?;

    if let Some(bencher_dev) = ctx.bencher_dev {
        let result = bencher_dev
            .track_results(
                MAIN_BRANCH,
                &payload.after,
                benchmark_run_start,
                benchmark_run_end,
                icounts,
                walltimes,
            )
            .await
            .context("failed to send results to bencher.dev");

        if let Err(e) = result {
            warn!("{e:?}");
        } else {
            trace!("pushed benchmark results to bencher.dev");
        }
    }

    Ok(())
}
