{% import "macros.md" as macros %}

# Benchmark results

{% if scenarios_missing_in_baseline.len() > 0 %}

### ⚠️ Warning: missing benchmarks

The following benchmark scenarios are present in the candidate but not in the baseline:

{% for scenario in scenarios_missing_in_baseline %}
* {{scenario}}
{% endfor %}

{% endif %}

## Significant instruction count differences

{% if significant_icount_diffs.is_empty() %}

_There are no significant instruction count differences_

{% else %}

{% call macros::table(significant_icount_diffs, cachegrind_diff_url, true) %}

{% endif %}

## Other instruction count differences

{% if significant_icount_diffs.is_empty() %}

_There are no other instruction count differences_

{% else %}

<details>
<summary>Click to expand</summary>

{% call macros::table(negligible_icount_diffs, cachegrind_diff_url, false) %}

</details>

{% endif %}

## Additional information

{% if let Some(project_id) = bencher_project_id %}
[Historical results](https://bencher.dev/perf/{{project_id}})
{% endif %}

{% call macros::checkout_details(branches) %}
