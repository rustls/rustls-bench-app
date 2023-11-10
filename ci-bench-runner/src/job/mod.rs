use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::Path;

use anyhow::{anyhow, bail, Context};
use serde::Deserialize;
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

/// Reads the (benchmark, result) pairs from previous criterion output
pub fn read_walltime_results(path: &Path) -> anyhow::Result<HashMap<String, CriterionResult>> {
    trace!(
        path = path.display().to_string(),
        "reading walltime results from criterion output"
    );

    let mut results = HashMap::new();
    for entry in fs::read_dir(path)
        .with_context(|| format!("failed to list criterion directory at {}", path.display()))?
    {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            let scenario = entry.file_name().to_string_lossy().to_string();
            let file = entry.path().join("base/estimates.json");
            let bytes = fs::read(&file)
                .with_context(|| format!("failed to read criterion file at {}", file.display()))?;
            let result = serde_json::from_slice(&bytes)?;
            results.insert(scenario, result);
        }
    }

    Ok(results)
}

#[derive(Deserialize)]
pub struct CriterionResult {
    pub mean: Measurement,
    pub slope: Option<Measurement>,
}

impl CriterionResult {
    pub fn value(&self) -> f64 {
        self.measurement().point_estimate
    }

    pub fn measurement(&self) -> Measurement {
        self.slope.clone().unwrap_or(self.mean.clone())
    }
}

#[derive(Clone, Deserialize)]
pub struct Measurement {
    pub confidence_interval: ConfidenceInterval,
    pub point_estimate: f64,
    pub standard_error: f64,
}

#[derive(Clone, Deserialize)]
pub struct ConfidenceInterval {
    pub confidence_level: f64,
    pub lower_bound: f64,
    pub upper_bound: f64,
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn parse_criterion_result() {
        let json = crate::test::criterion::SAMPLE_ESTIMATES;
        serde_json::from_str::<CriterionResult>(json).unwrap();
    }
}
