use std::collections::HashMap;

use bencher_client::{
    json::{
        project::metric_kind::INSTRUCTIONS_SLUG_STR, DateTime, JsonMetric, JsonMetricsMap,
        JsonReport, JsonResultsMap, MetricKind,
    },
    types::{Adapter, JsonNewReport, JsonReportSettings},
    BencherClient,
};

use crate::AppConfig;

const DEFAULT_PROJECT_ID: &str = "rustls";
const DEFAULT_TESTBED_ID: &str = "benchmarking-host";

/// Create a new Bencher report
pub fn new_bencher_report(
    config: &AppConfig,
    branch: &str,
    hash: &str,
    start_time: DateTime,
    end_time: DateTime,
    icounts: HashMap<String, f64>,
) -> anyhow::Result<JsonNewReport> {
    // Use `benchmarking-host` as the default Testbed ID
    let testbed = config
        .bencher_testbed_id
        .clone()
        .unwrap_or(DEFAULT_TESTBED_ID.parse().unwrap());

    // The benchmark metric kind is "Instructions" which should have the slug `instructions`
    let instructions: MetricKind = INSTRUCTIONS_SLUG_STR.parse().unwrap();

    // Convert the instruction counts map into Bencher Metric Format (BMF)
    // https://bencher.dev/docs/explanation/adapters#-json
    let results: JsonResultsMap = icounts
        .into_iter()
        .filter_map(|(scenario_name, value)| {
            let benchmark_name = scenario_name.parse().ok()?;
            let mut metrics = JsonMetricsMap::new();
            let metric = JsonMetric {
                value: value.into(),
                ..Default::default()
            };
            metrics.insert(instructions.clone(), metric);
            Some((benchmark_name, metrics))
        })
        .collect();

    Ok(JsonNewReport {
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
    })
}

/// Send the benchmark report to Bencher
pub async fn send_report_to_bencher(
    bencher_client: &BencherClient,
    config: &AppConfig,
    json_new_report: JsonNewReport,
) -> anyhow::Result<()> {
    // Use `rustls` as the default Project ID
    let project_id = config
        .bencher_project_id
        .clone()
        .unwrap_or(DEFAULT_PROJECT_ID.parse().unwrap());

    let project_id = &project_id;
    let new_report = &json_new_report;
    // Send report over to Bencher
    let report: JsonReport = bencher_client
        .send_with(
            |client| async move {
                client
                    .proj_report_post()
                    .project(project_id.clone())
                    .body(new_report.clone())
                    .send()
                    .await
            },
            false,
        )
        .await?;
    tracing::trace!("{}", serde_json::to_string_pretty(&report)?);

    Ok(())
}
