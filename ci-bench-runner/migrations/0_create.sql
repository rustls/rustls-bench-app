CREATE TABLE bench_runs(
    id BLOB PRIMARY KEY,
    created_utc TEXT NOT NULL
) STRICT;

CREATE INDEX idx_bench_runs_created_utc ON bench_runs(created_utc);

CREATE TABLE bench_results(
   bench_run_id BLOB NOT NULL,
   scenario_name TEXT NOT NULL,
   scenario_kind INTEGER NOT NULL,
   result REAL NOT NULL,
   FOREIGN KEY (bench_run_id) REFERENCES bench_runs(id)
) STRICT;

CREATE TABLE comparison_runs(
    id BLOB PRIMARY KEY,
    created_utc TEXT NOT NULL,
    baseline_commit TEXT NOT NULL,
    candidate_commit TEXT NOT NULL,
    icount_scenarios_missing_in_baseline TEXT,
    walltime_scenarios_missing_in_baseline TEXT
) STRICT;

CREATE INDEX idx_comparison_run_commits ON comparison_runs(baseline_commit, candidate_commit);

CREATE TABLE scenario_diffs(
    comparison_run_id BLOB NOT NULL,
    scenario_name TEXT NOT NULL,
    scenario_kind INTEGER NOT NULL,
    baseline_result REAL NOT NULL,
    candidate_result REAL NOT NULL,
    significance_threshold REAL NOT NULL,
    cachegrind_diff TEXT,
    FOREIGN KEY (comparison_run_id) REFERENCES comparison_runs(id)
) STRICT;

CREATE INDEX idx_scenario_diffs_by_kind ON scenario_diffs(comparison_run_id, scenario_kind);

CREATE TABLE event_queue(
    id BLOB PRIMARY KEY,
    job_id BLOB,
    event TEXT NOT NULL,
    payload BLOB NOT NULL,
    created_utc TEXT NOT NULL,
    FOREIGN KEY (job_id) REFERENCES jobs(id)
) STRICT;

CREATE TABLE jobs(
    id BLOB PRIMARY KEY,
    event_queued_utc TEXT NOT NULL,
    created_utc TEXT NOT NULL,
    finished_utc TEXT,
    success INTEGER
) STRICT;

CREATE TABLE result_comments(
    pr_number INTEGER PRIMARY KEY,
    comment_id INTEGER NOT NULL
) STRICT;
