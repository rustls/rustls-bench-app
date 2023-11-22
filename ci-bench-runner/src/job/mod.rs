use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context};
use tracing::trace;

pub use bench_main::bench_main;
pub use bench_pr::{handle_issue_comment, handle_pr_review, handle_pr_update};

mod bench_main;
mod bench_pr;

/// Reads the (benchmark, result) pairs from previous CSV output
fn read_icount_results(path: &Path) -> anyhow::Result<HashMap<String, f64>> {
    trace!(
        path = path.display().to_string(),
        "reading icount results from CSV file"
    );
    let file = File::open(path).context(format!(
        "CSV file for comparison not found: {}",
        path.display()
    ))?;

    let mut measurements = HashMap::new();
    for line in BufReader::new(file).lines() {
        let line = line.context("failed to read line from CSV file")?;
        let line = line.trim();
        let mut parts = line.split(',');
        measurements.insert(
            parts
                .next()
                .ok_or(anyhow!("CSV is wrongly formatted"))?
                .to_string(),
            parts
                .next()
                .ok_or(anyhow!("CSV is wrongly formatted"))?
                .parse()
                .context("failed to parse instruction count")?,
        );

        if parts.next().is_some() {
            bail!("CSV is wrongly formatted")
        }
    }

    Ok(measurements)
}

/// Reads the (benchmark, result) pairs from previous CSV output
pub fn read_walltime_results(path: &Path) -> anyhow::Result<HashMap<String, f64>> {
    trace!(
        path = path.display().to_string(),
        "reading walltime results from CSV file"
    );

    let mut results = HashMap::new();
    let results_file = File::open(path)?;
    for line in BufReader::new(results_file).lines() {
        let line = line.context("failed to read line from CSV file")?;
        let line = line.trim();
        let mut parts = line.split(',');

        let scenario = parts.next().ok_or(anyhow!("empty line"))?.to_string();
        let walltimes: Result<Vec<_>, _> = parts.map(|s| s.parse::<u128>()).collect();
        let mut walltimes = walltimes.context("invalid f64 in row")?;

        if walltimes.is_empty() {
            bail!("no measurements for walltime results row");
        }

        // Take the median of the results as the official wall-time
        walltimes.sort_unstable();
        let walltime = walltimes[walltimes.len() / 2] as f64;

        results.insert(scenario, walltime);
    }

    Ok(results)
}

fn icounts_path(base: &Path) -> PathBuf {
    base.join("results/icounts.csv")
}

pub fn walltimes_path(base: &Path) -> PathBuf {
    base.join("results/walltimes.csv")
}
