use std::collections::HashMap;
use std::sync::Arc;

use bencher_client::json::project::metric_kind::LATENCY_SLUG_STR;
use bencher_client::{
    json::{
        project::metric_kind::INSTRUCTIONS_SLUG_STR, DateTime, JsonMetric, JsonMetricsMap,
        JsonReport, JsonResultsMap, MetricKind,
    },
    types::{Adapter, JsonNewReport, JsonReportSettings},
    BencherClient,
};
use tracing::error;

use crate::job::CriterionResult;
use crate::BencherConfig;

/// The Bencher.dev client along with its configuration
#[derive(Clone)]
pub struct BencherDev {
    pub client: BencherClient,
    pub config: Arc<BencherConfig>,
}

impl BencherDev {
    /// Creates a new [BencherDev]
    pub fn new(config: BencherConfig) -> Self {
        let client = BencherClient::builder()
            .token(config.api_token.clone())
            .build();

        Self {
            client,
            config: Arc::new(config),
        }
    }

    /// Sends the icount and walltime results to bencher.dev for visualization
    pub async fn track_results(
        &self,
        branch: &str,
        hash: &str,
        start_time: DateTime,
        end_time: DateTime,
        icounts: HashMap<String, f64>,
        walltimes: HashMap<String, CriterionResult>,
    ) -> anyhow::Result<()> {
        let mut bmf_map = JsonResultsMap::new();
        append_icounts_as_bmf(&mut bmf_map, icounts);
        append_walltimes_as_bmf(&mut bmf_map, walltimes);

        let testbed = self.config.testbed_id.clone();
        let report = JsonNewReport {
            branch: branch.parse()?,
            hash: Some(hash.parse()?),
            testbed: testbed.into(),
            start_time: start_time.into(),
            end_time: end_time.into(),
            results: vec![serde_json::to_string(&bmf_map)?],
            settings: Some(JsonReportSettings {
                adapter: Some(Adapter::Json),
                average: None,
                fold: None,
            }),
        };

        let project_id = self.config.project_id.clone();
        let project_id = &project_id;
        let report = &report;
        let _: JsonReport = self
            .client
            .send_with(
                |client| async move {
                    client
                        .proj_report_post()
                        .project(project_id.clone())
                        .body(report.clone())
                        .send()
                        .await
                },
                false,
            )
            .await?;

        Ok(())
    }
}

/// Converts the instruction counts map into Bencher Metric Format (BMF) and appends them to the map
///
/// See <https://bencher.dev/docs/explanation/adapters#-json> for details
fn append_icounts_as_bmf(results_map: &mut JsonResultsMap, icounts: HashMap<String, f64>) {
    let instructions: MetricKind = INSTRUCTIONS_SLUG_STR.parse().unwrap();
    let bmf_results = icounts.into_iter().filter_map(|(scenario_name, value)| {
        let benchmark_name = match scenario_name.parse() {
            Ok(name) => name,
            Err(_) => {
                error!(
                    scenario_name,
                    "benchmark name does not conform to bencher.dev's requirements, ignoring"
                );
                return None;
            }
        };

        let mut metrics = JsonMetricsMap::new();
        let metric = JsonMetric {
            value: value.into(),
            ..Default::default()
        };
        metrics.insert(instructions.clone(), metric);
        Some((benchmark_name, metrics))
    });
    results_map.extend(bmf_results);
}

/// Converts the criterion results into Bencher Metric Format (BMF) and appends them to the map
fn append_walltimes_as_bmf(
    results_map: &mut JsonResultsMap,
    results: HashMap<String, CriterionResult>,
) {
    let latency: MetricKind = LATENCY_SLUG_STR.parse().unwrap();
    let bmf_results = results.into_iter().filter_map(|(scenario_name, result)| {
        let benchmark_name = match scenario_name.parse() {
            Ok(name) => name,
            Err(_) => {
                error!(
                    scenario_name,
                    "benchmark name does not conform to bencher.dev's requirements, ignoring"
                );
                return None;
            }
        };

        let measurement = result.measurement();
        let mut metrics = JsonMetricsMap::new();
        let metric = JsonMetric {
            value: measurement.point_estimate.into(),
            lower_value: Some(measurement.confidence_interval.lower_bound.into()),
            upper_value: Some(measurement.confidence_interval.upper_bound.into()),
        };
        metrics.insert(latency.clone(), metric);
        Some((benchmark_name, metrics))
    });

    results_map.extend(bmf_results);
}
