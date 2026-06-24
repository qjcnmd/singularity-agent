# Phase 1A Resolution Report: Freeze Current Fixed Behavior

Date: 2026-06-22

## Scope

Phase 1A freezes already-fixed component behavior as regression coverage. This phase did not add new component features and did not touch Rust, Desktop, Tauri, MCP, multi-agent, plugin marketplace, or Future items.

## Existence Check

The checked behavior already existed in the current Python component:

- Completion rejected for missing evidence maps to `ExecutionOutcomeStatus.REPLAN_REQUIRED`.
- `RETRYABLE` and `REPLAN_REQUIRED` outcomes are non-terminal in `AgentLoop._terminal_result_from_outcome()`.
- Explicit smoke commands are planned as `VERIFICATION_SMOKE` checks and run through `VerificationRunner`.
- Low-level `workspace_create_file` delegates writes to `WorkspaceMutationManager`, with policy and workspace-state participation.

The gap was regression coverage and an explicit implemented/resolved documentation status.

## Plan

1. Strengthen agent outcome tests so premature final answers, retryable protocol failures, and replan-required completions prove the loop continues.
2. Strengthen verification tests so `plan_verification(smoke_commands=...)` proves a required `VERIFICATION_SMOKE` check is generated before `run_plan()` executes it.
3. Add a focused mutation-tool regression proving `workspace_create_file` still goes through ToolExecutor policy, delegated WorkspaceMutationManager execution, and WorkspaceState tracking.
4. Update component docs with a Phase 1A fixed-behavior status table.

## Changes

- `tests/test_agent_task_outcome.py`
  - Added explicit assertions for `replan_required`, `retryable`, `next_action`, and `retry_allowed` evidence.
- `tests/test_verification_runner.py`
  - Added explicit required `VERIFICATION_SMOKE` plan assertions for smoke commands.
- `tests/test_workspace_mutation.py`
  - Added coverage for `workspace_create_file` through ToolExecutor policy, delegated WorkspaceMutationManager backend, and WorkspaceState dirty tracking.
- `docs/architecture/execution-map.md`
  - Added a Phase 1A fixed-behavior status table marking the checked issues as resolved.

## Verification

Targeted Phase 1A validation:

```powershell
.\.venv\Scripts\python.exe -m pytest tests\test_agent_task_outcome.py tests\test_verification_runner.py tests\test_workspace_mutation.py --basetemp work/pytest-tmp
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

- This phase intentionally freezes current behavior only. It does not implement TaskContract, RunController, Failure Analyzer, FinalReportRenderer, Dynamic Retrieval, or Evaluation Golden Tasks.
- The existing untracked `docs/reports/codebase-fact-report.md` was left untouched and is not part of this phase.
