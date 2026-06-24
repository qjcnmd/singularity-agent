# Phase 1D Task Controller

Phase 1D adds a thin task lifecycle controller above the existing planner, model, tool protocol, verification, and review runtimes.

## Types

- `RunLifecycleStatus`
- `RunControlEventKind`
- `RunControlEvent`
- `RunCheckpointStore`
- `RunOutcomeReducer`
- `RunController`

## Lifecycle Statuses

The controller materializes the checklist lifecycle states on `TaskState.lifecycle_status`:

- `created`
- `running`
- `waiting_user`
- `waiting_approval`
- `verifying`
- `repairing`
- `final_review`
- `reporting`
- `completed`
- `blocked`
- `failed`
- `cancelled`

The existing planner `TaskStatus` remains as the planner phase/status surface for compatibility. `lifecycle_status` is the controller-owned task lifecycle surface and is included in planner context.

## Controller Responsibilities

`RunController` owns the outer task turn loop through `run_loop()`. `AgentLoop.run()` still owns per-turn model/tool orchestration, but no longer owns the `for turn` lifecycle loop directly.

The controller:

- starts tasks through `Planner.start_task()`
- records lifecycle events to trace and planner event storage
- reduces `ExecutionOutcome` into lifecycle status changes
- maps Tool Protocol `next_action` values into lifecycle events
- checkpoints through the existing `PlannerStore`
- resumes from existing planner checkpoints
- preserves waiting states for pending approval and user input

## Reducer Behavior

`RunOutcomeReducer` maps outcomes without letting non-terminal results end the task:

- `approval_required` -> `waiting_approval`
- `user_input_required` -> `waiting_user`
- `retryable` / `replan_required` -> `running` or `repairing`
- `success` with `next_action=finalize` -> `completed`
- `blocked` -> `blocked`
- `fatal` -> `failed`

Tool Protocol recovery and turn results are mapped to `RunControlEvent`:

- `pending_approval` / `resume_pending_approval` -> `waiting_approval`
- `ask_user` / `request_user_input` -> `waiting_user`
- `await_tool_result`, `execute_pending_tool`, `append_tool_message`, `request_model`, `continue` -> `running`
- `finalize` -> `reporting`

## Resume Boundaries

The controller reuses the existing planner checkpoint files under `.singularity/planner/<session_id>/`. It does not introduce a second durable state store. `RunCheckpointStore` is a small adapter over `PlannerStore` so the lifecycle owner can checkpoint and resume without duplicating persistence.

## Context Boundary

When an existing `ContextManager` is supplied to `AgentLoop`, the agent now calls `ContextManager.set_user_goal()` so pending approval or user-input resumes keep the current task goal in both in-memory messages and context items.

## Non-Goals

- No Rust, Desktop, Tauri, MCP, multi-agent, plugin marketplace, or Future item changes.
- No rewrite of planner evidence, verification, review, or tool protocol internals.
- No second source of truth for planner evidence.
