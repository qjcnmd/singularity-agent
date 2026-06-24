# Phase 1E Failure Analysis

Phase 1E adds structured failure analysis and repair planning after verification failures.

## Types

- `FailureAnalysis`
- `RootCauseHypothesis`
- `RepairPlan`
- `RepairStep`
- `NoProgressGuard`
- `FailureAnalysisPipeline`
- `RepairPlanner`

## Component Flow

`VerificationRunner.run_plan()` and `VerificationRunner.rerun_check()` now build structured failure analysis for failed, blocked, timeout, and flaky checks.

For each failed result, `FailureAnalysisPipeline` records:

- failure type
- root-cause hypothesis
- suspect files
- retrieval queries for dynamic context lookup
- bound next verification command
- no-progress guard state

`RepairPlanner` converts one or more analyses into a `RepairPlan` with repair steps and a required `next_verification` command. If any analysis trips the no-progress guard, the repair strategy becomes `stop_and_ask`.

## Planner Integration

Verification observations include:

- `verification.failure_analysis`
- `verification.repair_plan`

`Planner.update_from_verification()` persists those values into `EvidenceLedger.failure_analyses` and `EvidenceLedger.repair_plans`. Planner context exposes the latest failure analyses and repair plan so the next model turn is not just told to enter a generic repair phase.

## No-Progress Guard

`NoProgressGuard` fingerprints repeated failures by check id, failure type, file, line, test name, and message. Repeating the same failure beyond the configured retry budget produces `same_failure_retry_budget_exceeded` and routes the plan to `stop_and_ask`.

## Bound Verification

Every generated repair step includes a `next_verification` object:

```text
check_id
command
```

The command is taken from the original `VerificationPlan` command argv, not from display-only redacted command text.

## Boundaries

- This phase does not implement semantic rolling planning; that remains Phase 1F.
- This phase does not auto-edit code from the repair plan.
- This phase does not change Rust, Desktop, Tauri, MCP, multi-agent, plugin marketplace, or Future items.
