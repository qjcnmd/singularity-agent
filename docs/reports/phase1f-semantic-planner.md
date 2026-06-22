# Phase 1F Resolution Report: Semantic Planner / RollingPlan

Date: 2026-06-22

## Scope

Phase 1F adds a deterministic, contract-aware rolling planner to the current Python planner path.

This phase did not modify Rust, Desktop, Tauri, MCP, multi-agent, plugin marketplace, or Future items.

## Existence Check

Before this loop, Singularity had:

- `PlannerRuntime` and `_default_plan()` phase templates.
- `TaskContract` acceptance criteria and evidence requirements from Phase 1C.
- `FailureAnalysis` and repair plans from Phase 1E.

The Phase 1F contract was still missing:

- No `SemanticPlannerRuntime`.
- No `RollingPlan`, `PlanStep`, `PlanDependency`, `ExpectedEvidence`, or `FallbackStep`.
- No initial rolling plan generated from `TaskContract`.
- No repair rolling plan generated from `FailureAnalysis`.
- No step-level acceptance criterion, capability, or expected evidence binding.
- Planner context still reflected mostly phase state, not requirement-specific steps.

## Plan

1. Add semantic planner types in the planner package.
2. Generate an initial `RollingPlan` from `TaskContract`.
3. Bind every semantic step to acceptance criteria, allowed capabilities, and expected evidence.
4. Generate repair rolling plans from `FailureAnalysis` and bind repair steps to failed verification criteria.
5. Persist `TaskState.rolling_plan`.
6. Expose rolling plan in planner context.
7. Extend `PlannerRuntime.filtered_tools()` so the current semantic step can expose required tools even when the phase template is narrower.
8. Add tests for multi-requirement plans, repair-step criterion binding, context exposure, and failure-analysis repair rolling plans.

## Changes

- `src/singularity/planner/semantic.py`
  - Added Phase 1F semantic planner types and `SemanticPlannerRuntime`.
- `src/singularity/planner/runtime.py`
  - Builds initial rolling plans in `start_task()`.
  - Updates rolling plans from failure analysis repair paths.
  - Adds `semantic_rolling_plan()`.
  - Merges current step allowed capabilities into `filtered_tools()`.
- `src/singularity/planner/models.py`
  - Persists `TaskState.rolling_plan`.
- `src/singularity/planner/context.py`
  - Renders rolling plan into planner context.
- `src/singularity/planner/__init__.py`
  - Exports semantic planner types.
- `tests/test_semantic_planner_runtime.py`
  - Adds Phase 1F coverage.

## Verification

Targeted Phase 1F validation:

```powershell
.\.venv\Scripts\python.exe -m pytest tests\test_semantic_planner_runtime.py tests\test_planner_runtime.py tests\test_agent.py tests\test_agent_task_outcome.py tests\test_verification_runtime.py tests\test_failure_analysis_runtime.py tests\test_task_controller.py --basetemp work/pytest-tmp
```

Result:

```text
66 passed
```

Full repository validation:

```powershell
.\.venv\Scripts\python.exe -m pytest tests --basetemp work/pytest-tmp
```

Result:

```text
646 passed, 4 skipped
```

Publish proof:

```powershell
git push origin main
```

Direct GitHub access timed out on port 443, so the push was retried with a command-scoped proxy:

```powershell
git -c http.proxy=http://127.0.0.1:7897 -c https.proxy=http://127.0.0.1:7897 push origin main
```

Result:

```text
d94b33b feat: add semantic rolling planner
origin/main...HEAD = 0 0
```

## Risks

- Rolling plans are deterministic and contract-aware; no new model planner call was added.
- `PlannerRuntime` still owns deterministic safety gates. The rolling plan adds current-step capabilities but does not bypass policy or runtime authorization.
- Final report step-evidence rendering is enabled by the rolling plan structure, but full final report generation remains Phase 1G.
- The existing untracked `docs/reports/codebase-fact-report.md` was left untouched and is not part of this phase.
