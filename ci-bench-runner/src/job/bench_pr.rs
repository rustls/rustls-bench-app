use std::cmp::Ordering;
use std::collections::HashMap;
use std::fmt::Write;
use std::fs;
use std::ops::Deref;
use std::path::Path;
use std::process::Command;

use anyhow::{anyhow, bail, Context};
use askama::Template;
use octocrab::models::pulls::PullRequest;
use octocrab::models::webhook_events::payload::PullRequestWebhookEventAction;
use octocrab::models::webhook_events::{WebhookEvent, WebhookEventPayload};
use octocrab::models::StatusState;
use octocrab::Octocrab;
use tempfile::TempDir;
use time::{Duration, OffsetDateTime};
use tracing::{error, trace};

use super::{icounts_path, read_icount_results, read_walltime_results, walltimes_path};
use crate::db::{BenchResult, ComparisonResult, ComparisonSubResult, ScenarioDiff, ScenarioKind};
use crate::event_queue::JobContext;
use crate::github::api::{CommentEvent, PullRequestReviewEvent};
use crate::github::{self, update_commit_status};
use crate::runner::{write_logs_for_run, BenchRunner, Log};
use crate::CommitIdentifier;

static ALLOWED_AUTHOR_ASSOCIATIONS: &[&str] = &[
    // The owner of the repository
    "OWNER",
    // A member of the organization that owns the repository
    "MEMBER",
    // Someone invited to collaborate on the repository
    "COLLABORATOR",
];

/// Handle an "issue comment"
///
/// Runs the PR benchmarks if the comment:
/// - Has just been created (edits are ignored);
/// - Has been posted to a PR (not to an issue);
/// - Has been posted by an authorized user; and
/// - Addresses the bot with the right command (`@APP_NAME bench`).
pub async fn handle_issue_comment(ctx: JobContext<'_>) -> anyhow::Result<()> {
    // Ideally, we'd use WebhookEvent::try_from_header_and_body from `octocrab`, but it doesn't have
    // the `author_association` field on the comment, which we need.
    let Ok(payload) = serde_json::from_slice::<CommentEvent>(ctx.event_payload) else {
        error!(
            event = ctx.event,
            body = String::from_utf8_lossy(ctx.event_payload).to_string(),
            "invalid JSON payload, ignoring event"
        );
        return Ok(());
    };

    if payload.issue.pull_request.is_none() {
        trace!("the comment was to a plain issue (not to a PR), ignoring event");
        return Ok(());
    };

    let body = &payload.comment.body;
    if payload.action != "created" {
        trace!("ignoring event for `{}` action", payload.action);
        return Ok(());
    }
    if payload.comment.user.id == ctx.config.github_app_id {
        trace!("ignoring comment from ourselves");
        return Ok(());
    }
    if !ALLOWED_AUTHOR_ASSOCIATIONS.contains(&payload.comment.author_association.as_str()) {
        trace!(
            "ignoring comment from unauthorized user (author association = {})",
            payload.comment.author_association
        );
        return Ok(());
    }

    let octocrab = ctx.octocrab.cached();
    if body.contains(&format!("@{APP_NAME} bench")) {
        let pr = octocrab
            .pulls(&ctx.config.github_repo_owner, &ctx.config.github_repo_name)
            .get(payload.issue.number)
            .await
            .context("unable to get PR details")?;

        let branches = pr_branches(&pr).ok_or(anyhow!("unable to get PR branch details"))?;
        bench_pr(ctx, pr.number, branches).await
    } else if body.contains(&format!("@{APP_NAME}")) {
        trace!("the comment was addressed at the application, but it is an unknown command!");
        let comment = format!(
            "Unrecognized command. Available commands are:\n\
        * `@{APP_NAME} bench`: runs the instruction count benchmarks and reports the results"
        );
        octocrab
            .issues(&ctx.config.github_repo_owner, &ctx.config.github_repo_name)
            .create_comment(payload.issue.number, comment)
            .await?;
        Ok(())
    } else {
        trace!("the comment was not addressed at the application");
        Ok(())
    }
}

/// Handle a "PR review"
///
/// Runs the PR benchmarks if the review:
/// - Is an approval; and
/// - Has just been submitted by an authorized user.
pub async fn handle_pr_review(ctx: JobContext<'_>) -> anyhow::Result<()> {
    // Ideally, we'd use WebhookEvent::try_from_header_and_body from `octocrab`, but it doesn't have
    // the `author_association` field on the review (which we need) and it requires the `head` field
    // on the PR (which is not provided).
    let Ok(payload) = serde_json::from_slice::<PullRequestReviewEvent>(ctx.event_payload) else {
        error!(
            event = ctx.event,
            body = String::from_utf8_lossy(ctx.event_payload).to_string(),
            "invalid JSON payload, ignoring event"
        );
        return Ok(());
    };

    if payload.action != "submitted" {
        trace!("ignoring pull request event with action {}", payload.action);
        return Ok(());
    }

    if !ALLOWED_AUTHOR_ASSOCIATIONS.contains(&payload.review.author_association.as_str()) {
        trace!("ignoring review from untrusted author");
        return Ok(());
    }

    if payload.review.state != "approved" {
        trace!("ignoring review with non-approved status");
        return Ok(());
    }

    let pr = payload.pull_request;
    let mut branches = pr_branches(&pr).ok_or(anyhow!("unable to get PR branch details"))?;

    // Ensure we bench the commit that was reviewed, and not something else
    branches.candidate.commit_sha = payload.review.commit_id;

    bench_pr(ctx, pr.number, branches).await
}

/// Handle a "PR update"
///
/// Runs the PR benchmarks if:
/// - The PR originates from a trusted branch (i.e. branches from the repository, not from forks); and
/// - The PR was just created (action is `opened`), or its branches were updated (action is `synchronize`).
pub async fn handle_pr_update(ctx: JobContext<'_>) -> anyhow::Result<()> {
    let Ok(event) = WebhookEvent::try_from_header_and_body(ctx.event, ctx.event_payload) else {
        error!(
            event = ctx.event,
            body = String::from_utf8_lossy(ctx.event_payload).to_string(),
            "invalid JSON payload, ignoring event"
        );
        return Ok(());
    };

    let WebhookEventPayload::PullRequest(payload) = event.specific else {
        error!("invalid JSON payload, ignoring event");
        return Ok(());
    };

    let allowed_actions = [
        PullRequestWebhookEventAction::Opened,
        PullRequestWebhookEventAction::Synchronize,
        PullRequestWebhookEventAction::Reopened,
    ];
    if !allowed_actions.contains(&payload.action) {
        trace!(
            "ignoring pull request event with action {:?}",
            payload.action
        );
        return Ok(());
    }

    let branches =
        pr_branches(&payload.pull_request).ok_or(anyhow!("unable to get PR branch details"))?;
    if branches.baseline.clone_url != branches.candidate.clone_url {
        trace!(
            "ignoring pull request update for forked repo (base repo = {}, head repo = {})",
            branches.baseline.clone_url,
            branches.candidate.clone_url
        );
        return Ok(());
    }

    bench_pr(ctx, payload.pull_request.number, branches).await
}

pub async fn bench_pr(
    ctx: JobContext<'_>,
    pr_number: u64,
    branches: PrBranches,
) -> anyhow::Result<()> {
    let job_url = format!("{}/jobs/{}", ctx.config.app_base_url, ctx.job_id);
    let octocrab = ctx.octocrab.cached();
    update_commit_status(
        branches.candidate.commit_sha.clone(),
        StatusState::Pending,
        job_url.clone(),
        ctx.config,
        &octocrab,
    )
    .await;

    let cached_result = ctx
        .db
        .comparison_result(
            &branches.baseline.commit_sha,
            &branches.candidate.commit_sha,
        )
        .await?;
    let result = match cached_result {
        Some(result) => Ok(result),
        None => {
            let mut logs = BenchPrLogs::default();
            bench_pr_and_cache_results(&ctx, branches.clone(), &mut logs)
                .await
                .map_err(|error| BenchPrError { error, logs })
        }
    };

    let cachegrind_diff_url = format!(
        "{}/comparisons/{}:{}/cachegrind-diff",
        ctx.config.app_base_url, branches.baseline.commit_sha, branches.candidate.commit_sha
    );
    let mut comment = markdown_comment(
        &branches,
        result,
        &cachegrind_diff_url,
        ctx.bencher_dev.map(|b| b.config.project_id.as_str()),
    );
    github::maybe_truncate_comment(&mut comment);

    let update_result = try_update_comment(pr_number, &comment, &octocrab, &ctx).await;
    if update_result.is_err() {
        // Fall back to creating a comment if updating fails
        let comment = octocrab
            .issues(&ctx.config.github_repo_owner, &ctx.config.github_repo_name)
            .create_comment(pr_number, comment)
            .await?;
        ctx.db
            .store_result_comment_id(pr_number, comment.id)
            .await?;
    }

    update_commit_status(
        branches.candidate.commit_sha.clone(),
        StatusState::Success,
        job_url,
        ctx.config,
        &octocrab,
    )
    .await;

    Ok(())
}

async fn try_update_comment(
    pr_number: u64,
    comment: &str,
    octocrab: &Octocrab,
    ctx: &JobContext<'_>,
) -> anyhow::Result<()> {
    if let Some(comment_id) = ctx.db.result_comment_id(pr_number).await? {
        octocrab
            .issues(&ctx.config.github_repo_owner, &ctx.config.github_repo_name)
            .update_comment(comment_id, comment)
            .await?;

        Ok(())
    } else {
        bail!("no comment registered for PR")
    }
}

async fn bench_pr_and_cache_results(
    ctx: &JobContext<'_>,
    branches: PrBranches,
    logs: &mut BenchPrLogs,
) -> anyhow::Result<ComparisonResult> {
    let cutoff_date = OffsetDateTime::now_utc() - Duration::days(30);
    let historical_results = ctx
        .db
        .result_history(cutoff_date)
        .await
        .context("could not obtain result history")?;

    let icount_results = historical_results
        .iter()
        .filter(|r| r.scenario_kind == ScenarioKind::Icount)
        .cloned();
    let icount_significance_thresholds = calculate_significance_thresholds(icount_results);

    let walltime_results = historical_results
        .into_iter()
        .filter(|r| r.scenario_kind == ScenarioKind::Walltime);
    let walltime_significance_thresholds = calculate_significance_thresholds(walltime_results);

    let significance_thresholds = SignificanceThresholds {
        icount: icount_significance_thresholds,
        walltime: walltime_significance_thresholds,
    };

    let job_output_dir = ctx.job_output_dir.clone();
    let runner = ctx.bench_runner.clone();
    let branches_cloned = branches.clone();
    let (result, task_logs) = tokio::task::spawn_blocking(move || {
        let mut logs = BenchPrLogs::default();

        let result = compare_refs(
            &branches_cloned,
            &job_output_dir,
            &mut logs,
            runner.deref(),
            &significance_thresholds,
        );

        if let Err(e) = &result {
            error!(cause = e.to_string(), "unable to compare refs");
        }

        (result, logs)
    })
    .await
    .context("benchmarking task crashed")?;

    *logs = task_logs;

    // Write the task logs so they are available even if commenting to GitHub fails
    let mut s = String::new();
    writeln!(s, "### Candidate").ok();
    write_logs_for_run(&mut s, &logs.candidate);
    writeln!(s, "### Base").ok();
    write_logs_for_run(&mut s, &logs.base);
    fs::write(ctx.job_output_dir.join("logs.md"), s).context("unable to write job logs")?;

    if let Ok(result) = &result {
        ctx.db
            .store_comparison_result(
                branches.baseline.commit_sha,
                branches.candidate.commit_sha,
                result.clone(),
            )
            .await
            .context("could not store comparison results")?;
    }

    result
}

fn pr_branches(pr: &PullRequest) -> Option<PrBranches> {
    Some(PrBranches {
        candidate: CommitIdentifier {
            branch_name: pr.head.ref_field.clone(),
            commit_sha: pr.head.sha.clone(),
            clone_url: pr.head.repo.as_ref()?.clone_url.as_ref()?.to_string(),
        },
        baseline: CommitIdentifier {
            branch_name: pr.base.ref_field.clone(),
            commit_sha: pr.base.sha.clone(),
            clone_url: pr.base.repo.as_ref()?.clone_url.as_ref()?.to_string(),
        },
    })
}

fn compare_refs(
    pr_branches: &PrBranches,
    job_output_path: &Path,
    logs: &mut BenchPrLogs,
    runner: &dyn BenchRunner,
    significance_thresholds: &SignificanceThresholds,
) -> anyhow::Result<ComparisonResult> {
    let candidate_repo = TempDir::new().context("Unable to create temp dir")?;
    let candidate_repo_path = candidate_repo.path().to_owned();

    let base_repo = TempDir::new().context("Unable to create temp dir")?;
    let base_repo_path = base_repo.path().to_owned();

    runner.checkout_and_run_benchmarks(
        &pr_branches.candidate,
        &candidate_repo_path,
        &job_output_path.join("candidate"),
        &mut logs.candidate,
    )?;

    runner.checkout_and_run_benchmarks(
        &pr_branches.baseline,
        &base_repo_path,
        &job_output_path.join("base"),
        &mut logs.base,
    )?;

    let icount_baseline = read_icount_results(&icounts_path(&job_output_path.join("base")))?;
    let icount_candidate = read_icount_results(&icounts_path(&job_output_path.join("candidate")))?;
    let (icount_diffs, icount_missing) = compare_results(
        job_output_path,
        &icount_baseline,
        &icount_candidate,
        &significance_thresholds.icount,
        ScenarioKind::Icount,
        DEFAULT_ICOUNT_NOISE_THRESHOLD,
        MINIMUM_ICOUNT_NOISE_THRESHOLD,
    )?;

    let walltime_baseline = read_walltime_results(&walltimes_path(&job_output_path.join("base")))?;
    let walltime_candidate =
        read_walltime_results(&walltimes_path(&job_output_path.join("candidate")))?;
    let (walltime_diffs, walltime_missing) = compare_results(
        job_output_path,
        &walltime_baseline,
        &walltime_candidate,
        &significance_thresholds.walltime,
        ScenarioKind::Walltime,
        DEFAULT_WALLTIME_NOISE_THRESHOLD,
        MINIMUM_WALLTIME_NOISE_THRESHOLD,
    )?;

    Ok(ComparisonResult {
        icount: ComparisonSubResult {
            diffs: icount_diffs,
            scenarios_missing_in_baseline: icount_missing,
        },
        walltime: ComparisonSubResult {
            diffs: walltime_diffs,
            scenarios_missing_in_baseline: walltime_missing,
        },
    })
}

/// Returns the calculated significance threshold for each scenario
///
/// Scenarios with less than 10 results will be skipped. It is the responsibility of the caller to
/// handle missing significance thresholds, and to clamp them to a minimum value.
pub fn calculate_significance_thresholds(
    historical_results: impl Iterator<Item = BenchResult>,
) -> HashMap<String, f64> {
    let mut results_by_name = HashMap::new();
    for result in historical_results {
        results_by_name
            .entry(result.scenario_name)
            .or_insert(Vec::new())
            .push(result.result);
    }

    let mut significance_thresholds = HashMap::with_capacity(results_by_name.len());
    for (name, results) in results_by_name {
        // Ensure we have at least 10 results available
        if results.len() < 10 {
            continue;
        }

        // A bench result is significant if the change percentage exceeds a threshold derived
        // from historic change percentages. We use inter-quartile range fencing by a factor of 3.0,
        // similar to the Rust compiler's benchmarks.
        // (see https://github.com/rust-lang/rustc-perf/blob/4f313add609f43e928e98132358e8426ed3969ae/site/src/comparison.rs#L1219)
        let mut historic_changes = results
            .windows(2)
            .map(|window| (window[0] - window[1]).abs() / window[0])
            .collect::<Vec<_>>();
        historic_changes.sort_unstable_by(|x, y| x.partial_cmp(y).unwrap_or(Ordering::Equal));

        let q1 = historic_changes[historic_changes.len() / 4];
        let q3 = historic_changes[(historic_changes.len() * 3) / 4];
        let iqr = q3 - q1;
        let iqr_multiplier = 3.0;
        let significance_threshold = q3 + iqr * iqr_multiplier;
        significance_thresholds.insert(name, significance_threshold);
    }

    significance_thresholds
}

struct SignificanceThresholds {
    icount: HashMap<String, f64>,
    walltime: HashMap<String, f64>,
}

#[derive(Debug, Clone)]
pub struct PrBranches {
    pub baseline: CommitIdentifier,
    pub candidate: CommitIdentifier,
}

#[derive(Debug)]
struct BenchPrError {
    error: anyhow::Error,
    logs: BenchPrLogs,
}

#[derive(Debug, Default)]
struct BenchPrLogs {
    base: Vec<Log>,
    candidate: Vec<Log>,
}

/// Creates a markdown version of the results for posting to GitHub as a comment
fn markdown_comment(
    branches: &PrBranches,
    result: Result<ComparisonResult, BenchPrError>,
    diff_url: &str,
    bencher_project_id: Option<&str>,
) -> String {
    match result {
        Ok(bench_results) => ComparisonSuccessComment {
            cachegrind_diff_url: diff_url,
            icount: Diffs::from_sub_result(bench_results.icount),
            walltime: Diffs::from_sub_result(bench_results.walltime),
            branches,
            bencher_project_id,
            common_time_unit: |x, y| common_time_unit(*x, *y),
        }
        .render()
        .expect("failed to render askama template"),
        Err(error) => {
            let mut baseline_logs = String::new();
            write_logs_for_run(&mut baseline_logs, &error.logs.base);
            let mut candidate_logs = String::new();
            write_logs_for_run(&mut candidate_logs, &error.logs.candidate);
            ComparisonErrorComment {
                error: format!("{:?}", error.error),
                baseline_logs,
                candidate_logs,
                branches,
            }
            .render()
            .expect("failed to render askama template")
        }
    }
}

/// Returns an internal representation of the comparison between the baseline and the candidate
/// measurements
fn compare_results(
    job_output_path: &Path,
    baseline: &HashMap<String, f64>,
    candidate: &HashMap<String, f64>,
    significance_thresholds: &HashMap<String, f64>,
    scenario_kind: ScenarioKind,
    default_noise_threshold: f64,
    minimum_noise_threshold: f64,
) -> anyhow::Result<(Vec<ScenarioDiff>, Vec<String>)> {
    let mut diffs = Vec::new();
    let mut missing = Vec::new();
    for (scenario, &instr_count) in candidate {
        let Some(&baseline_instr_count) = baseline.get(scenario) else {
            missing.push(scenario.clone());
            continue;
        };

        let cachegrind_diff = if scenario_kind == ScenarioKind::Icount {
            Some(callgrind_diff(job_output_path, scenario)?)
        } else {
            None
        };

        diffs.push(ScenarioDiff {
            scenario_name: scenario.clone(),
            scenario_kind,
            baseline_result: baseline_instr_count,
            candidate_result: instr_count,
            significance_threshold: significance_thresholds
                .get(scenario)
                .cloned()
                .unwrap_or(default_noise_threshold)
                .max(minimum_noise_threshold),
            cachegrind_diff,
        });
    }

    Ok((diffs, missing))
}

/// Splits the diffs into two `Vec`s, the first one containing the diffs that exceed the threshold,
/// the second one containing the rest
fn split_on_threshold(diffs: Vec<ScenarioDiff>) -> (Vec<ScenarioDiff>, Vec<ScenarioDiff>) {
    fn sort_by_abs_diff_ratio(diffs: &mut [ScenarioDiff]) {
        diffs.sort_by(|s1, s2| {
            f64::partial_cmp(&s2.diff_ratio().abs(), &s1.diff_ratio().abs())
                .unwrap_or(Ordering::Equal)
        });
    }

    let mut significant = Vec::new();
    let mut negligible = Vec::new();

    for diff in diffs {
        if diff.diff_ratio().abs() < diff.significance_threshold {
            negligible.push(diff);
        } else {
            significant.push(diff);
        }
    }

    sort_by_abs_diff_ratio(&mut significant);
    sort_by_abs_diff_ratio(&mut negligible);

    (significant, negligible)
}

/// Returns the detailed instruction diff between the baseline and the candidate
pub fn callgrind_diff(job_output_path: &Path, scenario: &str) -> anyhow::Result<String> {
    // callgrind_annotate formats the callgrind output file, suitable for comparison with
    // callgrind_differ
    let callgrind_annotate_base = Command::new("callgrind_annotate")
        .arg(
            job_output_path
                .join("base/results/callgrind")
                .join(scenario),
        )
        // do not annotate source, to keep output compact
        .arg("--auto=no")
        .output()
        .context("error waiting for callgrind_annotate to finish")?;

    let callgrind_annotate_candidate = Command::new("callgrind_annotate")
        .arg(
            job_output_path
                .join("candidate/results/callgrind")
                .join(scenario),
        )
        // do not annotate source, to keep output compact
        .arg("--auto=no")
        .output()
        .context("error waiting for callgrind_annotate to finish")?;

    if !callgrind_annotate_base.status.success() {
        anyhow::bail!(
            "callgrind_annotate for base finished with an error (code = {:?})",
            callgrind_annotate_base.status.code()
        )
    }

    if !callgrind_annotate_candidate.status.success() {
        anyhow::bail!(
            "callgrind_annotate for candidate finished with an error (code = {:?})",
            callgrind_annotate_candidate.status.code()
        )
    }

    let string_base = String::from_utf8(callgrind_annotate_base.stdout)
        .context("callgrind_annotate produced invalid UTF8")?;
    let string_candidate = String::from_utf8(callgrind_annotate_candidate.stdout)
        .context("callgrind_annotate produced invalid UTF8")?;

    // TODO: reinstate actual diffing, using `callgrind_differ` crate
    Ok(format!(
        "Base output:\n{string_base}\n\
         =====\n\n\
         Candidate output:\n{string_candidate}\n"
    ))
}

#[derive(Template)]
#[template(path = "comparison_success_comment.md")]
pub struct ComparisonSuccessComment<'a> {
    /// Diffs for the icount benchmarks
    icount: Diffs,
    /// Diffs for the walltime benchmarks
    walltime: Diffs,
    /// The base url to obtain cachegrind diffs
    cachegrind_diff_url: &'a str,
    /// Information about the branches that were compared
    branches: &'a PrBranches,
    /// Bencher's project id, if available
    bencher_project_id: Option<&'a str>,
    /// A function to obtain the time unit used to report the walltimes
    common_time_unit: fn(&f64, &f64) -> TimeUnit,
}

pub struct Diffs {
    /// Significant diffs, per scenario
    significant_diffs: Vec<ScenarioDiff>,
    /// Negligible diffs, per scenario
    negligible_diffs: Vec<ScenarioDiff>,
    /// Benchmark scenarios present in the candidate but missing in the baseline
    scenarios_missing_in_baseline: Vec<String>,
}

impl Diffs {
    fn from_sub_result(sub_result: ComparisonSubResult) -> Self {
        let (significant_diffs, negligible_diffs) = split_on_threshold(sub_result.diffs);
        Diffs {
            significant_diffs,
            negligible_diffs,
            scenarios_missing_in_baseline: sub_result.scenarios_missing_in_baseline,
        }
    }
}

#[derive(Template)]
#[template(path = "comparison_error_comment.md")]
pub struct ComparisonErrorComment<'a> {
    /// The error that caused the comparison to fail
    error: String,
    /// Information about the branches that were compared
    branches: &'a PrBranches,
    /// Logs from trying to benchmark the candidate branch
    candidate_logs: String,
    /// Logs from trying to benchmark the baseline branch
    baseline_logs: String,
}

/// Returns a time unit that has enough resolution to represent both values
fn common_time_unit(x: f64, y: f64) -> TimeUnit {
    let max = x.max(y);
    if max < 1_000.0 {
        TimeUnit::Nanoseconds
    } else if max < 1_000_000.0 {
        TimeUnit::Microseconds
    } else if max < 1_000_000_000.0 {
        TimeUnit::Milliseconds
    } else {
        TimeUnit::Seconds
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
enum TimeUnit {
    Nanoseconds,
    Microseconds,
    Milliseconds,
    Seconds,
}

static DEFAULT_ICOUNT_NOISE_THRESHOLD: f64 = 0.002; // 0.2%
static MINIMUM_ICOUNT_NOISE_THRESHOLD: f64 = 0.002; // 0.2%
static DEFAULT_WALLTIME_NOISE_THRESHOLD: f64 = 0.05; // 5%
static MINIMUM_WALLTIME_NOISE_THRESHOLD: f64 = 0.01; // 1%
static APP_NAME: &str = "rustls-benchmarking";

/// Functions inside this module will be available as askama filters
mod filters {
    use std::borrow::Borrow;

    use super::*;

    pub fn format_timing(
        timing_ns: impl Borrow<f64>,
        unit: impl Borrow<TimeUnit>,
    ) -> askama::Result<String> {
        // Note: we need to use `Borrow` to sidestep askama limitations (plain types result in
        // compile errors)
        let timing_ns = *timing_ns.borrow();
        let unit = *unit.borrow();

        let (number, unit, precision) = match unit {
            TimeUnit::Nanoseconds => (timing_ns, "ns", 0),
            TimeUnit::Microseconds => (timing_ns / 1_000.0, "µs", 2),
            TimeUnit::Milliseconds => (timing_ns / 1_000_000.0, "ms", 2),
            TimeUnit::Seconds => (timing_ns / 1_000_000_000.0, "s", 2),
        };

        Ok(format!("{number:.0$} {unit}", precision))
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn split_on_threshold_sorts_using_absolute_value() {
        fn diff(scenario: &str, baseline: f64, candidate: f64) -> ScenarioDiff {
            ScenarioDiff {
                scenario_name: scenario.to_string(),
                scenario_kind: ScenarioKind::Icount,
                baseline_result: baseline,
                candidate_result: candidate,
                significance_threshold: f64::MAX, // Everything is negligible
                cachegrind_diff: Some(String::new()),
            }
        }

        let diffs = vec![
            diff("x", 1.2, 1.0),
            diff("y", 1.0, 1.0),
            diff("z", 1.0, 1.25),
        ];

        let (significant, negligible) = split_on_threshold(diffs);
        assert!(significant.is_empty());
        assert_eq!(negligible[0].scenario_name, "z");
        assert_eq!(negligible[1].scenario_name, "x");
        assert_eq!(negligible[2].scenario_name, "y");
    }

    #[test]
    fn test_common_time_unit() {
        assert_eq!(common_time_unit(500.0, 999.0), TimeUnit::Nanoseconds);
        assert_eq!(common_time_unit(500.0, 1_999.0), TimeUnit::Microseconds);
        assert_eq!(common_time_unit(1_000.0, 1_999.0), TimeUnit::Microseconds);
        assert_eq!(
            common_time_unit(1_000_000.0, 1_999.0),
            TimeUnit::Milliseconds
        );
        assert_eq!(
            common_time_unit(1_000_000_000.0, 1_999.0),
            TimeUnit::Seconds
        );
    }

    #[test]
    fn format_timing() {
        assert_eq!(
            filters::format_timing(100.0, TimeUnit::Nanoseconds).unwrap(),
            "100 ns"
        );
        assert_eq!(
            filters::format_timing(1_500.0, TimeUnit::Microseconds).unwrap(),
            "1.50 µs"
        );
        assert_eq!(
            filters::format_timing(1_250_000.0, TimeUnit::Milliseconds).unwrap(),
            "1.25 ms"
        );
        assert_eq!(
            filters::format_timing(1_420_000_000.0, TimeUnit::Seconds).unwrap(),
            "1.42 s"
        );
    }

    #[test]
    fn calculate_significance_thresholds_not_enough_results() {
        let thresholds = calculate_significance_thresholds(std::iter::empty());
        assert_eq!(thresholds.len(), 0);
    }

    #[test]
    fn calculate_significance_thresholds_many_results() {
        let historical_results = vec![
            100.0, 97.0, 98.0, 101.0, 100.0, 99.0, 97.0, 102.0, 99.0, 98.0,
        ];

        let bench_results = historical_results.into_iter().map(|result| BenchResult {
            scenario_name: "foo".to_string(),
            scenario_kind: ScenarioKind::Icount,
            result,
        });
        let thresholds = calculate_significance_thresholds(bench_results);

        assert_eq!(thresholds.len(), 1);
        assert_eq!((thresholds["foo"] * 100.0).round(), 9.0);
    }

    #[test]
    fn compare_results_with_different_thresholds() {
        let baseline = HashMap::from([
            ("foo".to_string(), 42.0),
            ("bar".to_string(), 42.0),
            ("baz".to_string(), 42.0),
        ]);
        let candidate = HashMap::from([
            ("foo".to_string(), 40.0),
            ("bar".to_string(), 42.0),
            ("baz".to_string(), 42.0),
        ]);
        let thresholds = HashMap::from([("foo".to_string(), 0.005), ("baz".to_string(), 0.02)]);
        let (diffs, missing) = compare_results(
            Path::new("dummy"),
            &baseline,
            &candidate,
            &thresholds,
            ScenarioKind::Walltime,
            DEFAULT_WALLTIME_NOISE_THRESHOLD,
            MINIMUM_WALLTIME_NOISE_THRESHOLD,
        )
        .unwrap();

        assert_eq!(missing.len(), 0);
        assert_eq!(diffs.len(), 3);

        let diffs: HashMap<_, _> = diffs
            .into_iter()
            .map(|d| (d.scenario_name.clone(), d))
            .collect();

        // The significance threshold was clamped to the minimum threshold
        assert_eq!(
            diffs["foo"].significance_threshold,
            MINIMUM_WALLTIME_NOISE_THRESHOLD
        );

        // No significance threshold for this one, so the default was used
        assert_eq!(
            diffs["bar"].significance_threshold,
            DEFAULT_WALLTIME_NOISE_THRESHOLD
        );

        // Significance threshold was above minimum, so it was unchanged
        assert_eq!(diffs["baz"].significance_threshold, 0.02);
    }
}
