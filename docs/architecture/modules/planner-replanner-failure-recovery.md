# Planner / Replanner / Failure Recovery Runtime Flow

Runtime flow doc id: planner-replanner-failure-recovery
Source paths:
- src/singularity/agent_loop.py
- src/singularity/planner/engine.py
- src/singularity/planner/replanner.py
- src/singularity/planner/models.py
- src/singularity/failure_analysis.py
- src/singularity/verification/failure_analysis.py
- src/singularity/run_controller.py

Symbols:
- AgentLoop
- AgentLoop._maybe_analyze_failure
- AgentLoop._should_analyze_outcome
- AgentLoop._terminal_result_from_outcome
- Planner
- Planner.start_task
- Planner.step
- Planner.update_from_tool_result
- Planner.record_failure_analysis
- Planner.replan
- Planner.assess_completion
- Planner.finalize
- Planner.planner_context_message
- Replanner
- Replanner.decide
- TaskState
- EvidenceLedger
- ReplanDecision
- FinalReport
- FailureAnalysisRequest
- FailureAnalysisRequest.from_planner
- FailureAnalysisRequest.to_model_payload
- FailureAnalysisResult
- RepairContract
- RepairPlan
- RepairReplanSignal
- FailureAnalyzer
- FailureAnalyzer.analyze
- FailureAnalyzer._model_request
- RepairPlanner
- RepairPlanner.plan
- RepairPlanner.to_replan_signal
- RepairPlanner.blocked_outcome
- RunController
- RunController.run_loop

## Module Boundary

This module owns task phase state, planner evidence, replan decisions, failure analysis, repair contract creation, and terminal outcome selection.

It is responsible for moving tasks through phases, authorizing tools by phase and repair contract, recording tool/verification/review evidence, deciding when to replan, calling the failure analyzer, converting failure analysis to repair signals, and deciding whether the run can continue or must ask the user.

It is not responsible for executing tools, parsing provider tool-call protocol, enforcing low-level policy permissions, or storing trace artifacts.

## Current Source Locations

- `src/singularity/agent_loop.py`: failure-analysis trigger and terminal outcome handling.
- `src/singularity/planner/engine.py`: planner state, evidence, tool authorization, replan, finalization, model context rendering.
- `src/singularity/planner/replanner.py`: rule-based replan decision.
- `src/singularity/planner/models.py`: task state, evidence, budget, replan decision, final report.
- `src/singularity/failure_analysis.py`: `FailureAnalysisRequest`, `FailureAnalyzer`, `RepairPlanner`, repair contracts, replan signals.
- `src/singularity/verification/failure_analysis.py`: verification-focused failure analyzer and repair planner helper.
- `src/singularity/run_controller.py`: loop reducer and execution outcome application.

## Runtime Call Chain

1. `AgentLoop.run()` starts or resumes planner state through `RunController`.
2. Each turn calls `planner.step()` to select the next action.
3. Tool results flow back through `Planner.update_from_tool_result()`.
4. Completion attempts call `planner.assess_completion()` and `planner.finalize()`.
5. Failed tool/protocol/verification/completion outcomes are reduced by `RunController`.
6. `AgentLoop._maybe_analyze_failure()` decides whether the outcome is repairable.
7. `FailureAnalysisRequest.from_planner(planner, context, ...)` collects failure sources, recent tail, verification refs, changed files, and evidence refs.
8. `FailureAnalyzer.analyze()` builds a `ModelTurnRequest` with `ModelPurpose.FAILURE_ANALYSIS`, no tools, JSON mode, and the bounded `FailureAnalysisRequest.to_model_payload()`.
9. `FailureAnalysisResult.from_model_payload()` validates root cause, failure category, affected files, evidence refs, repair strategy, next actions, verification plan, confidence, and user-input need.
10. `RepairPlanner.plan()` creates `RepairPlan` and `RepairContract`.
11. `RepairPlanner.to_replan_signal()` creates `RepairReplanSignal`.
12. `Planner.record_failure_analysis()` stores analysis/repair evidence and updates planner state.
13. `Planner.replan()` delegates to `Replanner.decide()` and updates phase/status.
14. If repair is blocked or requires input, `RepairPlanner.blocked_outcome()` produces a terminal user-input outcome.

## Runtime Objects Passed

- `TaskState`: task id, session id, user goal, normalized/effective goal, constraints, assumptions, status, current phase, task contract, rolling plan, risk level, blocked reasons, final assessment, goal revisions, completion criteria.
- `EvidenceLedger`: inspected files, applied changes, command results, verification results, tool results, edit results, review results, risks, unresolved failures, retrieval results, assumptions.
- `FailureAnalysisRequest`: request/run/session/task/phase ids, workspace root, failure source, failure summary, failure sources, context refs, recent tail, verification log refs, changed files, evidence refs, metadata.
- `FailureAnalysisResult`: analysis id, root cause, failure category, affected files, evidence refs, repair strategy, next actions, verification plan, confidence, needs user input, blocked reason, raw response ref, verification contract.
- `RepairContract`: contract id, analysis id, failure category, target files, evidence refs, action candidates, verification plan, confidence, allowed tool names, user-input/blocking flags, validation errors, verification contract.
- `RepairPlan`: plan id, analysis id, failure category, strategy, action candidates, verification plan, repair contract, confidence, user-input/blocking flags.
- `RepairReplanSignal`: signal id, repair plan id, analysis id, failure category, target files, allowed tool names, verification plan, contract, confidence, user-input/blocking flags.
- `ReplanDecision`: decision, reason, updated status, next phase, blocked reason, metadata.

## Model-Visible Objects (模型实际可见对象)

The main task model sees planner state only through rendered context:

- `planner.planner_context_message()` included by `ModelTurnRequestBuilder`;
- context items created from planner state, failures, policy observations, and verification evidence;
- tool result messages and bounded failure observations.

The failure-analysis model call sees `FailureAnalysisRequest.to_model_payload()` fields:

- request id;
- failure source and summary;
- last eight failure sources;
- recent context refs and tail;
- verification log refs;
- changed files;
- allowed target files;
- evidence refs.

That failure-analysis request has `tools=[]` and `ToolChoicePolicy(mode=NONE)`.

## Internal Trace Debug Audit Objects (内部 trace/debug/audit 对象)

Internal-only planning data includes:

- full `TaskState`, `TaskPlan`, `EvidenceLedger`, and `ExecutionBudget`;
- `FailureAnalysisRequest.fingerprint`;
- full failure sources before model payload bounding;
- `FailureAnalysisResult.raw_response_ref`;
- repair contract validation errors;
- duplicate failure analysis fingerprints and snapshots in `AgentLoop`;
- planner store events, dynamic retrieval records, and trace `planner` events;
- repair signal consumed trace payloads.

## State Transitions And Failure Paths

- Normal phases include `understanding_task`, `inspecting_workspace`, `planning_changes`, `applying_changes`, `running_verification`, `repairing_failures`, and `finalizing`.
- `Planner.update_from_tool_result()` can trigger `replan()` on failed tool results.
- `Replanner.decide()` asks the user for blocked categories, missing information, low confidence, permission/policy/sandbox categories, and repair-budget exhaustion.
- Patch context, snapshot mismatch, and external-change failures route to fresh reads.
- Verification and semantic failures route to repair.
- Repeated failure fingerprints without new evidence are suppressed.
- Invalid failure-analysis JSON, low confidence, unauthorized affected files, missing evidence refs, or invalid verification plans block repair.
- Completion rejection can trigger failure analysis only after repeated stalled rejection.

## Current Structure Assessment

The current structure is coherent: planner owns state/evidence, failure analysis owns model-assisted diagnosis, and replanner owns conservative rule-based decisions.

The main complexity is that planner evidence is broad and receives signals from tools, verification, review, policy, context, and failure analysis. Every new evidence type should define whether it can become model-visible through planner context.

## Production-Grade Target Structure

Current code has no single durable `FailureRecoveryRun` object spanning analyzer, repair planner, planner replan, verification contract, and final outcome.

A production-grade target could add proposed fields:

- proposed `failure_recovery_id`;
- proposed `failure_fingerprint_hash`;
- proposed `repair_contract_id`;
- proposed `evidence_snapshot_before`;
- proposed `evidence_snapshot_after`;
- proposed `user_input_boundary_reason`.

These are proposed. Current code distributes equivalent information across request/result/plan/signal dictionaries, planner evidence, trace, and AgentLoop in-memory maps.

## Harness Usage Example

The model edits a file and runs verification. Verification fails with parsed pytest errors. `AgentLoop._maybe_analyze_failure()` builds a `FailureAnalysisRequest` from planner evidence and context observations. `FailureAnalyzer` asks the model for a bounded JSON diagnosis. `RepairPlanner` converts it into a repair contract that allows only target files and verification commands. `Planner.replan()` switches to `repairing_failures`. The next model turn sees the repair context and constrained tools.

## Maintenance Rules

Update this document when changing:

- planner phases, state, evidence, completion criteria, or final report fields;
- `Planner.update_from_tool_result()`, `record_failure_analysis()`, `replan()`, `assess_completion()`, or `finalize()`;
- `Replanner.decide()` categories or thresholds;
- `FailureAnalysisRequest.to_model_payload()` or `FailureAnalysisResult.from_model_payload()`;
- `RepairPlanner.plan()`, repair contracts, repair signals, or blocked outcome behavior;
- `AgentLoop._maybe_analyze_failure()` gating.

## Verification

- `python scripts/verify_runtime_docs.py`
- `python -m pytest tests/test_agent_task_outcome.py tests/test_failure_analysis_pipeline.py tests/test_repair_contract_verification.py tests/test_semantic_planner.py tests/test_planner.py --basetemp work/pytest-tmp`

## Last Verified Against

Last verified against commit `5f2202bd8cfcc2a4e4a66c025891550e52f3556e` on 2026-06-25.
