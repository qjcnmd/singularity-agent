# Phase 1E Resolution Report: Failure Analyzer / Repair Planner

Date: 2026-06-22

## Scope

Phase 1E adds structured failure analysis and repair planning to the current Python verification path.

This phase did not modify Rust, Desktop, Tauri, MCP, multi-agent, plugin marketplace, or Future items.

## Existence Check

Before this loop, Singularity already had useful verification failure primitives:

- `FailureParserRegistry` parsed pytest, TypeScript, ESLint, Python traceback, command, and generic failures.
- `RepairHintGenerator` generated local hints from parsed failures.
- `RepairLoopController` tracked repeated failure fingerprints.
- `PlannerRuntime.replan()` could route generic verification failures to `repairing_failures`.

The Phase 1E contract was still missing:

- No `FailureAnalysisRuntime`.
- No `FailureAnalysis`, `RootCauseHypothesis`, `RepairPlan`, `RepairStep`, or `NoProgressGuard` contract surface.
- No verification observation field for structured failure analysis.
- No planner evidence fields for failure analysis and repair plans.
- No repair plan with a required next verification command.
- Repeated failures were budgeted, but not exposed as a structured no-progress repair route.

## Plan

1. Add failure analysis and repair planning types in the verification package.
2. Generate failure analysis from failed verification results, stdout/stderr excerpts, parsed failures, changed files, and task contract context.
3. Bind every repair plan to the original verification command argv.
4. Add no-progress guard behavior for repeated identical failures.
5. Attach `failure_analysis` and `repair_plan` to verification observations.
6. Persist failure analysis and repair plans into planner evidence and planner context.
7. Add tests for failing pytest analysis, repeated failure no-progress, golden fail-repair-rerun flow, and planner context integration.

## Changes

- `src/singularity/verification/failure_analysis.py`
  - Added Phase 1E failure analysis, repair plan, and no-progress runtime types.
- `src/singularity/verification/runtime.py`
  - Generates `failure_analysis` and `repair_plan` for failed checks in `run_plan()` and `rerun_check()`.
  - Binds repair plans to original `VerificationPlan` command argv.
- `src/singularity/verification/__init__.py`
  - Exports Phase 1E runtime and model types.
- `src/singularity/planner/models.py`
  - Persists `EvidenceLedger.failure_analyses` and `EvidenceLedger.repair_plans`.
- `src/singularity/planner/runtime.py`
  - Records failure analyses and repair plans from verification observations.
- `src/singularity/planner/context.py`
  - Exposes latest failure analysis and repair plan in planner context.
- `tests/test_failure_analysis_runtime.py`
  - Adds Phase 1E coverage.

## Verification

Targeted Phase 1E validation:

```powershell
.\.venv\Scripts\python.exe -m pytest tests\test_failure_analysis_runtime.py tests\test_verification_runtime.py tests\test_planner_runtime.py tests\test_agent_task_outcome.py tests\test_agent.py tests\test_task_controller.py tests\test_context_policy_planner_integration.py --basetemp work/pytest-tmp
```

Result:

```text
63 passed
```

Full repository validation:

```powershell
.\.venv\Scripts\python.exe -m pytest tests --basetemp work/pytest-tmp
```

Result:

```text
642 passed, 4 skipped
```

Publish verification:

```powershell
git rev-list --left-right --count origin/main...HEAD
```

Result:

```text
0 0
```

Direct `git push origin main` succeeded for this phase. A follow-up `git ls-remote --heads origin main` check timed out, but local tracking alignment was verified.

## Risks

- Repair plans are advisory and evidence-bound; they do not auto-edit code.
- Failure type promotion from runtime smoke to unit test failure is limited to pytest commands or parsed test names.
- Dynamic retrieval hook is represented as structured retrieval queries; Phase 1F can consume these when rolling planning is added.
- The existing untracked `docs/reports/codebase-fact-report.md` was left untouched and is not part of this phase.
