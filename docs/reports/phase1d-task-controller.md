# Phase 1D Resolution Report: TaskController / Lifecycle Reducer

Date: 2026-06-22

## Scope

Phase 1D establishes a task lifecycle owner for the current Python agent path.

This phase did not modify Rust, Desktop, Tauri, MCP, multi-agent, plugin marketplace, or Future items.

## Existence Check

Before this loop, Singularity had several useful primitives but no single task lifecycle controller:

- `SingularityAgent.run()` directly owned the outer turn loop.
- `ExecutionOutcome` existed, but lifecycle reduction happened inside the agent and planner.
- `PlannerRuntime` directly mutated planner status in several methods.
- Tool Protocol recovery produced string `next_action` values, but no task-level event model consumed them.
- Planner checkpoint/resume existed through `PlannerStore`, but no `TaskController` adapter preserved lifecycle waiting states.
- There was a kernel `RunLifecycleManager`, but it tracks run/session lifecycle, not task-level controller state.

## Plan

1. Add a small task controller module with lifecycle status, task events, reducer, and state-store adapter.
2. Persist a controller-owned `TaskState.lifecycle_status`.
3. Move the outer `SingularityAgent.run()` loop into `TaskController.run_loop()`.
4. Route model/protocol/completion outcomes through `TaskController.apply_outcome()`.
5. Map Tool Protocol `next_action` and recovery results into `TaskEvent`.
6. Preserve pending approval and user-input waiting states across checkpoint/resume.
7. Ensure reused `ContextManager` instances keep the current user goal.
8. Add tests for reducer behavior, protocol recovery dispatch, checkpoint/resume, max-turn blocking, and approval wait context preservation.

## Changes

- `src/singularity/task_controller.py`
  - Added `TaskLifecycleStatus`, `TaskEventKind`, `TaskEvent`, `TaskStateStore`, `OutcomeReducer`, and `TaskController`.
- `src/singularity/agent.py`
  - Delegates the outer turn loop to `TaskController.run_loop()`.
  - Routes outcomes through `TaskController.apply_outcome()`.
  - Routes Tool Protocol `next_action` through `TaskController.apply_protocol_result()`.
- `src/singularity/planner/models.py`
  - Persists `TaskState.lifecycle_status`.
- `src/singularity/planner/context.py`
  - Adds `lifecycle_status` to planner context.
- `src/singularity/context/manager.py`
  - Adds `set_user_goal()` for reused context managers.
- `tests/test_task_controller.py`
  - Adds Phase 1D controller, reducer, protocol recovery, resume, and max-turn tests.
- `tests/test_agent_task_outcome.py`
  - Adds approval wait context preservation coverage.
- `tests/agent_runtime_helpers.py`
  - Allows tests to inject an existing `ContextManager`.

## Verification

Targeted Phase 1D validation:

```powershell
.\.venv\Scripts\python.exe -m pytest tests\test_task_controller.py tests\test_agent_task_outcome.py tests\test_agent.py tests\test_planner_runtime.py tests\test_tool_protocol_recovery.py tests\test_tool_protocol_state.py tests\test_verification_runtime.py tests\test_context_recovery_production.py --basetemp work/pytest-tmp
```

Result:

```text
73 passed
```

Full repository validation:

```powershell
.\.venv\Scripts\python.exe -m pytest tests --basetemp work/pytest-tmp
```

Result:

```text
638 passed, 4 skipped
```

Publish verification:

```powershell
git rev-list --left-right --count origin/main...HEAD
```

Result:

```text
0 0
```

Direct `git push origin main` succeeded for this phase.

## Risks

- The controller is intentionally thin. Planner evidence, verification, review, and tool protocol internals still own their subsystem facts.
- Existing `TaskStatus` remains for planner phase compatibility; new lifecycle status is materialized separately as `TaskState.lifecycle_status`.
- Successful low-level tool observations are still recorded by their existing runtimes; the controller consumes protocol turn results and task outcomes for lifecycle decisions.
- The existing untracked `docs/reports/codebase-fact-report.md` was left untouched and is not part of this phase.
