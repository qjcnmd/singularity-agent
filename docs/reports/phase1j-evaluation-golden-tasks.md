# Phase 1J Resolution Report: Evaluation Golden Tasks

Date: 2026-06-22

## Scope

Phase 1J turns evaluation golden tasks into a concrete, checked-in contract for the current Python CLI baseline.

This phase did not modify Rust, Desktop, Tauri, MCP, multi-agent, plugin marketplace, or Future items.

## Existence Check

Before this loop, Singularity already had:

- `BenchmarkTask`, `ExpectedOutcome`, `GoldenTaskStore`, `EvaluationRuntime`, trace replay, A/B reports, regression reports, and CLI evaluation commands.
- Tests for generic task schema round-trip, store filtering, trace replay determinism, report writing, and CLI evaluation commands.

The Phase 1J contract was still missing:

- No checked-in golden task set covering the ten required scenarios.
- `BenchmarkTask` had no task-level golden contract for expected files, commands, evidence, report sections, or trace artifacts.
- Evaluation reports did not surface golden task evidence in Markdown.
- Regression records did not bind task regressions to trace artifact refs.
- Offline suite scoring scanned the whole workspace for diff outcomes, which made the checked-in Phase 1J suite unsuitable as a fast CI smoke.

## Plan

1. Keep Phase 1J inside the existing `src/singularity/evaluation` runtime.
2. Add an optional, backwards-compatible `golden_contract` to `BenchmarkTask`.
3. Add a checked-in `docs/evaluation/phase1j-golden-tasks.json` covering all ten required scenarios.
4. Carry golden contract evidence into `TaskExecutionEvidence`, JSON reports, and Markdown reports.
5. Add opaque regression `trace_artifact_ref` values and persist per-regression artifacts when a `TraceRuntime` is attached.
6. Make offline suite runs avoid whole-workspace snapshots so the checked-in suite is CI-runnable.
7. Update README and evaluation docs.

## Changes

- `src/singularity/evaluation/models.py`
  - Adds `GoldenTaskContract`.
  - Adds optional `BenchmarkTask.golden_contract`.
  - Adds `phase1j-golden` as a valid task tag.
- `docs/evaluation/phase1j-golden-tasks.json`
  - Adds ten checked-in Phase 1J golden tasks:
    - create file + smoke verify
    - modify bug + test pass
    - verification failure + repair
    - completion rejected + continue
    - final review rejected + repair
    - full markdown report
    - approval required + resume
    - sandbox required / unavailable fail closed
    - dynamic retrieval after failure
    - memory write only after verified completion
- `src/singularity/evaluation/execution.py`
  - Records `execution_evidence.golden_contract`.
  - Marks offline diff outcomes as blocked without scanning the workspace.
  - Writes per-regression trace artifacts when a `TraceRuntime` is present.
- `src/singularity/evaluation/reports.py`
  - Adds `Golden Task Evidence` to Markdown reports.
  - Adds trace artifact refs to regression Markdown.
- `src/singularity/evaluation/runtime.py`
  - Adds stable opaque `trace_artifact_ref` values to regression records.
- `src/singularity/evaluation/__init__.py`
  - Exports `GoldenTaskContract`.
- `README.md`
  - Documents the checked-in Phase 1J task set and golden evidence fields.
- `docs/evaluation-runtime.md`
  - Documents `golden_contract`, Phase 1J scenarios, report evidence, and regression artifact refs.
- Tests
  - Adds coverage in `tests/evaluation/test_models_store.py`.
  - Adds coverage in `tests/evaluation/test_scoring_replay_runtime.py`.

## Verification

Red test proof:

```powershell
.\.venv\Scripts\python.exe -m pytest tests\evaluation\test_models_store.py::test_benchmark_task_round_trips_golden_contract tests\evaluation\test_models_store.py::test_phase1j_golden_task_set_covers_all_required_scenarios tests\evaluation\test_scoring_replay_runtime.py::test_evaluation_report_includes_golden_contract_evidence tests\evaluation\test_scoring_replay_runtime.py::test_regression_report_binds_each_regression_to_trace_artifact_ref --basetemp work\pytest-tmp-phase1j-red
```

Result:

```text
4 failed
```

Additional red proof for CI-runnable offline suites:

```powershell
.\.venv\Scripts\python.exe -m pytest tests\evaluation\test_scoring_replay_runtime.py::test_offline_golden_suite_does_not_scan_workspace_for_diff_outcomes --basetemp work\pytest-tmp-phase1j-red-offline-scan
```

Result:

```text
1 failed
```

Targeted Phase 1J validation:

```powershell
.\.venv\Scripts\python.exe -m pytest tests\evaluation tests\test_cli.py tests\test_trace_artifacts.py tests\test_docs_consistency.py --basetemp work\pytest-tmp-phase1j-targeted3
```

Result:

```text
46 passed, 1 skipped
```

Built-in task set validation:

```powershell
.\.venv\Scripts\python.exe -m singularity.cli eval task validate docs\evaluation\phase1j-golden-tasks.json --json
```

Result:

```text
task_count = 10
```

Offline suite smoke:

```powershell
.\.venv\Scripts\python.exe -m singularity.cli eval suite run docs\evaluation\phase1j-golden-tasks.json --output-dir work\evaluations-phase1j-smoke --run-id phase1j_smoke2
```

Result:

```text
report written to work\evaluations-phase1j-smoke\phase1j_smoke2
```

Full repository validation:

```powershell
.\.venv\Scripts\python.exe -m pytest tests --basetemp work\pytest-tmp-phase1j-final
```

Result:

```text
663 passed, 4 skipped
```

Whitespace validation:

```powershell
git diff --check
```

Result:

```text
exit code 0
```

Note: `git diff --check` printed the normal Windows CRLF conversion warnings, but no whitespace errors.

Publish proof:

```powershell
git push origin main
git rev-list --left-right --count origin/main...HEAD
```

Result:

```text
761c2af feat: add evaluation golden task contracts
origin/main...HEAD = 0 0
```

## Risks

- The checked-in Phase 1J suite is a contract and offline smoke by default. It does not perform a live model run unless a caller explicitly wires full runtime execution.
- Offline suite runs intentionally mark executable test/diff outcomes as blocked instead of pretending they passed.
- `golden_contract` is optional for legacy benchmark tasks to avoid breaking existing task files.
- Regression artifact refs are opaque handles, not local filesystem paths.
- The existing untracked `docs/reports/codebase-fact-report.md` was left untouched and is not part of this phase.
