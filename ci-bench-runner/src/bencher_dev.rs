use std::collections::HashMap;
use std::sync::Arc;

use bencher_client::{
    json::{
        project::metric_kind::INSTRUCTIONS_SLUG_STR, DateTime, JsonMetric, JsonMetricsMap,
        JsonReport, JsonResultsMap, MetricKind,
    },
    types::{Adapter, JsonNewReport, JsonReportSettings},
    BencherClient,
};
use tracing::error;

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

    /// Sends the instruction counts to bencher.dev for visualization
    pub async fn track_icounts(
        &self,
        branch: &str,
        hash: &str,
        start_time: DateTime,
        end_time: DateTime,
        icounts: HashMap<String, f64>,
    ) -> anyhow::Result<()> {
        let results = icounts_to_bmf(icounts);
        let testbed = self.config.testbed_id.clone();

        let report = JsonNewReport {
            branch: branch.parse()?,
            hash: Some(hash.parse()?),
            testbed: testbed.into(),
            start_time: start_time.into(),
            end_time: end_time.into(),
            results: vec![serde_json::to_string(&results)?],
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

/// Converts the instruction counts map into Bencher Metric Format (BMF)
///
/// See <https://bencher.dev/docs/explanation/adapters#-json> for details
fn icounts_to_bmf(icounts: HashMap<String, f64>) -> JsonResultsMap {
    let instructions: MetricKind = INSTRUCTIONS_SLUG_STR.parse().unwrap();
    icounts
        .into_iter()
        .filter_map(|(scenario_name, value)| {
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
        })
        .collect()
}
