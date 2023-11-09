{%- macro table(diffs, cachegrind_diff_url, use_emoji) -%}

| Scenario | Baseline | Candidate | Diff | Threshold |
| --- | ---: | ---: | ---: | ---: |
{% for diff in diffs %}
{%- let emoji -%}
{%- if use_emoji && diff.diff() > 0.0 -%}
{%- let emoji = "⚠️ " -%}
{%- else if use_emoji && diff.diff() < 0.0 -%}
{%- let emoji = "✅ " -%}
{%- else -%}
{%- let emoji = "" -%}
{%- endif -%}
| {{ diff.scenario_name }} | {{ diff.baseline_result }} | {{ diff.candidate_result }} | {{emoji}}[{{diff.diff()}}]({{cachegrind_diff_url}}/{{diff.scenario_name}}) ({{ "{:.2}%"|format(diff.diff_ratio() * 100.0) }}) | {{ "{:.2}%"|format(diff.significance_threshold * 100.0) }} |
{% endfor %}
{%- endmacro -%}

{%- macro checkout_details(branches) -%}

Checkout details:

- Base repo: {{branches.baseline.clone_url}}
- Base branch: {{branches.baseline.branch_name}} ({{branches.baseline.commit_sha}})
- Candidate repo: {{branches.candidate.clone_url}}
- Candidate branch: {{branches.candidate.branch_name}} ({{branches.candidate.commit_sha}})

{%- endmacro -%}
