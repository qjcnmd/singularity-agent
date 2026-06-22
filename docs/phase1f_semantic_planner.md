# Phase 1F Semantic Planner

Phase 1F adds a contract-aware `RollingPlan` beside the existing deterministic planner phase template.

## Types

- `RollingPlan`
- `PlanStep`
- `PlanDependency`
- `ExpectedEvidence`
- `FallbackStep`
- `SemanticPlannerRuntime`

## Planner Relationship

`PlannerRuntime` remains the deterministic gate and keeps its existing `TaskPlan` phase machine. The new semantic planner generates a task-specific rolling plan and stores it on `TaskState.rolling_plan`.

The rolling plan is not a replacement for policy, mutation, verification, or completion gates. It gives the agent a requirement-aware explanation of what step is current, which evidence is expected, and which capabilities are needed.

## Initial Plan

`SemanticPlannerRuntime.initial_plan()` converts `TaskContract` acceptance criteria into ordered steps:

- an initial context inspection step
- one step per acceptance criterion
- dependencies between criterion steps
- allowed capabilities derived from required evidence
- expected evidence entries bound to acceptance criteria
- fallback steps for missing evidence

## Repair Plan

`SemanticPlannerRuntime.repair_plan()` converts a `FailureAnalysis` into a repair rolling plan. The repair step binds back to the failed verification criterion when the task contract contains one.

## Tool Filtering

`PlannerRuntime.filtered_tools()` still starts from the deterministic phase allowed tools. It then adds the current rolling step's allowed capabilities so the phase template does not hide a tool required by the current semantic step.

## Context

Planner context now includes `rolling_plan`:

- `plan_id`
- `current_step_id`
- `steps`

This lets later final reports cite step evidence instead of referring only to `_default_plan()` phases.

## Boundaries

- No Future, Rust, Desktop, Tauri, MCP, multi-agent, or plugin marketplace changes.
- No replacement of `PlannerRuntime` deterministic gates.
- No automatic semantic model planner call; this phase is deterministic and contract-aware.
