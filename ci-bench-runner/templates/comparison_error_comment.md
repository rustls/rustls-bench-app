{% import "macros.md" as macros %}

# Error running benchmarks

Cause:

```
{{error}}
```

{% call macros::checkout_details(branches) %}

## Logs

### Candidate

{{ candidate_logs }}

### Baseline

{{ baseline_logs }}
