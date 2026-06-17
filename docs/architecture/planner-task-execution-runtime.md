# Planner / Task Execution Runtime

Miniharness v0.0.10 adds `PlannerRuntime` as the task execution controller. It is not a natural-language checklist. It is a runtime layer that records task state, decides the current phase, restricts allowed actions and tools, tracks evidence, reacts to failures, enforces budgets, escalates risk, and produces a final report from structured facts.

The compact boundary is:

```txt
MiniAgent
  -> PlannerRuntime
  -> TaskState + TaskPlan + AgentAction
  -> ToolRuntime action gate
  -> MutationRuntime / CommandRuntime / VerificationRuntime
  -> EvidenceLedger
  -> Replanner + ExecutionBudget + RiskEscalation
  -> CompletionCriteria
  -> FinalReport
  -> trace + planner store + context observation
```

## Why It Is Not A Natural-Language Plan

A natural-language plan is useful for humans, but it is not enough for a production local coding harness. It cannot reliably answer whether a tool is allowed in the current phase, whether the same failure repeated too many times, whether verification evidence is missing, or whether final output is based on real runtime facts.

`PlannerRuntime` stores a structured `TaskPlan`, not only prose. Each `TaskPhase` defines:

```txt
phase_id, name, purpose, allowed_tools, allowed_actions,
entry_conditions, exit_conditions, required_evidence,
failure_policy, risk_notes
```

The default plan is:

```txt
understanding_task
inspecting_workspace
planning_changes
applying_changes
running_verification
repairing_failures
finalizing
```

Read phases allow only read/workspace-health tools. Mutation phases allow workspace mutation tools only through `MutationRuntime`. Verification phases allow verification tools only through `VerificationRuntime`, which still executes checks through `CommandRuntime`.

## Core Runtime Objects

`TaskState` is the live state record for a task:

```txt
task_id, session_id, user_goal, normalized_goal, constraints,
assumptions, current_phase, status, risk_level, created_at,
updated_at, completion_criteria, open_questions, blocked_reasons,
linked_transactions, linked_commands, linked_verifications,
final_assessment
```

The status set includes:

```txt
initialized, understanding_task, inspecting_workspace, planning_changes,
applying_changes, running_verification, repairing_failures, finalizing,
completed, blocked, failed, needs_review, interrupted, recovering
```

`AgentAction` is the unit the model is allowed to perform:

```txt
action_id, kind, intent, phase_id, preconditions, allowed_tools,
expected_evidence, risk_level, status, result_ref
```

The supported action kinds are:

```txt
InspectWorkspace, ReadRelevantFiles, SearchCode, AnalyzeIssue,
ProposeChangeSet, ApplyMutation, RunVerification, ParseFailure,
RepairChange, AskUser, RequireReview, Finalize, Abort
```

`EvidenceLedger` records facts linked to runtime ids:

```txt
inspected_files, relevant_symbols, search_results, applied_changes,
command_results, verification_results, parsed_failures, assumptions,
missing_evidence, unresolved_failures, external_changes, risks,
policy_observations
```

Tool results are compact in model context, but the planner receives richer runtime result objects through observer hooks from mutation, command, and verification runtimes.

## Runtime Boundaries

`PlannerRuntime` does not read or write files, spawn subprocesses, or call the model API. It only makes execution decisions and stores task facts.

`ToolRuntime` owns schema validation, tool policy, timeouts, output limiting, caching, and `ToolResult` wrapping. When a planner is attached, `ToolRuntime` asks `authorize_tool_call(...)` before executing a handler and reports the full `ToolResult` afterward.

`MutationRuntime` owns file edits, snapshots, policy checks, diffs, atomic writes, journals, rollback data, and mutation trace records. It reports applied changes and transaction ids back to the planner.

`CommandRuntime` owns process execution, resource limits, env redaction, output artifacts, side-effect detection, and command trace records. It reports command ids and semantic results back to the planner.

`VerificationRuntime` owns project detection, impact analysis, check planning, execution through `CommandRuntime`, failure parsing, repair hints, flaky handling, and `CompletionAssessment`. It reports verification check ids and final assessment back to the planner.

`LocalWorkspaceStateRuntime` owns workspace health. On resume, the planner treats conflicted workspace health or external changes as `needs_review`; it does not assume the old context is still valid.

`ContextManager` receives a compact planner context message. Repairing phases prioritize failure evidence and recent changes. Finalizing phases prioritize changed files, verification status, unresolved failures, and risks.

## Replanning, Budget, And Risk

`Replanner` maps structured failures to deterministic next decisions:

```txt
patch_context_not_found -> read_fresh_file
snapshot_mismatch -> read_fresh_file
external_change_detected -> read_fresh_file
verification failure -> repair_failure
repeated same failure -> ask_user / blocked
risk escalation -> require_review
```

`ExecutionBudget` tracks:

```txt
max_model_turns, max_tool_calls, max_command_runs,
max_mutation_transactions, max_repair_iterations,
max_changed_files, max_wall_time_seconds,
max_repeated_failures, max_context_growth
```

When a budget is exceeded, the planner stops expanding scope and moves to `blocked`, `failed`, or `needs_review` instead of continuing indefinitely.

`RiskEscalation` can require review for high-risk files such as dependency/config/CI files, high-risk mutation tools, large change scope, verification gaps, or external workspace conflicts. Risk decisions are recorded in the evidence ledger, planner event log, and final report.

## Completion And Final Report

Completion is criteria-driven:

```txt
required_files_inspected
required_changes_applied
required_verifications_passed
unresolved_failures_empty
workspace_health_acceptable
risks_acknowledged
final_report_ready
```

If the criteria are not met, `PlannerRuntime` returns `blocked`, `needs_review`, or another non-completed status. The model's prose cannot override missing evidence.

`FinalReport` is built from runtime facts:

```txt
user_goal, status, files_changed, agent_changes,
command_side_effects, verification_summary, unresolved_issues,
risks, rollback_status, policy_approval_summary, artifacts, next_steps
```

`policy_approval_summary` counts allowed low-risk actions, reviewed actions, denied actions, sandbox-required actions, user-approved actions, high-risk commands, and actions skipped due to policy.

For read-only goals, completion criteria do not require mutation or verification evidence. For coding goals, finalization requires applied change evidence and a ready or ready-with-warnings verification assessment.

## Persistence And Audit

Planner state is stored under:

```txt
.miniharness/planner/<session_id>/
  state.json
  plan.json
  evidence.json
  budget.json
  final_report.json
  planner_events.jsonl
```

Each planner event includes:

```txt
task_id, session_id, phase, action_id, action_kind,
decision, reason, evidence_refs, budget_state, risk_level,
replan_decision, completion_assessment, timestamp
```

CLI resume uses `--resume-session <session_id>`. Planner state is loaded from the planner store, workspace state is checked again, and conflicted workspaces enter `needs_review` before the model receives more tools.
