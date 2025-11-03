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

/// Reads the (benchmark, result) pairs from previous CSV output
pub fn maybe_read_memory_results(path: &Path) -> anyhow::Result<HashMap<String, MemoryDetails>> {
    trace!(
        path = path.display().to_string(),
        "reading memory results from CSV file"
    );

    let mut results = HashMap::new();
    let results_file = match File::open(path) {
        Ok(f) => f,
        Err(e) => {
            trace!("failed to open file {e:?} (ignoring)");
            return Ok(results);
        }
    };
    for line in BufReader::new(results_file).lines() {
        let line = line.context("failed to read line from CSV file")?;
        let line = line.trim();
        let mut parts = line.split(',');

        let scenario = parts.next().ok_or(anyhow!("empty line"))?.to_string();
        let counts: Result<Vec<_>, _> = parts.map(|s| s.parse::<u64>()).collect();
        let details = match counts.context("invalid u64 in row")?.as_slice() {
            [heap_total_bytes, heap_total_blocks, heap_peak_bytes, heap_peak_blocks] => {
                MemoryDetails {
                    heap_total_bytes: *heap_total_bytes,
                    heap_total_blocks: *heap_total_blocks,
                    heap_peak_bytes: *heap_peak_bytes,
                    heap_peak_blocks: *heap_peak_blocks,
                }
            }
            _ => bail!("incorrect number of measurements for memory results row"),
        };

        results.insert(scenario, details);
    }

    Ok(results)
}

fn icounts_path(base: &Path) -> PathBuf {
    base.join("results/icounts.csv")
}

pub fn walltimes_path(base: &Path) -> PathBuf {
    base.join("results/walltimes.csv")
}

pub fn memory_path(base: &Path) -> PathBuf {
    base.join("results/memory.csv")
}

#[derive(Copy, Clone, Debug, Default)]
pub(crate) struct MemoryDetails {
    pub(crate) heap_total_bytes: u64,
    pub(crate) heap_total_blocks: u64,
    pub(crate) heap_peak_bytes: u64,
    pub(crate) heap_peak_blocks: u64,
}

impl MemoryDetails {
    /// Which field we use to compare two `MemoryDetails`
    fn comparison_basis(&self) -> u64 {
        self.heap_total_bytes
    }

    /// Returns a string, like the `Display` impl, that illustrates
    /// the difference between the baseline `self` and `candidate`.
    pub(crate) fn diff_string(&self, candidate: MemoryDetails) -> String {
        let heap_total_bytes = candidate.heap_total_bytes as f64 - self.heap_total_bytes as f64;
        let heap_total_blocks = candidate.heap_total_blocks as f64 - self.heap_total_blocks as f64;
        let heap_peak_bytes = candidate.heap_peak_bytes as f64 - self.heap_peak_bytes as f64;
        let heap_peak_blocks = candidate.heap_peak_blocks as f64 - self.heap_peak_blocks as f64;

        format!(
            "∑ {heap_total_bytes:+}B  {heap_total_blocks:+}a<br/>🔝 {heap_peak_bytes:+}B  {heap_peak_blocks:+}a"
        )
    }
}

impl std::fmt::Display for MemoryDetails {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "∑ {}B  {}a<br/>🔝 {}B  {}a",
            self.heap_total_bytes,
            self.heap_total_blocks,
            self.heap_peak_bytes,
            self.heap_peak_blocks
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_diff() {
        let start = MemoryDetails {
            heap_total_bytes: 10,
            heap_total_blocks: 0,
            heap_peak_bytes: 20,
            heap_peak_blocks: 30,
        };
        let end = MemoryDetails {
            heap_total_bytes: 10,
            heap_total_blocks: 20,
            heap_peak_bytes: 15,
            heap_peak_blocks: 10,
        };
        assert_eq!(start.diff_string(end), "∑ +0B  +20a<br/>🔝 -5B  -20a");
    }
}
