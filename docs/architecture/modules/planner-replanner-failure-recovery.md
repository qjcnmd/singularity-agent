# Planner / Replanner / Failure Recovery Runtime Flow

Runtime flow doc id: planner-replanner-failure-recovery
Source paths:
- src/singularity/agent_loop.py
- src/singularity/planner/engine.py
- src/singularity/planner/replanner.py
- src/singularity/planner/models.py
- src/singularity/planner/semantic_objects.py
- src/singularity/planner/semantic_producers.py
- src/singularity/planner/context.py
- src/singularity/kernel/graph.py
- src/singularity/failure_analysis/__init__.py
- src/singularity/failure_analysis/request.py
- src/singularity/failure_analysis/result.py
- src/singularity/failure_analysis/analyzer.py
- src/singularity/repair/contract.py
- src/singularity/repair/plan.py
- src/singularity/repair/planner.py
- src/singularity/repair/signal.py
- src/singularity/verification/contract.py
- src/singularity/verification/satisfaction.py
- src/singularity/planner/final_reviewer.py
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
- VerificationContract
- VerificationStep
- ContractSatisfaction
- StepEvidence
- Planner.assess_verification_contract_satisfaction
- FailureAnalyzer
- FailureAnalyzer.analyze
- FailureAnalyzer._model_request
- RepairPlanner
- RepairPlanner.plan
- RepairPlanner.to_replan_signal
- RepairPlanner.blocked_outcome
- RunController
- RunController.run_loop
- PlannerProducerBundle
- TaskContractProducer
- SemanticPlanProducer
- PlannerDecisionProducer
- Planner.attach_producers
- Planner._producer_context
- RiskPoint
- VerificationStrategy
- RepairPolicy
- SemanticPlan
- PlannerDecision
- AgentGraphBuilder._wire_planner
- FinalReviewer
- FinalReviewer.assess
- FinalReviewer._assess_criterion
- FinalReviewer._model_confirm
- CompletionAssessment
- CriterionAssessment
- Planner._run_final_reviewer_assessment
- EvidenceLedger.query_evidence
- EvidenceLedger.evidence_for_criterion

Field checks:
- FailureAnalysisRequest: request_id, run_id, session_id, task_id, phase_id, workspace_root, failure_source, failure_summary, failure_sources, context_references, recent_tail, verification_log_refs, changed_files, evidence_refs, metadata, risk_points, repair_policy, verification_strategies
- FailureAnalysisResult: analysis_id, request_id, root_cause, failure_category, affected_files, evidence_refs, repair_strategy, next_actions, verification_plan, confidence, needs_user_input, blocked_reason, raw_response_ref, verification_contract
- RepairContract: contract_id, analysis_id, failure_category, target_files, evidence_refs, action_candidates, verification_plan, confidence, allowed_tool_names, needs_user_input, blocked_reason, validation_errors, verification_contract
- RepairPlan: plan_id, analysis_id, strategy, summary, action_candidates, next_actions, verification_plan, evidence_refs, confidence, needs_user_input, blocked_reason, repair_contract, verification_contract
- RepairReplanSignal: signal_id, repair_plan_id, analysis_id, contract_id, failure_fingerprint, failure_category, target_files, action_candidates, verification_plan, confidence, needs_user_input, blocked_reason, repair_contract, error_code, verification_failed, verification_contract
- VerificationContract: contract_id, steps, status, validation_errors
- VerificationStep: step_id, command, kind, required
- ContractSatisfaction: contract_id, satisfied, completed_steps, failed_steps, skipped_steps, reason, step_evidence
- StepEvidence: step_id, check_id, command_id, status, artifact_ref

## Module Boundary

This module owns task phase state, planner evidence, replan decisions, failure analysis, repair contract creation, and terminal outcome selection.

It is responsible for moving tasks through phases, authorizing tools by phase and repair contract, recording tool/verification/review evidence, deciding when to replan, calling the failure analyzer, converting failure analysis to repair signals, and deciding whether the run can continue or must ask the user.

It is not responsible for executing tools, parsing provider tool-call protocol, enforcing low-level policy permissions, or storing trace artifacts.

## Current Source Locations

- `src/singularity/agent_loop.py`: failure-analysis trigger and terminal outcome handling.
- `src/singularity/planner/engine.py`: planner state, evidence, tool authorization, replan, finalization, model context rendering.
- `src/singularity/planner/replanner.py`: rule-based replan decision.
- `src/singularity/planner/models.py`: task state, evidence, budget, replan decision, final report.
- `src/singularity/failure_analysis/request.py`: `FailureAnalysisRequest` and model payload construction.
- `src/singularity/failure_analysis/result.py`: `FailureAnalysisResult` model payload validation and serialization.
- `src/singularity/failure_analysis/analyzer.py`: `FailureAnalyzer` model call boundary.
- `src/singularity/failure_analysis/__init__.py`: compatibility re-export for the old `singularity.failure_analysis` import path.
- `src/singularity/repair/contract.py`: `RepairContract`, fail-closed categories, and contract validation.
- `src/singularity/repair/plan.py`: `RepairPlan`.
- `src/singularity/repair/planner.py`: `RepairPlanner`.
- `src/singularity/repair/signal.py`: `RepairReplanSignal`.
- `src/singularity/verification/contract.py`: `VerificationContract` and `VerificationStep`.
- `src/singularity/verification/satisfaction.py`: `ContractSatisfaction`, `StepEvidence`, and `assess_verification_contract_satisfaction()`.
- `src/singularity/run_controller.py`: loop reducer and execution outcome application.
- `src/singularity/planner/semantic_objects.py`: frozen structured objects (`RiskPoint`/`VerificationStrategy`/`RepairPolicy`/`SemanticPlan`/`PlannerDecision`) carrying risk/verification/repair-policy metadata with `producer_source` origin tagging.
- `src/singularity/planner/semantic_producers.py`: model-driven producers (`TaskContractProducer`/`SemanticPlanProducer`/`PlannerDecisionProducer`) + `PlannerProducerBundle`; always try model first, fall back to rules on failure.
- `src/singularity/planner/context.py`: `PlannerContextRenderer.render()` projects planner state summaries (including `risk_points`/`verification_strategies`/`repair_policy`) into the main task model context.
- `src/singularity/kernel/graph.py`: `AgentGraphBuilder._wire_planner` injects `PlannerProducerBundle` into `Planner`.
- `src/singularity/planner/final_reviewer.py`: `FinalReviewer` class + `CompletionAssessment`/`CriterionAssessment` frozen dataclasses; per-criterion completion gate consuming `TaskContract.acceptance_criteria` + `SemanticPlan.verification_strategies` + `EvidenceLedger` + `RiskPoint` mitigation evidence.

## Runtime Call Chain

1. `AgentLoop.run()` starts or resumes planner state through `RunController`.
2. Each turn calls `planner.step()` to select the next action.
3. Tool results flow back through `Planner.update_from_tool_result()`.
4. Completion attempts call `planner.assess_completion()` and `planner.finalize()`.
5. Failed tool/protocol/verification/completion outcomes are reduced by `RunController`.
6. `AgentLoop._maybe_analyze_failure()` decides whether the outcome is repairable.
7. `FailureAnalysisRequest.from_planner(planner, context, ...)` collects failure sources, recent tail, verification refs, changed files, and evidence refs.
8. `FailureAnalyzer.analyze()` builds a `ModelTurnRequest` with `ModelPurpose.FAILURE_ANALYSIS`, no tools, JSON mode, and the bounded `FailureAnalysisRequest.to_model_payload()`.
9. `FailureAnalysisResult.from_model_payload()` validates root cause, failure category, affected files, evidence refs, repair strategy, next actions, verification plan, confidence, and user-input need. The `failure_category` is normalized (`/` and `-` → `_`) before pattern validation (`^[a-z][a-z0-9_]{2,80}$`) to accept common categorical separators like `"environment/configuration"` → `"environment_configuration"`.
10. `RepairPlanner.plan()` creates `RepairPlan` and `RepairContract`.
11. `RepairPlanner.to_replan_signal()` creates `RepairReplanSignal`.
12. `Planner.record_failure_analysis()` now calls `self.producers.semantic_plan.produce_repair(analysis, task_contract=..., context_payload=self._producer_context())` → `SemanticPlanProducer` tries the model (`ModelPurpose.SEMANTIC_PLANNING`, json_mode) then falls back to `SemanticPlanner.repair_plan()`; stores `risk_points`/`verification_strategies`/`repair_policy` on `TaskState`.
13. `Planner.replan()` now calls `self.producers.planner_decision.produce(signal, context_payload=..., risk_points=..., verification_strategies=..., repair_policy=...)` → `PlannerDecisionProducer` tries the model (`ModelPurpose.PLANNER_DECISION`, json_mode) then falls back to `Replanner.decide()`; wraps the result in `ReplanDecision` and updates phase/status.
14. If repair is blocked or requires input, `RepairPlanner.blocked_outcome()` produces a terminal user-input outcome.
15. `Planner.update_from_verification()` calls `self.producers.semantic_plan.produce_repair()` for each failure analysis, updating `rolling_plan` and the `risk_points`/`verification_strategies`/`repair_policy` fields on `TaskState`.
16. `AgentGraphBuilder._wire_planner()` constructs `PlannerProducerBundle.with_rule_fallback(model_runner=..., rule_builder=planner.contract_builder, rule_planner=planner.semantic_planner, rule_replanner=planner.replanner, trace=planner.trace)` and calls `planner.attach_producers(bundle)`. `Planner.step()` relies on the `TaskState` produced by `start_task` through the producers (carrying `risk_points`/`verification_strategies`/`repair_policy`).
17. `Planner.finalize()` now runs `self._run_final_reviewer_assessment()` before marking `COMPLETED`. This builds a `SemanticPlan` from `TaskState.risk_points`/`verification_strategies`/`repair_policy` (or `plan=None` when all three are empty, triggering the fallback coarse bucket-non-empty check) and calls `FinalReviewer.assess(contract=..., plan=..., evidence=..., state=..., context_payload=self._producer_context())`.
18. `FinalReviewer.assess()` walks every `TaskContract.acceptance_criteria` entry, checks each `criterion.evidence` key against `EvidenceLedger.query_evidence()` (bucket non-empty), requires `state.final_assessment.status in {ready, ready_with_warnings}` for `verification_results` evidence, binds `RiskPoint`s via `acceptance_criterion_id` and flags `risk_remaining` when `evidence.command_results` is empty. When `model_runner` is provided, calls `ModelPurpose.FINAL_REVIEW` (json_mode, tools=[]) to *confirm* criteria — the model can flip `satisfied` False→True only when `failed_evidence` is empty and `evidence_refs` are attached; it cannot downgrade True→False. If `overall_satisfied=False`, `Planner.finalize()` sets `TaskStatus.BLOCKED` and returns early.
19. During evaluation benchmark tasks, `Planner.apply_benchmark_constraints()` stores expected file changes. `Planner.update_from_mutation()` and `_auto_advance_before_step()` keep the task in `applying_changes` until all expected files are present in mutation evidence.
20. In `repairing_failures`, `Planner._repair_contract_execution_block()` fails closed when no authoritative `RepairContract` exists, or when the contract is blocked, invalid, low-confidence, or requires user input. `authorize_tool_call()`, `decide_tool_exposure()`, and `filtered_tools()` allow read/evidence tools but block mutation and verification execution until the FailureAnalyzer/RepairPlanner path records a contract.

## Runtime Objects Passed

- `TaskState`: task id, session id, user goal, normalized/effective goal, constraints, assumptions, status, current phase, task contract, rolling plan, risk level, blocked reasons, final assessment, goal revisions, completion criteria.
- `EvidenceLedger`: inspected files, applied changes, command results, verification results, tool results, edit results, review results, risks, unresolved failures, retrieval results, assumptions.
- `FailureAnalysisRequest`: request/run/session/task/phase ids, workspace root, failure source, failure summary, failure sources, context refs, recent tail, verification log refs, changed files, evidence refs, metadata, risk_points, repair_policy, verification_strategies.
- `FailureAnalysisResult`: analysis id, root cause, failure category, affected files, evidence refs, repair strategy, next actions, verification plan, confidence, needs user input, blocked reason, raw response ref, verification contract.
- `RepairContract`: contract id, analysis id, failure category, target files, evidence refs, action candidates, verification plan, confidence, allowed tool names, user-input/blocking flags, validation errors, verification contract.
- `RepairPlan`: plan id, analysis id, failure category, strategy, action candidates, verification plan, repair contract, confidence, user-input/blocking flags.
- `RepairReplanSignal`: signal id, repair plan id, analysis id, failure category, target files, allowed tool names, verification plan, contract, confidence, user-input/blocking flags.
- `ReplanDecision`: decision, reason, updated status, next phase, blocked reason, metadata.
- Benchmark constraints on `TaskState.task_contract["benchmark_constraints"]`: evaluator-supplied allowed tools, expected file changes, completion standard, risk tags, and the public/model-visible verification command.
- `SemanticPlan`: `rolling_plan` + `risk_points` + `verification_strategies` + `repair_policy` + `producer_source` ("model" | "rules" | "rules_fallback").
- `PlannerDecision`: `decision` (`ReplanDecisionKind`) + `reason` + `next_action` (`ActionKind`) + `risk_points_triggered` + `verification_strategy_selected` + `producer_source`.
- `PlannerProducerBundle`: bundles `task_contract` / `semantic_plan` / `planner_decision` producers with shared `model_runner` + `trace`.
- `TaskState` additional fields: `risk_points` (list[dict]), `verification_strategies` (list[dict]), `repair_policy` (dict | None).
- `CompletionAssessment`: `overall_satisfied` (bool), `criteria` (list[CriterionAssessment]), `blocking_reasons` (list[str]), `producer_source` ("rules" | "model" | "rules_no_contract").
- `CriterionAssessment`: `criterion_id`, `description`, `required`, `satisfied`, `missing_evidence` (list[str]), `failed_evidence` (list[str]), `risk_remaining` (list[str]), `evidence_refs` (list[str]), `producer_source` ("rules" | "model").
- `EvidenceLedger.query_evidence(evidence_key)`: returns list of records from the bucket mapped by `_EVIDENCE_KEY_TO_BUCKET` (24-bucket ClassVar); unknown keys fall back to `getattr` lookup.
- `EvidenceLedger.evidence_for_criterion(criterion_id)`: cross-bucket query returning all dict records whose `criterion_id` field matches.

## Model-Visible Objects (模型实际可见对象)

The main task model sees planner state only through rendered context:

- `planner.planner_context_message()` included by `ModelTurnRequestBuilder`;
- context items created from planner state, failures, policy observations, and verification evidence;
- tool result messages and bounded failure observations.
- benchmark expected file changes, allowed scope, risk tags, completion standard, and model-visible verification command when evaluation constraints are active.

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

Semantic Planner producers make their own model calls via `ModelRunner.run_turn(ModelTurnRequest)` with `ModelPurpose.TASK_CONTRACT_EXTRACTION`/`SEMANTIC_PLANNING`/`PLANNER_DECISION`, `ModelPreferences(json_mode=True)`, `ToolChoiceMode.NONE`, `tools=[]`. These calls use a compact `Planner._producer_context()` dict (run_id/session_id/task_id/phase_id/user_goal/task_contract/current_step_id) that is intentionally separate from `PlannerContextRenderer.render()` so producer-internal model calls do not pollute the main task model's context. The main task model still sees planner state only through `PlannerContextRenderer.render()` projections (including `risk_points`/`verification_strategies`/`repair_policy` summaries).

The FinalReviewer model call (`ModelPurpose.FINAL_REVIEW`, json_mode, `tools=[]`, `ToolChoiceMode.NONE`) uses the same `Planner._producer_context()` compact dict and does NOT flow through `PlannerContextRenderer`. The model receives per-criterion assessment summaries + evidence bucket counts; it can only *confirm* (False→True with evidence_refs), never override an evidence-gate failure. This preserves the fail-closed guarantee.

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
- full `RiskPoint`/`VerificationStrategy`/`RepairPolicy`/`SemanticPlan`/`PlannerDecision` objects (persisted to `TaskState` as dicts);
- producer trace events: `semantic_planner.task_contract.model_ok`/`.fallback`, `semantic_planner.semantic_plan.model_ok`/`.fallback`, `semantic_planner.planner_decision.model_ok`/`.fallback`;
- `producer_source` field recording each object's origin (model vs rules vs rules_fallback).
- full `CompletionAssessment` + `CriterionAssessment` objects (with `failed_evidence`/`risk_remaining`/`evidence_refs`) are internal-only; `PlannerContextRenderer` does NOT project them into the main task model context. Trace events: `final_reviewer.assess.done`, `final_reviewer.assess.model_ok`, `final_reviewer.assess.fallback`.

## State Transitions And Failure Paths

- Normal phases include `understanding_task`, `inspecting_workspace`, `planning_changes`, `applying_changes`, `running_verification`, `repairing_failures`, and `finalizing`.
- `Planner.update_from_tool_result()` can trigger `replan()` on failed tool results.
- `Replanner.decide()` asks the user for blocked categories, missing information, low confidence, policy/sandbox/approval/action-not-allowed categories, and repair-budget exhaustion. `RepairContract` validation treats those categories as fail-closed blockers rather than repairable edit categories.
- Patch context, snapshot mismatch, and external-change failures route to fresh reads.
- Verification and semantic failures route to repair.
- Repeated failure fingerprints without new evidence are suppressed.
- Invalid failure-analysis JSON, low confidence, unauthorized affected files, missing evidence refs, or invalid verification plans block repair.
- Completion rejection can trigger failure analysis only after repeated stalled rejection.
- Producer fallback path: model call failure / invalid JSON / schema validation failure / `model_runner` unavailable → automatic fallback to rule path (`TaskContractBuilder.from_rules` / `SemanticPlanner` / `Replanner.decide`). On fallback, a `semantic_planner.{name}.fallback` trace event is emitted with `severity=warning`.
- FinalReviewer gate: `Planner.finalize()` runs `_run_final_reviewer_assessment()` before marking `COMPLETED`. If any required criterion has `missing_evidence`/`failed_evidence`/`risk_remaining`, `overall_satisfied=False` → task moves to `BLOCKED` with `blocking_reasons` and `finalize` returns early (does not mark `COMPLETED`).
- `assess_completion()` blocks completion on any unsatisfied active verification contract, not only contracts with explicit failed steps. This covers no verification results, missing step evidence, empty verification contracts under active repair, and failed steps.
- `assess_verification_contract_satisfaction` fail-closed: when an active repair contract exists but `VerificationContract.steps` is empty → `satisfied=False` (fail-closed); when no active repair exists and steps is empty → `satisfied=True` (no verification needed). When steps exist but `verification_results` is empty → `satisfied=False`. When steps exist and verification ran but `step_evidence` is missing (command didn't match contract step) → `satisfied=False`.
- Benchmark expected-file gate: when `benchmark_constraints.expected_file_changes` is present, mutation evidence must include every normalized expected path before the planner can auto-advance from `applying_changes` to `running_verification`. Missing expected files appear in completion assessment as `benchmark_expected_file_changes`, preventing final completion even if a final report is produced.
- Repair-contract execution gate: if the phase is `repairing_failures` and `_active_repair_contract()` cannot find a contract from the authoritative FailureAnalyzer/RepairPlanner path, mutation tools, edit-plan tools, and executable verification tools are denied with `repair_contract_missing`. Read/evidence tools such as `read_file`, `search_text`, `inspect_diff`, and `get_verification_result` remain available so the agent can gather evidence without bypassing repair analysis.
- Repair-phase completion gate: when the model attempts to finalize while `Planner.assess_completion()` reports `verification_contract_satisfaction` unmet in `repairing_failures`, `AgentLoop._repair_phase_completion_blocked_outcome()` returns a terminal blocked outcome with `repair_budget_exceeded`. This prevents an unrepaired failure from silently consuming turns until `max_turns_exceeded`.
- Benchmark `verification_command` augmentation: when `apply_benchmark_constraints` declares a model-visible verification command from an evaluation task set, two things happen. First, `_apply_benchmark_verification_requirement` overrides the rules-based `TaskContract.verification_requirements` command field with the parsed benchmark argv, so `contract_smoke_commands()` returns the manifest-declared command (e.g. `["python", "-m", "pytest", ...]`) instead of the rules-synthesized `["python", <path>]`. This override is flushed in **both** `apply_benchmark_constraints` (when `planner.state` already exists) and `Planner.start_task` (when constraints were applied before state creation: `evaluation/runner.py` calls `apply_benchmark_constraints` before `KernelBootstrap.boot` -> `AgentLoop.run` -> `RunController.start` -> `planner.start_task`). Second, `_active_repair_verification_contract` augments the active repair contract with that command as an additional `VerificationStep` (`step_id="vstep_benchmark"`, `required=False`). This ensures the gate at `authorize_action` (which checks `smoke_commands` against the active repair `VerificationContract` using exact prefix argv matching) does not deny the canonical manifest-declared public verification command. The benchmark step is an allowance, not a requirement; it does not affect `assess_verification_contract_satisfaction`. Empty contracts (no active repair) already allow all commands and are not augmented. Hidden verification commands and `verification_prepare_commands` stay evaluator-internal; `evaluation/runner.py` passes only `_model_visible_verification_command(task)` into planner benchmark constraints.

## Current Structure Assessment

Planner decisions have been upgraded from pure rules to model-driven with rule fallback. `PlannerProducerBundle` is injected via `AgentGraphBuilder._wire_planner`; producers are the main path in `start_task`/`replan`/`record_failure_analysis`/`update_from_verification`, with rule code (`TaskContractBuilder.from_rules` / `SemanticPlanner` / `Replanner.decide`) retained as fallback inside each producer.

The main complexity is that planner evidence is broad and receives signals from tools, verification, review, policy, context, and failure analysis. Every new evidence type should define whether it can become model-visible through planner context. The producer-internal model calls use a separate compact context (`Planner._producer_context()`) that does not flow through `PlannerContextRenderer`, preventing producer calls from polluting the main task model context.

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

For the Semantic Planner producer layer, the production-grade target is already partially implemented: the producer bundle injection point (`_wire_planner`), the model/rule fallback strategy (producer-internal try/except), and the FinalReviewer per-criterion completion gate are all in place. `FinalReviewer.assess()` verifies `EvidenceLedger` against `SemanticPlan.verification_strategies` + `TaskContract.acceptance_criteria` and assesses residual risk via `RiskPoint.mitigation_strategy` (requiring `evidence.command_results` for mitigation evidence). The model participates via `ModelPurpose.FINAL_REVIEW` but cannot override evidence-gate failures (fail-closed).

Remaining production-grade gaps: `RepairPlanner.plan` does not yet consume `RepairPolicy.max_attempts` (only `allowed_repair_actions` + `escalation_threshold`); `FailureAnalyzer` does not yet consume `PlannerDecision.risk_points_triggered` to escalate to `ASK_USER`.

## Harness Usage Example

The model edits a file and runs verification. Verification fails with parsed pytest errors. `AgentLoop._maybe_analyze_failure()` builds a `FailureAnalysisRequest` from planner evidence and context observations. `FailureAnalyzer` asks the model for a bounded JSON diagnosis. `RepairPlanner` converts it into a repair contract that allows only target files and verification commands. `Planner.replan()` switches to `repairing_failures`. The next model turn sees the repair context and constrained tools.

## Maintenance Rules

Update this document when changing:

- planner phases, state, evidence, completion criteria, or final report fields;
- `Planner.update_from_tool_result()`, `record_failure_analysis()`, `replan()`, `assess_completion()`, or `finalize()`;
- `Planner.update_from_mutation()`, `_auto_advance_before_step()`, benchmark expected-file gating, or repair-contract tool exposure/authorization gates;
- `Replanner.decide()` categories or thresholds;
- `FailureAnalysisRequest.to_model_payload()` or `FailureAnalysisResult.from_model_payload()`;
- `RepairPlanner.plan()`, repair contracts, repair signals, or blocked outcome behavior;
- `AgentLoop._maybe_analyze_failure()` gating.
- `PlannerProducerBundle`, producers, `semantic_objects.py`, `semantic_producers.py`, `_producer_context()`, `attach_producers()`, or `AgentGraphBuilder._wire_planner` producer injection.
- `FinalReviewer.assess()`, `_run_final_reviewer_assessment()`, `assess_verification_contract_satisfaction()` fail-closed branches, `EvidenceLedger.query_evidence()`/`evidence_for_criterion()`, or `CompletionAssessment`/`CriterionAssessment` fields.
- `_active_repair_verification_contract()`, `_augment_with_benchmark_verification_command()`, `_apply_benchmark_verification_requirement`, the `verification_command` field in `apply_benchmark_constraints`/benchmark constraints, or the `start_task` flush site that applies the benchmark `verification_command` to `verification_requirements` when constraints were set before state creation.
- evaluation model-visible benchmark command routing from `evaluation/runner.py` into `Planner.apply_benchmark_constraints()`, especially public/hidden verification boundaries.

## Verification

- `python scripts/verify_runtime_docs.py`
- `python -m pytest tests/test_agent_task_outcome.py tests/test_failure_analysis_pipeline.py tests/test_repair_contract_verification.py tests/test_semantic_planner.py tests/test_semantic_planner_capability.py tests/test_planner.py tests/test_final_reviewer.py --basetemp work/pytest-tmp`

## Last Verified Against

Last verified against commit `5f2202bd8cfcc2a4e4a66c025891550e52f3556e` on 2026-06-26.
