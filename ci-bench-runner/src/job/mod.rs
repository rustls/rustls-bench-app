use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use anyhow::{anyhow, bail, Context};
use tracing::trace;

pub use bench_main::bench_main;
pub use bench_pr::{handle_issue_comment, handle_pr_review, handle_pr_update};

mod bench_main;
mod bench_pr;

/// Reads the (benchmark, instruction count) pairs from previous CSV output
fn read_results(path: &Path) -> anyhow::Result<HashMap<String, f64>> {
    trace!(
        path = path.display().to_string(),
        "reading comparison results from CSV file"
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
