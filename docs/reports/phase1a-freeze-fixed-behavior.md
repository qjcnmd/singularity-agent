# Phase 1A Resolution Report: Freeze Current Fixed Behavior

Date: 2026-06-22

## Scope

Phase 1A freezes already-fixed runtime behavior as regression coverage. This phase did not add new runtime features and did not touch Rust, Desktop, Tauri, MCP, multi-agent, plugin marketplace, or Future items.

## Existence Check

The checked behavior already existed in the current Python runtime:

- Completion rejected for missing evidence maps to `ExecutionOutcomeStatus.REPLAN_REQUIRED`.
- `RETRYABLE` and `REPLAN_REQUIRED` outcomes are non-terminal in `SingularityAgent._terminal_result_from_outcome()`.
- Explicit smoke commands are planned as `RUNTIME_SMOKE` checks and run through `VerificationRuntime`.
- Low-level `workspace_create_file` delegates writes to `MutationRuntime`, with policy and workspace-state participation.

The gap was regression coverage and an explicit implemented/resolved documentation status.

## Plan

1. Strengthen agent outcome tests so premature final answers, retryable protocol failures, and replan-required completions prove the loop continues.
2. Strengthen verification tests so `plan_verification(smoke_commands=...)` proves a required `RUNTIME_SMOKE` check is generated before `run_plan()` executes it.
3. Add a focused mutation-tool regression proving `workspace_create_file` still goes through ToolRuntime policy, delegated MutationRuntime execution, and WorkspaceState tracking.
4. Update runtime docs with a Phase 1A fixed-behavior status table.

## Changes

- `tests/test_agent_task_outcome.py`
  - Added explicit assertions for `replan_required`, `retryable`, `next_action`, and `retry_allowed` evidence.
- `tests/test_verification_runtime.py`
  - Added explicit required `RUNTIME_SMOKE` plan assertions for smoke commands.
- `tests/test_workspace_mutation.py`
  - Added coverage for `workspace_create_file` through ToolRuntime policy, delegated MutationRuntime backend, and WorkspaceState dirty tracking.
- `docs/architecture/runtime-map.md`
  - Added a Phase 1A fixed-behavior status table marking the checked issues as resolved.

## Verification

Targeted Phase 1A validation:

```powershell
.\.venv\Scripts\python.exe -m pytest tests\test_agent_task_outcome.py tests\test_verification_runtime.py tests\test_workspace_mutation.py --basetemp work/pytest-tmp
```

Result:

```text
35 passed, 1 skipped
```

Full repository validation and publish proof are recorded in the final terminal summary for this phase.

Full repository validation:

```powershell
.\.venv\Scripts\python.exe -m pytest tests --basetemp work/pytest-tmp
```

Result:

```text
622 passed, 4 skipped
```

Whitespace validation:

```powershell
git diff --check
```

Result: passed. Git reported CRLF normalization warnings for touched text files only.

## Risks

- This phase intentionally freezes current behavior only. It does not implement TaskContract, TaskController, Failure Analyzer, FinalReportRuntime, Dynamic Retrieval, or Evaluation Golden Tasks.
- The existing untracked `docs/reports/codebase-fact-report.md` was left untouched and is not part of this phase.
