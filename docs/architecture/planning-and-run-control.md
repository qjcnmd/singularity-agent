# Planner / Task Execution Component

Singularity v0.0.10 adds `Planner` as the task execution controller. It is not a natural-language checklist. It is a component layer that records task state, decides the current phase, restricts allowed actions and tools, tracks evidence, reacts to failures, enforces budgets, escalates risk, and produces a final report from structured facts.

The compact boundary is:

```txt
AgentLoop
  -> Planner
  -> TaskState + TaskPlan + AgentAction
  -> ToolExecutor action gate
  -> WorkspaceMutationManager / CommandExecutor / VerificationRunner
  -> EvidenceLedger
  -> Replanner + ExecutionBudget + RiskEscalation
  -> CompletionCriteria
  -> FinalReport
  -> trace + planner store + context observation
```

## Why It Is Not A Natural-Language Plan

A natural-language plan is useful for humans, but it is not enough for a production local coding component. It cannot reliably answer whether a tool is allowed in the current phase, whether the same failure repeated too many times, whether verification evidence is missing, or whether final output is based on real execution facts.

`Planner` stores a structured `TaskPlan`, not only prose. Each `TaskPhase` defines:

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

Read phases allow only read/workspace-health tools. Mutation phases allow workspace mutation tools only through `WorkspaceMutationManager`. Verification phases allow verification tools only through `VerificationRunner`, which still executes checks through `CommandExecutor`.

## Core Core Objects

`TaskState` is the live state record for a task:

```txt
task_id, session_id, user_goal, normalized_goal, constraints,
assumptions, current_phase, status, risk_level, created_at,
updated_at, completion_criteria, open_questions, blocked_reasons,
linked_transactions, linked_commands, linked_verifications,
final_assessment, sandbox_capability
```

`sandbox_capability` is written during agent graph wiring from `SandboxManager.capability_summary()`. It records `hard_isolation`, `soft_workspace_isolation`, `no_isolation`, `network_blocked`, `write_scope`, `approval_mode`, available backends, and backend capability flags so a local staging fallback is visible in task state.

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

`EvidenceLedger` records facts linked to component ids:

```txt
inspected_files, relevant_symbols, search_results, applied_changes,
command_results, verification_results, parsed_failures, assumptions,
missing_evidence, unresolved_failures, external_changes, risks,
policy_observations
```

Tool results are compact in model context, but the planner receives richer execution result objects through observer hooks from mutation, command, and verification runners.

## Execution Boundaries

`Planner` does not read or write files, spawn subprocesses, or call the model API. It only makes execution decisions and stores task facts.

`ToolExecutor` owns schema validation, tool policy, timeouts, output limiting, caching, and `ToolResult` wrapping. When a planner is attached, `ToolExecutor` asks `authorize_tool_call(...)` before executing a handler and reports the full `ToolResult` afterward.

`WorkspaceMutationManager` owns file edits, snapshots, policy checks, diffs, atomic writes, journals, rollback data, and mutation trace records. It reports applied changes and transaction ids back to the planner.

`CommandExecutor` owns process execution, resource limits, env redaction, output artifacts, side-effect detection, and command trace records. It reports command ids and semantic results back to the planner.

`VerificationRunner` owns project detection, impact analysis, check planning, execution through `CommandExecutor`, failure parsing, repair hints, flaky handling, and `CompletionAssessment`. It reports verification check ids and final assessment back to the planner.

`WorkspaceStateManager` owns workspace health. On resume, the planner treats conflicted workspace health or external changes as `needs_review`; it does not assume the old context is still valid.

`ContextManager` receives a compact planner context message. Repairing phases prioritize failure evidence and recent changes. Finalizing phases prioritize changed files, verification status, unresolved failures, and risks.

## Dynamic Retrieval And Memory Learning

`RetrievalOrchestrator` turns execution facts into explicit retrieval guidance:

```txt
current PlanStep + FailureAnalysis + changed files + TaskContract
  -> files_to_read
  -> index_queries
  -> memory_queries
```

Verification failures trigger retrieval from `FailureAnalysis.suspect_files` and `retrieval_queries`. Diff observations trigger retrieval from changed files and `ProjectIndex` impact/test-impact results. The output is stored in `EvidenceLedger.retrieval_results` and rendered into planner context as `dynamic_retrieval`.

Retrieval guidance is not the same as inspected evidence. A path in `files_to_read` does not satisfy `inspected_files` until a read/search tool reports real evidence.

`LessonExtractor` gates memory learning. It only forwards a final report to `MemoryLearningPipeline` when the final report status is `completed` and verification is ready or ready-with-warnings. `MemoryLearningPipeline` still owns extraction, redaction, evidence-source checks, quarantine, and manual acceptance.

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

If the criteria are not met, `Planner` returns `blocked`, `needs_review`, or another non-completed status. The model's prose cannot override missing evidence.

Planner `FinalReport` is built from execution facts after final review:

```txt
user_goal, status, files_changed, agent_changes,
command_side_effects, verification_summary, unresolved_issues,
risks, rollback_status, policy_approval_summary, artifacts, next_steps
```

`policy_approval_summary` counts allowed low-risk actions, reviewed actions, denied actions, sandbox-required actions, user-approved actions, high-risk commands, and actions skipped due to policy.

This planner report is distinct from the kernel-level `FinalReport` in `src/singularity/kernel/finalization.py`. The kernel report owns run/session/task ids, kernel status, shutdown reason, cleanup status, recovery summary, component health summary, lifecycle summary, effective config summary, workspace summary, and trace summary. `AgentKernel.final_report()` may include the planner report as a source summary, but the two data structures are intentionally separate.

For read-only goals, completion criteria do not require mutation or verification evidence. For coding goals, finalization requires applied change evidence, a ready or ready-with-warnings verification assessment, and a final review route of `approve`.

Planner finalization writes both a structured JSON report and a markdown artifact. The markdown artifact path is recorded in planner events and trace.

## Persistence And Audit

Planner state is stored under:

```txt
.singularity/planner/<session_id>/
  state.json
  plan.json
  evidence.json
  budget.json
  final_report.json
  final_report.md
  planner_events.jsonl
```

Each planner event includes:

```txt
task_id, session_id, phase, action_id, action_kind,
decision, reason, evidence_refs, budget_state, risk_level,
replan_decision, completion_assessment, timestamp
```

CLI resume uses `--resume-session <session_id>`. Planner state is loaded from the planner store, workspace state is checked again, and conflicted workspaces enter `needs_review` before the model receives more tools.
