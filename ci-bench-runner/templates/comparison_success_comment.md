{% import "macros.md" as macros %}

# Benchmark results

## Instruction counts

{% call macros::missing_scenarios(icount.scenarios_missing_in_baseline) %}

#### Significant differences

{% if icount.significant_diffs.is_empty() %}

_There are no significant instruction count differences_

{% else %}

⚠️ There are significant instruction count differences

<details>
<summary>Click to expand</summary>

{% call macros::icount_table(icount.significant_diffs, cachegrind_diff_url, true) %}

</details>

{% endif %}

#### Other differences

{% if icount.significant_diffs.is_empty() %}

_There are no other instruction count differences_

{% else %}

<details>
<summary>Click to expand</summary>

{% call macros::icount_table(icount.negligible_diffs, cachegrind_diff_url, false) %}

</details>

{% endif %}

## Wall-time

{% call macros::missing_scenarios(walltime.scenarios_missing_in_baseline) %}

#### Significant differences

{% if walltime.significant_diffs.is_empty() %}

_There are no significant wall-time differences_

{% else %}

⚠️ There are significant wall-time differences

<details>
<summary>Click to expand</summary>

{% call macros::walltime_table(walltime.significant_diffs, true) %}

</details>

{% endif %}

#### Other differences

{% if walltime.negligible_diffs.is_empty() %}

_There are no other wall-time count differences_

{% else %}

<details>
<summary>Click to expand</summary>

{% call macros::walltime_table(walltime.negligible_diffs, false) %}

</details>

{% endif %}

## Additional information

{% if let Some(project_id) = bencher_project_id %}
[Historical results](https://bencher.dev/perf/{{project_id}})
{% endif %}

{% call macros::checkout_details(branches) %}
