from __future__ import annotations

from contextlib import suppress
from pathlib import Path
from typing import Any
from uuid import uuid4

from singularity.execution_outcome import ExecutionOutcome
from singularity.observability.protocols import TraceRecorderProtocol
from singularity.planner.budget import BudgetController
from singularity.planner.context import PlannerContextRenderer
from singularity.planner.contract import TaskContract, TaskContractBuilder
from singularity.planner.final_reviewer import FinalReviewer
from singularity.planner.finalizer import Finalizer, FinalReportRenderer
from singularity.planner.models import (
    ActionKind,
    ActionStatus,
    AgentAction,
    AuthorizationDecision,
    EvidenceLedger,
    ExecutionBudget,
    FinalReport,
    ReplanDecision,
    ReplanDecisionKind,
    RiskDecisionKind,
    RiskLevel,
    TaskPhase,
    TaskPlan,
    TaskState,
    TaskStatus,
    _now,
)
from singularity.planner.policy import (
    DIFF_TOOLS,
    EDIT_PLAN_TOOLS,
    MUTATION_TOOLS,
    READ_TOOLS,
    VERIFICATION_TOOLS,
    PlannerPolicy,
)
from singularity.planner.replanner import Replanner
from singularity.planner.retrieval import LessonExtractor, RetrievalOrchestrator
from singularity.planner.risk import RiskEscalator
from singularity.planner.semantic import RollingPlan, SemanticPlanner
from singularity.planner.semantic_objects import (
    RepairPolicy,
    RiskPoint,
    VerificationStrategy,
)
from singularity.planner.semantic_producers import PlannerProducerBundle
from singularity.planner.store import PlannerStore
from singularity.tools.models import ToolResult, ToolSpec
from singularity.tools.router import (
    ToolExposureDecision,
    ToolRouter,
    target_paths_from_tool_arguments,
    write_blocked_by_user_constraint,
)
from singularity.verification.contract import VerificationContract, VerificationStep
from singularity.verification.satisfaction import ContractSatisfaction, StepEvidence


class Planner:
    def __init__(
        self,
        workspace_root: Path | str,
        *,
        session_id: str | None = None,
        task_id: str | None = None,
        trace: TraceRecorderProtocol | None = None,
        store: PlannerStore | None = None,
        review_pipeline: Any | None = None,
        final_report_renderer: FinalReportRenderer | None = None,
        project_index: Any | None = None,
        memory_pipeline: Any | None = None,
        retrieval_orchestrator: RetrievalOrchestrator | None = None,
        lesson_extractor: LessonExtractor | None = None,
        producers: PlannerProducerBundle | None = None,
        model_runner: Any | None = None,
        final_reviewer: Any | None = None,
    ) -> None:
        self.workspace_root = Path(workspace_root).resolve(strict=False)
        self.session_id = session_id or uuid4().hex
        self.task_id = task_id or self.session_id
        self.trace = trace
        self.store = store or PlannerStore(self.workspace_root)
        self.policy = PlannerPolicy()
        self.tool_router = ToolRouter()
        self.contract_builder = TaskContractBuilder()
        self.semantic_planner = SemanticPlanner()
        self.risk = RiskEscalator()
        self.replanner = Replanner()
        self.renderer = PlannerContextRenderer()
        self.finalizer = Finalizer()
        self.final_report_renderer = final_report_renderer or FinalReportRenderer()
        self.review_pipeline = review_pipeline
        self.project_index = project_index
        self.memory_pipeline = memory_pipeline
        self.retrieval_orchestrator = retrieval_orchestrator or RetrievalOrchestrator()
        self.lesson_extractor = lesson_extractor or LessonExtractor()
        self.producers = producers or PlannerProducerBundle.with_rule_fallback(
            model_runner=model_runner,
            rule_builder=self.contract_builder,
            rule_planner=self.semantic_planner,
            rule_replanner=self.replanner,
            trace=self.trace,
        )
        self.final_reviewer = final_reviewer or FinalReviewer(
            model_runner=model_runner, trace=self.trace
        )
        self.state: TaskState | None = None
        self.plan: TaskPlan | None = None
        self.evidence = EvidenceLedger()
        self.budget = ExecutionBudget()
        self.final_report: FinalReport | None = None
        self.actions: dict[str, AgentAction] = {}
        self._benchmark_constraints: dict[str, Any] = {}

    def attach_producers(self, bundle: PlannerProducerBundle) -> None:
        """Attach a producer bundle (called by ``AgentGraphBuilder._wire_planner``)."""
        self.producers = bundle

    def _producer_context(self) -> dict[str, Any]:
        """Compact context for producer-internal model calls.

        Intentionally separate from ``PlannerContextRenderer.render()`` (which
        projects to the main task model) so producer calls do not pollute the
        main task model's context.
        """
        if self.state is None:
            return {"task_id": self.task_id, "session_id": self.session_id}
        return {
            "run_id": self.session_id,
            "session_id": self.session_id,
            "task_id": self.state.task_id,
            "phase_id": self.state.current_phase,
            "user_goal": self.state.effective_goal or self.state.normalized_goal,
            "task_contract": self.state.task_contract,
            "current_step_id": (self.state.rolling_plan or {}).get("current_step_id"),
        }

    def start_task(
        self,
        user_goal: str,
        *,
        constraints: list[str] | None = None,
        assumptions: list[str] | None = None,
    ) -> TaskState:
        self._throw_if_cancelled()
        normalized_goal = " ".join(user_goal.split())
        context_payload = self._producer_context()
        contract = self.producers.task_contract.produce(
            normalized_goal, context_payload=context_payload
        )
        semantic_plan = self.producers.semantic_plan.produce_initial(
            contract, context_payload=context_payload
        )
        rolling_plan = semantic_plan.rolling_plan
        self.state = TaskState(
            task_id=self.task_id,
            session_id=self.session_id,
            user_goal=user_goal,
            normalized_goal=normalized_goal,
            effective_goal=normalized_goal,
            constraints=constraints or [],
            assumptions=assumptions or [],
            status=TaskStatus.UNDERSTANDING_TASK,
            current_phase="understanding_task",
            task_contract=contract.to_dict(),
            rolling_plan=rolling_plan.to_dict(),
            risk_points=[rp.to_dict() for rp in semantic_plan.risk_points],
            verification_strategies=[
                vs.to_dict() for vs in semantic_plan.verification_strategies
            ],
            repair_policy=(
                semantic_plan.repair_policy.to_dict()
                if semantic_plan.repair_policy
                else None
            ),
        )
        if self._is_read_only_goal(user_goal):
            self.state.completion_criteria.required_files_inspected = (
                self._requires_workspace_evidence(user_goal)
            )
            self.state.completion_criteria.required_changes_applied = False
            self.state.completion_criteria.required_verifications_passed = False
        self.plan = self._default_plan(self.state.task_id)
        if self._benchmark_constraints:
            task_contract = {
                **self.state.task_contract,
                "benchmark_constraints": dict(self._benchmark_constraints),
            }
            verification_command = str(
                self._benchmark_constraints.get("verification_command") or ""
            )
            if verification_command:
                task_contract = self._apply_benchmark_verification_requirement(
                    task_contract, verification_command
                )
            self.state.task_contract = task_contract
        self.evidence = EvidenceLedger(assumptions=list(self.state.assumptions))
        self.budget = ExecutionBudget()
        self._persist()
        self._record_event(decision="start_task", reason="Task initialized.")
        return self.state

    def apply_benchmark_constraints(self, constraints: dict[str, Any]) -> None:
        self._throw_if_cancelled()
        allowed_tools = sorted(
            dict.fromkeys(str(item) for item in constraints.get("allowed_tools") or [])
        )
        expected_file_changes = sorted(
            dict.fromkeys(str(item) for item in constraints.get("expected_file_changes") or [])
        )
        verification_command = str(constraints.get("verification_command") or "")
        payload = {
            "allowed_tools": allowed_tools,
            "expected_file_changes": expected_file_changes,
            "completion_standard": str(constraints.get("completion_standard") or ""),
            "risk_tags": [str(item) for item in constraints.get("risk_tags") or []],
            "task_id": str(constraints.get("task_id") or ""),
            "verification_command": verification_command,
        }
        self._benchmark_constraints = payload
        if self.state is not None:
            task_contract = {
                **self.state.task_contract,
                "benchmark_constraints": dict(payload),
            }
            if verification_command:
                task_contract = self._apply_benchmark_verification_requirement(
                    task_contract, verification_command
                )
            self.state.task_contract = task_contract
            self._persist()
        if self.trace is not None:
            self.trace.record(
                "planner.benchmark_constraints_applied",
                {"benchmark_constraints": payload},
            )

    def contract_smoke_commands(self) -> list[list[str]]:
        contract = self._contract()
        return contract.smoke_commands() if contract is not None else []

    def record_clarification_answer(self, request: Any, answer: Any) -> TaskState:
        self._throw_if_cancelled()
        state = self._state()
        request_payload = request.to_dict() if hasattr(request, "to_dict") else dict(request)
        answer_payload = answer.to_dict() if hasattr(answer, "to_dict") else dict(answer)
        revised_goal = answer_payload.get("revised_goal")
        revision = {
            "request_id": request_payload.get("request_id"),
            "question": request_payload.get("question"),
            "reason": request_payload.get("reason"),
            "answer": answer_payload.get("answer"),
            "revised_goal": revised_goal,
            "answered_by": answer_payload.get("answered_by"),
            "timestamp": answer_payload.get("timestamp"),
        }
        state.goal_revisions.append(revision)
        if revised_goal:
            state.effective_goal = " ".join(str(revised_goal).split())
        elif answer_payload.get("answer"):
            state.effective_goal = (
                f"{state.effective_goal or state.normalized_goal}\n"
                f"Clarification: {answer_payload['answer']}"
            )
        state.touch()
        self._persist()
        self._record_event(
            decision="clarification_answer",
            reason=str(request_payload.get("reason") or "Clarification answered."),
            extra={"clarification": revision, "effective_goal": state.effective_goal},
        )
        return state

    def checkpoint(self) -> None:
        self._persist()

    def record_task_lifecycle_event(self, payload: dict[str, Any]) -> None:
        self._record_event(
            decision="task_lifecycle",
            reason=str(payload.get("reason") or "Task lifecycle updated."),
            extra={"task_event": payload},
        )

    def step(self) -> AgentAction:
        self._throw_if_cancelled()
        state = self._state()
        plan = self._plan()
        self._auto_advance_before_step()
        BudgetController(self.budget).record_model_turn()
        phase = plan.phase(state.current_phase)
        action = AgentAction(
            kind=phase.allowed_actions[0],
            intent=phase.purpose,
            phase_id=phase.phase_id,
            preconditions=phase.entry_conditions,
            allowed_tools=phase.allowed_tools,
            expected_evidence=phase.required_evidence,
            risk_level=state.risk_level,
            status=ActionStatus.ALLOWED,
        )
        self.actions[action.action_id] = action
        self._persist()
        self._record_event(
            action=action,
            decision="step",
            reason="Planner selected next allowed action.",
        )
        return action

    def authorize_tool_call(
        self,
        *,
        tool_name: str,
        tool_call_id: str | None,
        spec: ToolSpec,
        arguments: dict[str, Any] | None = None,
    ) -> AuthorizationDecision:
        self._throw_if_cancelled()
        state = self._state()
        plan = self._plan()
        phase = plan.phase(state.current_phase)
        normalized_args = self.policy.normalize_arguments(arguments or {})
        repair_allowed = self._active_repair_allowed_tools()
        repair_execution_block = self._repair_contract_execution_block()
        if repair_execution_block and tool_name not in self._repair_contract_evidence_tools():
            error_code, block_reason = repair_execution_block
            decision = AuthorizationDecision(
                allowed=False,
                error_code=error_code,
                reason=(
                    f"{tool_name} requires an executable repair contract before "
                    f"repair-phase execution: {block_reason}."
                ),
            )
            self._record_event(
                action_id=tool_call_id,
                action_kind=self.policy.action_for_tool(tool_name).value,
                decision="deny",
                reason=decision.reason,
                extra={
                    "reason_code": decision.error_code,
                    "repair_contract": self._active_repair_contract(),
                },
            )
            return decision
        if repair_allowed and tool_name not in repair_allowed:
            decision = AuthorizationDecision(
                allowed=False,
                error_code="repair_contract_tool_not_allowed",
                reason=f"{tool_name} is not allowed by the active repair contract.",
            )
            self._record_event(
                action_id=tool_call_id,
                action_kind=self.policy.action_for_tool(tool_name).value,
                decision="deny",
                reason=decision.reason,
                extra={
                    "reason_code": decision.error_code,
                    "repair_contract": self._active_repair_contract(),
                },
            )
            return decision
        benchmark_allowed = self._benchmark_allowed_tools()
        if benchmark_allowed and tool_name not in benchmark_allowed:
            decision = AuthorizationDecision(
                allowed=False,
                error_code="benchmark_tool_not_allowed",
                reason=f"{tool_name} is not allowed by the active benchmark task.",
            )
            self._record_event(
                action_id=tool_call_id,
                action_kind=self.policy.action_for_tool(tool_name).value,
                decision="deny",
                reason=decision.reason,
                extra={
                    "reason_code": decision.error_code,
                    "benchmark_constraints": self._benchmark_constraints,
                },
            )
            return decision
        if not self.policy.is_allowed(phase=phase, tool_name=tool_name, spec=spec):
            decision = AuthorizationDecision(
                allowed=False,
                error_code="action_not_allowed",
                reason=f"{tool_name} is not allowed in phase {phase.phase_id}.",
            )
            self._record_event(
                action_id=tool_call_id,
                action_kind=self.policy.action_for_tool(tool_name).value,
                decision="deny",
                reason=decision.reason,
            )
            return decision

        target_paths = target_paths_from_tool_arguments(tool_name, normalized_args)
        repair_targets = self._active_repair_target_files()
        if (
            repair_targets
            and tool_name in (MUTATION_TOOLS | EDIT_PLAN_TOOLS)
            and target_paths
        ):
            outside_targets = [
                path
                for path in target_paths
                if _normalize_planner_path(path) not in repair_targets
            ]
            if outside_targets:
                decision = AuthorizationDecision(
                    allowed=False,
                    error_code="repair_contract_target_not_allowed",
                    reason="Mutation target is outside the active repair contract.",
                )
                self._record_event(
                    action_id=tool_call_id,
                    action_kind=self.policy.action_for_tool(tool_name).value,
                    decision="deny",
                    reason=decision.reason,
                    extra={
                        "reason_code": decision.error_code,
                        "blocked_paths": outside_targets,
                        "repair_contract": self._active_repair_contract(),
                    },
                )
                return decision
        if write_blocked_by_user_constraint(spec, self._active_user_constraints(), target_paths):
            decision = AuthorizationDecision(
                allowed=False,
                error_code="user_constraint_blocks_write_path",
                reason=f"{tool_name} targets a path blocked by active user constraints.",
            )
            self._record_event(
                action_id=tool_call_id,
                action_kind=self.policy.action_for_tool(tool_name).value,
                decision="deny",
                reason=decision.reason,
                extra={
                    "reason_code": decision.error_code,
                    "blocked_paths": target_paths,
                    "active_user_constraints": self._active_user_constraints(),
                },
            )
            return decision

        # During repair phase, constrain run_verification / rerun_check to the
        # active VerificationContract's allowed commands.
        if tool_name in {"run_verification", "rerun_check"} and repair_allowed:
            smoke_commands = normalized_args.get("smoke_commands")
            if isinstance(smoke_commands, list) and smoke_commands:
                vcontract = self._active_repair_verification_contract()
                disallowed = [
                    cmd
                    for cmd in smoke_commands
                    if isinstance(cmd, list) and not vcontract.is_command_allowed(cmd)
                ]
                if disallowed:
                    decision = AuthorizationDecision(
                        allowed=False,
                        error_code="verification_contract_command_not_allowed",
                        reason=(
                            "Verification command is not in the active VerificationContract: "
                            + "; ".join(" ".join(str(c) for c in cmd) for cmd in disallowed[:3])
                        ),
                    )
                    self._record_event(
                        action_id=tool_call_id,
                        action_kind=self.policy.action_for_tool(tool_name).value,
                        decision="deny",
                        reason=decision.reason,
                        extra={
                            "reason_code": decision.error_code,
                            "disallowed_commands": disallowed,
                            "verification_contract": vcontract.to_dict(),
                        },
                    )
                    return decision

        risk = self.risk.evaluate_action(
            tool_name=tool_name,
            arguments=normalized_args,
            changed_files=self._changed_files(),
        )
        if risk.decision != RiskDecisionKind.CONTINUE:
            state.status = TaskStatus.NEEDS_REVIEW
            state.risk_level = risk.risk_level
            state.blocked_reasons.extend(risk.reasons)
            self.evidence.risks.append(risk.to_dict())
            self._persist()
            self._record_event(
                action_id=tool_call_id,
                action_kind=self.policy.action_for_tool(tool_name).value,
                decision=risk.decision.value,
                reason="; ".join(risk.reasons),
            )
            return AuthorizationDecision(
                allowed=False,
                error_code="risk_escalated",
                reason="; ".join(risk.reasons),
                risk_decision=risk.decision,
            )

        action = AgentAction(
            kind=self.policy.action_for_tool(tool_name),
            intent=f"Use {tool_name}",
            phase_id=phase.phase_id,
            preconditions=phase.entry_conditions,
            allowed_tools=[tool_name],
            expected_evidence=self.policy.expected_evidence(tool_name),
            risk_level=risk.risk_level,
            status=ActionStatus.ALLOWED,
        )
        self.actions[action.action_id] = action
        self._record_event(
            action=action,
            decision="allow",
            reason=f"{tool_name} is allowed in phase {phase.phase_id}.",
        )
        return AuthorizationDecision(allowed=True, action=action)

    def update_from_tool_result(
        self,
        *,
        tool_call_id: str | None,
        tool_name: str,
        result: ToolResult | dict[str, Any],
        action_id: str | None = None,
    ) -> None:
        self._throw_if_cancelled()
        state = self._state()
        BudgetController(self.budget).record_tool_call()
        payload = result.model_dump(mode="json") if isinstance(result, ToolResult) else result
        content = payload.get("content")
        metadata = self._dict_payload(payload.get("metadata"))
        error = self._dict_payload(payload.get("error"))
        failure = None
        if payload.get("ok") is False:
            failure = {
                "code": payload.get("error_code"),
                "message": error.get("message"),
                "details": error.get("details"),
                "backend": metadata.get("backend"),
                "policy_decision_id": metadata.get("policy_decision_id"),
                "approval_grant_id": metadata.get("approval_grant_id"),
            }
        tool_result_entry = {
            "tool_call_id": tool_call_id,
            "tool_name": tool_name,
            "action_id": action_id,
            "ok": payload.get("ok"),
            "status": "ok" if payload.get("ok") else "failed",
            "error_code": payload.get("error_code"),
            "failure": failure,
        }
        self.evidence.add_tool_result(tool_result_entry)
        if payload.get("ok") is False and payload.get("error_code"):
            self.replan(
                {
                    "error_code": payload.get("error_code"),
                    "tool_name": tool_name,
                    "tool_call_id": tool_call_id,
                    "failure": failure,
                }
            )
        if tool_name == "read_file" and isinstance(content, dict):
            self.evidence.add_unique_file(str(content.get("path") or ""))
        elif tool_name == "list_files" and isinstance(content, dict):
            root = content.get("root")
            if root:
                self.evidence.add_unique_file(str(root))
        elif tool_name == "search_text" and isinstance(content, dict):
            self.evidence.search_results.extend(content.get("matches") or [])
        elif tool_name.startswith("index_") and isinstance(content, dict):
            project_index = content.get("project_index")
            observation = project_index if isinstance(project_index, dict) else content
            self.record_project_index_observation(observation)
        elif tool_name in DIFF_TOOLS and isinstance(content, dict):
            self.record_diff_observation(content, tool_call_id=tool_call_id)
        elif tool_name in MUTATION_TOOLS and isinstance(content, dict):
            self.update_from_mutation(content, tool_call_id=tool_call_id)
        elif tool_name.startswith("edit_") and isinstance(content, dict):
            self.update_from_edit(content, tool_call_id=tool_call_id)
        elif tool_name.startswith("review_") and isinstance(content, dict):
            self.record_review_observation(content)
        elif tool_name.startswith("workspace_") and isinstance(content, dict):
            self.update_from_mutation(content, tool_call_id=tool_call_id)
        elif tool_name in {"run_command", "start_process", "stop_process"} and isinstance(content, dict):
            self.update_from_command(content, tool_call_id=tool_call_id)
        elif "verification" in tool_name and isinstance(content, dict):
            self.update_from_verification(content, tool_call_id=tool_call_id)

        if action_id and action_id in self.actions:
            self.actions[action_id].status = ActionStatus.SUCCEEDED if payload.get("ok") else ActionStatus.FAILED
            self.actions[action_id].result_ref = tool_call_id
        self._maybe_advance_after_tool(tool_name)
        state.touch()
        self._persist()
        self._record_event(
            action_id=action_id,
            action_kind=self.actions[action_id].kind.value if action_id in self.actions else None,
            decision="tool_result",
            reason=f"Recorded result for {tool_name}.",
            evidence_refs=[tool_call_id] if tool_call_id else [],
        )

    def update_from_mutation(
        self, result: Any, *, tool_call_id: str | None = None
    ) -> None:
        self._throw_if_cancelled()
        payload = self._content_payload(result)
        if payload.get("mutation_status") not in {None, "preview"}:
            changed_files = list(payload.get("changed_files") or [])
            BudgetController(self.budget).record_mutation(changed_files=len(changed_files))
            transaction_id = payload.get("transaction_id")
            if transaction_id and any(
                change.get("transaction_id") == transaction_id
                for change in self.evidence.applied_changes
            ):
                return
            entry = {
                "tool_call_id": tool_call_id,
                "transaction_id": transaction_id,
                "changeset_id": payload.get("changeset_id"),
                "changed_files": changed_files,
                "status": payload.get("mutation_status"),
                "artifact_path": payload.get("artifact_path"),
                "diff_summary": payload.get("diff_summary") or [],
                "diff_digest": payload.get("diff_digest"),
                "artifact_refs": payload.get("artifact_refs") or [],
                "warnings": payload.get("warnings") or [],
            }
            self.evidence.applied_changes.append(entry)
            self._append_unique(self._state().linked_transactions, transaction_id)
            if self._mutation_contract_ready_for_verification():
                self._state().status = TaskStatus.RUNNING_VERIFICATION
                self._state().current_phase = "running_verification"
                self._plan().current_phase = "running_verification"
        if payload.get("error_code"):
            self.replan({"error_code": payload.get("error_code")})
        self._persist()

    def update_from_command(
        self, result: Any, *, tool_call_id: str | None = None
    ) -> None:
        self._throw_if_cancelled()
        payload = self._content_payload(result)
        command = payload.get("command_result") if isinstance(payload.get("command_result"), dict) else payload
        if isinstance(command, dict):
            command_id = command.get("command_id")
            if command_id and any(
                existing.get("command_id") == command_id
                for existing in self.evidence.command_results
            ):
                return
            BudgetController(self.budget).record_command()
            command = {**command, "tool_call_id": tool_call_id}
            self.evidence.command_results.append(command)
            self._record_sandbox_from_command(command, source="command")
            self._append_unique(self._state().linked_commands, command_id)
            for failure in command.get("parsed_failures") or []:
                self.evidence.parsed_failures.append(failure)
            if command.get("semantic_status") not in {None, "succeeded", "SUCCEEDED"}:
                self.evidence.unresolved_failures.append(command)
        self._persist()

    def update_from_verification(
        self, result: Any, *, tool_call_id: str | None = None
    ) -> None:
        self._throw_if_cancelled()
        payload = self._content_payload(result)
        verification = payload.get("verification") if isinstance(payload.get("verification"), dict) else payload
        if not isinstance(verification, dict):
            return
        plan = verification.get("plan")
        plan_payload = plan if isinstance(plan, dict) else {}
        plan_id = plan_payload.get("verification_plan_id")
        if plan_id and any(
            (existing.get("plan") or {}).get("verification_plan_id") == plan_id
            for existing in self.evidence.verification_results
        ):
            return
        verification = {**verification, "tool_call_id": tool_call_id}
        self.evidence.add_verification_result(verification)
        failure_analyses: list[dict[str, Any]] = []
        for analysis in verification.get("failure_analysis") or []:
            if isinstance(analysis, dict):
                failure_analyses.append(analysis)
                analysis_id = analysis.get("analysis_id")
                if not analysis_id or not any(
                    existing.get("analysis_id") == analysis_id
                    for existing in self.evidence.failure_analyses
                ):
                    self.evidence.failure_analyses.append(analysis)
                if self.state and self.state.task_contract:
                    semantic_plan = self.producers.semantic_plan.produce_repair(
                        analysis,
                        task_contract=TaskContract.from_dict(self.state.task_contract),
                        context_payload=self._producer_context(),
                    )
                    self.state.rolling_plan = semantic_plan.rolling_plan.to_dict()
                    self.state.risk_points = [
                        rp.to_dict() for rp in semantic_plan.risk_points
                    ]
                    self.state.verification_strategies = [
                        vs.to_dict() for vs in semantic_plan.verification_strategies
                    ]
                    if semantic_plan.repair_policy:
                        self.state.repair_policy = (
                            semantic_plan.repair_policy.to_dict()
                        )
        repair_plan = verification.get("repair_plan")
        if isinstance(repair_plan, dict):
            plan_id = repair_plan.get("plan_id")
            if not plan_id or not any(
                existing.get("plan_id") == plan_id for existing in self.evidence.repair_plans
            ):
                self.evidence.repair_plans.append(repair_plan)
        self._record_sandbox_from_verification(verification)
        for status in verification.get("check_status") or []:
            self._append_unique(self._state().linked_verifications, status.get("check_id"))
            if status.get("status") in {"failed", "blocked", "timeout", "flaky"}:
                self.evidence.unresolved_failures.append(status)
        assessment = verification.get("completion_assessment") or {}
        self._state().final_assessment = assessment
        if assessment.get("status") in {"ready", "ready_with_warnings"}:
            self.evidence.unresolved_failures.clear()
            self._state().status = TaskStatus.FINALIZING
            self._state().current_phase = "finalizing"
            self._plan().current_phase = "finalizing"
        elif assessment.get("status") in {"blocked", "failed"}:
            self._state().status = TaskStatus.REPAIRING_FAILURES
            self._state().current_phase = "repairing_failures"
            self._plan().current_phase = "repairing_failures"
        if failure_analyses:
            self._record_dynamic_retrieval(
                trigger="verification_failure",
                failure_analysis=failure_analyses[-1],
                changed_files=self._changed_files(),
            )
        if isinstance(verification.get("review_report"), dict):
            self.record_review_observation(verification["review_report"])
        self._persist()

    def _record_sandbox_from_command(self, command: dict[str, Any], *, source: str) -> None:
        sandbox = ((command.get("isolation_report") or {}).get("sandbox") or {})
        if not isinstance(sandbox, dict) or not sandbox.get("sandbox_id"):
            return
        payload = {
            "source": source,
            "command_id": command.get("command_id"),
            "sandbox_id": sandbox.get("sandbox_id"),
            "backend": sandbox.get("backend"),
            "status": sandbox.get("status"),
            "trace_id": sandbox.get("trace_id"),
            "enforcement_status": sandbox.get("enforcement_status"),
            "execution_backend": sandbox.get("execution_backend"),
            "network_denied_verified": sandbox.get("network_denied_verified"),
            "process_tree_kill": sandbox.get("process_tree_kill"),
            "job_killed": sandbox.get("job_killed"),
            "timeout_enforced": sandbox.get("timeout_enforced"),
            "artifact_count": sandbox.get("artifact_count", 0),
            "artifacts": sandbox.get("artifacts") or [],
            "artifact_refs": sandbox.get("artifact_refs") or [],
            "changed_files_count": sandbox.get("changed_files_count", 0),
            "changed_files": sandbox.get("changed_files") or {},
            "violations": sandbox.get("violations") or [],
            "imported_changes_count": sandbox.get("imported_changes_count", 0),
            "summary": self._sandbox_summary(command, sandbox),
        }
        self.evidence.add_sandbox_observation(payload)

    def _record_sandbox_from_verification(self, verification: dict[str, Any]) -> None:
        for result in verification.get("results") or []:
            evidence = result.get("evidence") or {}
            sandbox_id = evidence.get("sandbox_id")
            if not sandbox_id:
                continue
            payload = {
                "source": "verification",
                "check_id": result.get("check_id"),
                "kind": result.get("kind"),
                "command_id": evidence.get("command_id"),
                "sandbox_id": sandbox_id,
                "backend": evidence.get("sandbox_backend"),
                "status": evidence.get("sandbox_status"),
                "enforcement_status": evidence.get("enforcement_status"),
                "execution_backend": evidence.get("execution_backend"),
                "network_denied_verified": evidence.get("network_denied_verified"),
                "process_tree_kill": evidence.get("process_tree_kill"),
                "job_killed": evidence.get("job_killed"),
                "timeout_enforced": evidence.get("timeout_enforced"),
                "artifact_count": len(evidence.get("sandbox_artifacts") or []),
                "artifacts": evidence.get("sandbox_artifacts") or [],
                "artifact_refs": [
                    artifact.get("artifact_id")
                    for artifact in (evidence.get("sandbox_artifacts") or [])
                    if isinstance(artifact, dict) and artifact.get("artifact_id")
                ],
                "changed_files_count": (evidence.get("sandbox_changed_files") or {}).get("total_changed_files", 0),
                "changed_files": evidence.get("sandbox_changed_files") or {},
                "violations": evidence.get("sandbox_violations") or [],
                "imported_changes_count": 0,
                "summary": self._sandbox_summary(
                    {
                        "exit_code": evidence.get("exit_code"),
                        "command_id": evidence.get("command_id"),
                    },
                    {
                        "sandbox_id": sandbox_id,
                        "backend": evidence.get("sandbox_backend"),
                        "status": evidence.get("sandbox_status"),
                    },
                    prefix=f"{result.get('kind') or 'verification'} ran",
                ),
            }
            self.evidence.add_sandbox_observation(payload)

    @staticmethod
    def _sandbox_summary(
        command: dict[str, Any],
        sandbox: dict[str, Any],
        *,
        prefix: str = "command ran",
    ) -> str:
        status = sandbox.get("status")
        backend = sandbox.get("backend")
        exit_code = command.get("exit_code")
        if status == "backend_unavailable":
            return "[sandbox] command blocked: backend cannot enforce required isolation."
        return f"[sandbox] {prefix} under native OS sandbox enforcement via {backend}, exit_code={exit_code}."

    def record_sandbox_capability(self, snapshot: dict[str, Any]) -> None:
        self._throw_if_cancelled()
        state = self._state()
        state.sandbox_capability = dict(snapshot)
        self._persist()
        self._record_event(
            decision="sandbox_capability",
            reason="Sandbox capability snapshot recorded in task state.",
            extra={"sandbox_capability": state.sandbox_capability},
        )

    def record_policy_observation(self, observation: dict[str, Any]) -> None:
        self._throw_if_cancelled()
        state = self._state()
        payload = {
            "outcome": observation.get("outcome"),
            "component": observation.get("component"),
            "operation": observation.get("operation"),
            "reason": observation.get("reason"),
            "risk_level": observation.get("risk_level"),
            "resource": observation.get("resource"),
        }
        self.evidence.add_policy_observation(payload)
        if payload["outcome"] in {"deny", "require_review", "escalate"}:
            self.evidence.unresolved_failures.append({"policy": payload})
            state.status = TaskStatus.NEEDS_REVIEW
            if payload["reason"]:
                self._append_unique(state.blocked_reasons, payload["reason"])
        self._persist()
        self._record_event(
            decision="policy_observation",
            reason=str(payload.get("reason") or "Policy observation recorded."),
            evidence_refs=[],
            extra={"policy_observation": payload},
        )

    def record_instruction_prompt_observation(self, observation: dict[str, Any]) -> None:
        self._throw_if_cancelled()
        prompt_hash_references = self._string_list(observation.get("prompt_hash_references"))
        payload: dict[str, Any] = {
            "prompt_bundles_compiled_count": int(observation.get("prompt_bundles_compiled_count") or 0),
            "project_instruction_files_loaded_count": int(observation.get("project_instruction_files_loaded_count") or 0),
            "injection_warning_count": int(observation.get("injection_warning_count") or 0),
            "conflict_count": int(observation.get("conflict_count") or 0),
            "developer_message_folded_count": int(observation.get("developer_message_folded_count") or 0),
            "prompt_budget_exceeded_count": int(observation.get("prompt_budget_exceeded_count") or 0),
            "untrusted_context_sections_count": int(observation.get("untrusted_context_sections_count") or 0),
            "prompt_hash_references": prompt_hash_references,
        }
        self.evidence.instruction_prompt_observations = [payload]
        self._persist()
        self._record_event(
            decision="instruction_prompt_observation",
            reason="Instruction prompt observation recorded.",
            evidence_refs=prompt_hash_references,
            extra={"instruction_prompt_observation": payload},
        )

    def record_execution_outcome(self, outcome: ExecutionOutcome | dict[str, Any]) -> None:
        self._throw_if_cancelled()
        payload = outcome.to_dict() if hasattr(outcome, "to_dict") else dict(outcome)
        self.evidence.add_task_outcome(payload)
        for item in payload.get("missing_evidence") or []:
            self._append_unique(self.evidence.missing_evidence, item)
        status = str(payload.get("status") or "")
        if status in {"fatal", "blocked"}:
            self.evidence.unresolved_failures.append({"execution_outcome": payload})
        state = self._state()
        if status == "replan_required":
            self._route_after_missing_evidence(payload.get("missing_evidence") or [])
            self.replan({"error_code": payload.get("error_code") or "task_outcome_replan"})
            return
        if status == "retryable" and state.status == TaskStatus.BLOCKED:
            state.status = TaskStatus.RECOVERING
            state.current_phase = self._plan().current_phase
        elif status == "approval_required":
            state.status = TaskStatus.NEEDS_REVIEW
        elif status == "user_input_required":
            state.status = TaskStatus.BLOCKED
        elif status in {"fatal", "blocked"}:
            state.status = TaskStatus.FAILED if status == "fatal" else TaskStatus.BLOCKED
        state.touch()
        self._persist()
        self._record_event(
            decision="execution_outcome",
            reason=str(payload.get("reason") or "Execution outcome recorded."),
            extra={"execution_outcome": payload},
        )

    def record_failure_analysis(
        self,
        analysis: Any,
        repair_plan: Any,
        *,
        replan_signal: Any | None = None,
    ) -> None:
        self._throw_if_cancelled()
        analysis_payload = analysis.to_dict() if hasattr(analysis, "to_dict") else dict(analysis)
        repair_payload = repair_plan.to_dict() if hasattr(repair_plan, "to_dict") else dict(repair_plan)
        replan_payload = _dict_like(replan_signal)
        repair_contract = _repair_contract_payload(repair_payload, replan_payload)
        if repair_contract and not repair_payload.get("repair_contract"):
            repair_payload["repair_contract"] = repair_contract
        analysis_id = analysis_payload.get("analysis_id")
        if not analysis_id or not any(
            existing.get("analysis_id") == analysis_id
            for existing in self.evidence.failure_analyses
        ):
            self.evidence.failure_analyses.append(analysis_payload)
        plan_id = repair_payload.get("plan_id")
        if not plan_id or not any(
            existing.get("plan_id") == plan_id for existing in self.evidence.repair_plans
        ):
            self.evidence.repair_plans.append(repair_payload)

        state = self._state()
        plan = self._plan()
        blocked_reason = (
            repair_payload.get("blocked_reason")
            or (repair_contract or {}).get("blocked_reason")
        )
        if (
            repair_payload.get("needs_user_input")
            or blocked_reason
            or (repair_contract or {}).get("needs_user_input")
        ):
            state.status = TaskStatus.BLOCKED
            if blocked_reason:
                self._append_unique(state.blocked_reasons, blocked_reason)
            for action in repair_payload.get("next_actions") or []:
                self._append_unique(state.open_questions, action)
        else:
            state.status = TaskStatus.REPAIRING_FAILURES
            state.current_phase = "repairing_failures"
            plan.current_phase = "repairing_failures"
            if state.task_contract:
                semantic_plan = self.producers.semantic_plan.produce_repair(
                    repair_contract or repair_payload or analysis_payload,
                    task_contract=TaskContract.from_dict(state.task_contract),
                    context_payload=self._producer_context(),
                )
                state.rolling_plan = semantic_plan.rolling_plan.to_dict()
                state.risk_points = [
                    rp.to_dict() for rp in semantic_plan.risk_points
                ]
                state.verification_strategies = [
                    vs.to_dict() for vs in semantic_plan.verification_strategies
                ]
                if semantic_plan.repair_policy:
                    state.repair_policy = semantic_plan.repair_policy.to_dict()
            self._record_dynamic_retrieval(
                trigger="failure_analysis",
                failure_analysis=analysis_payload,
                changed_files=self._changed_files(),
            )
        self._persist()
        root_cause = analysis_payload.get("root_cause")
        root_cause_reason = (
            root_cause.get("description")
            if isinstance(root_cause, dict)
            else analysis_payload.get("root_cause_text")
        )
        self._record_event(
            decision="failure_analysis",
            reason=str(root_cause_reason or "Failure analysis recorded."),
            evidence_refs=list(analysis_payload.get("evidence_refs") or []),
            replan_decision=replan_payload,
            extra={
                "failure_analysis": analysis_payload,
                "repair_plan": repair_payload,
                "repair_contract": repair_contract,
            },
        )

    def _route_after_missing_evidence(self, missing: list[Any]) -> None:
        names = {str(item) for item in missing}
        state = self._state()
        plan = self._plan()
        if "required_files_inspected" in names:
            state.status = TaskStatus.INSPECTING_WORKSPACE
            state.current_phase = "inspecting_workspace"
            plan.current_phase = "inspecting_workspace"
        elif "required_changes_applied" in names:
            state.status = TaskStatus.APPLYING_CHANGES
            state.current_phase = "applying_changes"
            plan.current_phase = "applying_changes"
        elif "unresolved_failures_empty" in names:
            state.status = TaskStatus.REPAIRING_FAILURES
            state.current_phase = "repairing_failures"
            plan.current_phase = "repairing_failures"
        elif "required_verifications_passed" in names:
            state.status = TaskStatus.RUNNING_VERIFICATION
            state.current_phase = "running_verification"
            plan.current_phase = "running_verification"

    def record_project_index_observation(self, observation: dict[str, Any]) -> None:
        self._throw_if_cancelled()
        relevant_files = self._dict_list(observation.get("relevant_files"))[:20]
        payload: dict[str, Any] = {
            "index_id": observation.get("index_id"),
            "summary": observation.get("summary") or {},
            "relevant_files": relevant_files,
            "context_candidates": list(observation.get("context_candidates") or [])[:20],
            "impact": observation.get("impact"),
            "test_impact": observation.get("test_impact"),
            "warnings": list(observation.get("warnings") or []),
            "trust_level": observation.get("trust_level") or "untrusted_workspace_data",
            "truncated": bool(observation.get("truncated")),
        }
        self.evidence.project_index_observations.append(payload)
        for candidate in relevant_files:
            path = candidate.get("path")
            if path:
                self.evidence.relevant_symbols.append(
                    {
                        "source": "project_index",
                        "path": path,
                        "score": candidate.get("score"),
                        "reasons": candidate.get("reasons") or [],
                        "freshness": candidate.get("freshness"),
                    }
                )
        self._persist()

    def record_diff_observation(
        self,
        observation: dict[str, Any],
        *,
        tool_call_id: str | None = None,
    ) -> None:
        self._throw_if_cancelled()
        changed_files = self._string_list(observation.get("changed_files"))
        payload: dict[str, Any] = {
            "tool_call_id": tool_call_id,
            "scope": observation.get("scope") or "current_run",
            "changeset_id": observation.get("changeset_id"),
            "changed_files": changed_files,
            "added_files": list(observation.get("added_files") or []),
            "modified_files": list(observation.get("modified_files") or []),
            "deleted_files": list(observation.get("deleted_files") or []),
            "diff_digest": observation.get("diff_digest"),
            "artifact_refs": list(observation.get("artifact_refs") or []),
            "warnings": list(observation.get("warnings") or []),
        }
        if payload not in self.evidence.diff_observations:
            self.evidence.diff_observations.append(payload)
        if changed_files and self.state is not None:
            self._record_dynamic_retrieval(
                trigger="diff_observation",
                changed_files=changed_files,
            )
        self._persist()

    def update_from_edit(
        self, result: Any, *, tool_call_id: str | None = None
    ) -> None:
        self._throw_if_cancelled()
        payload = self._content_payload(result)
        edit = payload.get("edit") if isinstance(payload.get("edit"), dict) else payload
        if not isinstance(edit, dict):
            return
        plan_id = edit.get("edit_plan_id")
        plan_entry = {
            "tool_call_id": tool_call_id,
            "edit_plan_id": plan_id,
            "intent_id": edit.get("intent_id"),
            "strategy": edit.get("strategy"),
            "patch_digest": edit.get("patch_digest"),
            "changed_files": list(edit.get("changed_files") or []),
            "status": edit.get("status"),
        }
        if plan_id and not any(existing.get("edit_plan_id") == plan_id for existing in self.evidence.edit_plans):
            self.evidence.edit_plans.append(plan_entry)
        result_id = edit.get("edit_result_id")
        if result_id and not any(existing.get("edit_result_id") == result_id for existing in self.evidence.edit_results):
            self.evidence.edit_results.append(
                {
                    **plan_entry,
                    "edit_result_id": result_id,
                    "ok": edit.get("ok"),
                    "error_code": edit.get("error_code"),
                    "validation": edit.get("validation"),
                    "repair_attempts": edit.get("repair_attempts") or [],
                    "changeset_id": edit.get("changeset_id"),
                    "transaction_id": edit.get("transaction_id"),
                    "verification_plan_id": (edit.get("verification_plan") or {}).get("id")
                    or (edit.get("verification_plan") or {}).get("verification_plan_id"),
                    "review_report_id": (edit.get("review_report") or {}).get("review_id")
                    if isinstance(edit.get("review_report"), dict)
                    else None,
                }
            )
        if edit.get("transaction_id") or edit.get("mutation_status"):
            self.update_from_mutation(edit, tool_call_id=tool_call_id)
        if edit.get("error_code"):
            self.replan({"error_code": edit.get("error_code")})
        if isinstance(edit.get("review_report"), dict):
            self.record_review_observation(edit["review_report"])
        self._persist()
        self._record_event(
            decision="edit_observation",
            reason="Edit observation recorded.",
            evidence_refs=[str(result_id)] if result_id else [],
            extra={"edit_observation": plan_entry},
        )

    def record_review_observation(self, observation: dict[str, Any]) -> None:
        self._throw_if_cancelled()
        if not isinstance(observation, dict):
            return
        decision = self._dict_payload(observation.get("decision"))
        target = self._dict_payload(observation.get("target"))
        findings = self._dict_list(observation.get("findings"))
        payload: dict[str, Any] = {
            "review_id": observation.get("review_id"),
            "target": target,
            "decision": decision,
            "findings": findings[:50],
            "next_actions": list(observation.get("next_actions") or []),
            "model_critic_status": observation.get("model_critic_status"),
            "input_summary": observation.get("input_summary"),
            "created_at": observation.get("created_at"),
        }
        review_id = payload.get("review_id")
        if review_id and any(existing.get("review_id") == review_id for existing in self.evidence.review_results):
            return
        self.evidence.review_results.append(payload)
        action = str(decision.get("action") or "")
        blocking = [item for item in findings if isinstance(item, dict) and item.get("blocking")]
        if action == "repair":
            self._state().status = TaskStatus.REPAIRING_FAILURES
            self._state().current_phase = "repairing_failures"
            self._plan().current_phase = "repairing_failures"
            if blocking:
                self.evidence.unresolved_failures.extend({"review": item} for item in blocking)
        elif action == "replan":
            signal = decision.get("replan_signal") if isinstance(decision.get("replan_signal"), dict) else {}
            self.replan(signal or {"error_code": "review_replan"})
        elif action == "needs_human_approval":
            self._state().status = TaskStatus.NEEDS_REVIEW
            for reason in decision.get("reasons") or []:
                self._append_unique(self._state().blocked_reasons, reason)
            if blocking:
                self.evidence.unresolved_failures.extend({"review": item} for item in blocking)
        elif action == "rollback":
            self._state().status = TaskStatus.NEEDS_REVIEW
            self._append_unique(self._state().blocked_reasons, "review requested rollback")
            if blocking:
                self.evidence.unresolved_failures.extend({"review": item} for item in blocking)
        self._persist()
        self._record_event(
            decision="review_observation",
            reason=f"Review decision recorded: {action or 'unknown'}.",
            evidence_refs=[str(review_id)] if review_id else [],
            extra={"review_observation": payload},
        )

    def replan(self, signal: Any) -> ReplanDecision:
        self._throw_if_cancelled()
        signal_payload = _dict_like(signal)
        state = self._state()
        repair_contract = _repair_contract_payload(signal_payload)
        fingerprint = signal_payload.get("failure_fingerprint")
        if fingerprint:
            count = BudgetController(self.budget).record_failure(str(fingerprint))
            if count >= self.budget.max_repeated_failures:
                reason = "repeated_failure"
                signal_payload = {**signal_payload, "blocked_reason": reason}
                decision = self.replanner.decide(signal_payload)
                if decision.decision == ReplanDecisionKind.CONTINUE:
                    decision = ReplanDecision(
                        decision=ReplanDecisionKind.ASK_USER,
                        reason="Repeated failure budget exceeded.",
                        next_action=ActionKind.ASK_USER,
                    )
                state.status = TaskStatus.BLOCKED
                self._append_unique(state.blocked_reasons, reason)
                self._persist()
                self._record_event(
                    decision="replan",
                    reason=decision.reason,
                    replan_decision=decision.to_dict(),
                    extra={
                        "replan_signal": signal_payload,
                        "repair_contract": repair_contract,
                    },
                )
                self._record_repair_signal_consumed(signal_payload, decision)
                return decision
        rule_decision = self.replanner.decide(signal_payload)
        if rule_decision.decision != ReplanDecisionKind.CONTINUE:
            decision = rule_decision
            if decision.decision == ReplanDecisionKind.ASK_USER:
                state.status = TaskStatus.BLOCKED
                self._append_unique(state.blocked_reasons, decision.reason)
            elif decision.decision == ReplanDecisionKind.READ_FRESH_FILE:
                state.status = TaskStatus.INSPECTING_WORKSPACE
                state.current_phase = "inspecting_workspace"
                self._plan().current_phase = "inspecting_workspace"
            elif decision.decision == ReplanDecisionKind.REPAIR_FAILURE:
                state.status = TaskStatus.REPAIRING_FAILURES
                state.current_phase = "repairing_failures"
                self._plan().current_phase = "repairing_failures"
                self._consume_repair_signal(signal_payload)
            elif decision.decision == ReplanDecisionKind.REQUIRE_REVIEW:
                state.status = TaskStatus.NEEDS_REVIEW
            self._persist()
            self._record_event(
                decision="replan",
                reason=decision.reason,
                replan_decision=decision.to_dict(),
                extra={
                    "replan_signal": signal_payload,
                    "repair_contract": repair_contract,
                },
            )
            self._record_repair_signal_consumed(signal_payload, decision)
            return decision
        planner_decision = self.producers.planner_decision.produce(
            signal_payload,
            context_payload=self._producer_context(),
            risk_points=[
                RiskPoint.from_dict(rp) for rp in state.risk_points
            ],
            verification_strategies=[
                VerificationStrategy.from_dict(vs)
                for vs in state.verification_strategies
            ],
            repair_policy=(
                RepairPolicy.from_dict(state.repair_policy)
                if state.repair_policy
                else None
            ),
        )
        decision = ReplanDecision(
            decision=planner_decision.decision,
            reason=planner_decision.reason,
            next_action=planner_decision.next_action,
        )
        if decision.decision == ReplanDecisionKind.READ_FRESH_FILE:
            state.status = TaskStatus.INSPECTING_WORKSPACE
            state.current_phase = "inspecting_workspace"
            self._plan().current_phase = "inspecting_workspace"
        elif decision.decision == ReplanDecisionKind.REPAIR_FAILURE:
            state.status = TaskStatus.REPAIRING_FAILURES
            state.current_phase = "repairing_failures"
            self._plan().current_phase = "repairing_failures"
            self._consume_repair_signal(signal_payload)
        elif decision.decision == ReplanDecisionKind.REQUIRE_REVIEW:
            state.status = TaskStatus.NEEDS_REVIEW
        self._persist()
        self._record_event(
            decision="replan",
            reason=decision.reason,
            replan_decision=decision.to_dict(),
            extra={
                "replan_signal": signal_payload,
                "repair_contract": repair_contract,
            },
        )
        self._record_repair_signal_consumed(signal_payload, decision)
        return decision

    def assess_completion(self, *, mark_blocked: bool = True) -> dict[str, Any]:
        self._throw_if_cancelled()
        state = self._state()
        unmet: list[str] = []
        if state.completion_criteria.required_files_inspected and not self.evidence.inspected_files:
            unmet.append("required_files_inspected")
        if state.completion_criteria.required_changes_applied and not self.evidence.applied_changes:
            unmet.append("required_changes_applied")
        verification_status = state.final_assessment.get("status")
        if (
            state.completion_criteria.required_verifications_passed
            and verification_status not in {"ready", "ready_with_warnings"}
        ):
            unmet.append("required_verifications_passed")
        if state.completion_criteria.unresolved_failures_empty and self.evidence.unresolved_failures:
            unmet.append("unresolved_failures_empty")
        if self.evidence.external_changes:
            unmet.append("workspace_health_acceptable")
        if self.evidence.risks and state.risk_level != RiskLevel.LOW:
            unmet.append("risks_acknowledged")
        criteria = self._contract_criterion_status()
        for criterion_id, criterion in criteria.items():
            if criterion["required"] and not criterion["satisfied"]:
                unmet.append(f"contract:{criterion_id}")
        missing_benchmark_changes = self._missing_benchmark_expected_file_changes()
        if missing_benchmark_changes:
            unmet.append("benchmark_expected_file_changes")
        satisfaction = self.assess_verification_contract_satisfaction()
        if not satisfaction.satisfied:
            unmet.append("verification_contract_satisfaction")
        status = TaskStatus.COMPLETED if not unmet else TaskStatus.BLOCKED
        assessment = {
            "status": status.value,
            "unmet": unmet,
            "criteria": criteria,
            "verification_contract_satisfaction": satisfaction.to_dict(),
        }
        if unmet and mark_blocked:
            state.status = TaskStatus.BLOCKED
            state.blocked_reasons = sorted(set([*state.blocked_reasons, *unmet]))
        self._persist()
        self._record_event(
            decision="assess_completion",
            reason="Completion criteria assessed.",
            completion_assessment=assessment,
        )
        return assessment

    def _contract(self) -> TaskContract | None:
        state = self._state()
        if not state.task_contract:
            return None
        return TaskContract.from_dict(state.task_contract)

    def _contract_criterion_status(self) -> dict[str, dict[str, Any]]:
        contract = self._contract()
        if contract is None:
            return {}
        status: dict[str, dict[str, Any]] = {}
        for criterion in contract.acceptance_criteria:
            satisfied = all(self._contract_evidence_satisfied(item) for item in criterion.evidence)
            status[criterion.criterion_id] = {
                "description": criterion.description,
                "required": criterion.required,
                "evidence": criterion.evidence,
                "satisfied": satisfied,
                "missing_evidence": [
                    item for item in criterion.evidence if not self._contract_evidence_satisfied(item)
                ],
            }
        return status

    def _contract_evidence_satisfied(self, evidence_key: str) -> bool:
        if evidence_key == "task_contract":
            return bool(self._state().task_contract)
        if evidence_key == "inspected_files":
            return bool(self.evidence.inspected_files)
        if evidence_key == "applied_changes":
            return bool(self.evidence.applied_changes)
        if evidence_key == "verification_results":
            return self._state().final_assessment.get("status") in {"ready", "ready_with_warnings"}
        if evidence_key == "final_report_ready":
            return bool(self._state().completion_criteria.final_report_ready)
        value = getattr(self.evidence, evidence_key, None)
        return bool(value)

    def finalize(self) -> FinalReport:
        self._throw_if_cancelled()
        trace_summary = (
            self.trace.final_report_summary(task_id=self.task_id)
            if self.trace is not None and hasattr(self.trace, "final_report_summary")
            else None
        )
        final_review = self._run_final_review(trace_summary=trace_summary)
        contract_satisfaction = self.assess_verification_contract_satisfaction().to_dict()
        completion_assessment = self._run_final_reviewer_assessment()
        if not completion_assessment.overall_satisfied:
            self._state().status = TaskStatus.BLOCKED
            self._state().blocked_reasons = sorted(
                set([*self._state().blocked_reasons, *completion_assessment.blocking_reasons])
            )
            self._persist()
            self._record_event(
                decision="finalize",
                reason="Final reviewer blocked completion: "
                + "; ".join(completion_assessment.blocking_reasons),
                completion_assessment=completion_assessment.to_dict(),
            )
            report = self.finalizer.build(
                state=self._state(),
                evidence=self.evidence,
                trace_summary=trace_summary,
                contract_satisfaction=contract_satisfaction,
            )
            return report
        report = self.finalizer.build(
            state=self._state(),
            evidence=self.evidence,
            trace_summary=trace_summary,
            contract_satisfaction=contract_satisfaction,
        )
        output_dir = self.store.session_dir(self.session_id)
        artifact_ref = (output_dir / "final_report.md").relative_to(self.workspace_root).as_posix()
        if artifact_ref not in report.artifacts:
            report.artifacts.append(artifact_ref)
            report.artifacts.sort()
        self.final_report_renderer.write_markdown(
            report=report,
            state=self._state(),
            evidence=self.evidence,
            output_dir=output_dir,
        )
        self.final_report = report
        lesson_candidates = self.extract_lessons(report)
        if report.status == TaskStatus.COMPLETED:
            self._state().status = TaskStatus.COMPLETED
            self._state().completion_criteria.final_report_ready = True
            self._clear_resolved_completion_blockers(report)
        else:
            self._state().status = report.status
        self._state().touch()
        self._persist()
        self._record_event(
            decision="finalize",
            reason="Final report generated from ledger evidence.",
            evidence_refs=[artifact_ref],
            completion_assessment=report.verification_summary,
            extra={
                "final_report_artifact": artifact_ref,
                "final_review_route": final_review.decision.route,
                "lesson_candidates": len(lesson_candidates),
            },
        )
        if self.trace is not None:
            self.trace.record(
                "final_report.completed",
                {
                    "task_id": self.task_id,
                    "session_id": self.session_id,
                    "status": report.status.value,
                    "artifact_path": artifact_ref,
                    "review_route": final_review.decision.route,
                },
            )
        return report

    def _clear_resolved_completion_blockers(self, report: FinalReport) -> None:
        state = self._state()
        if report.verification_summary.get("status") not in {"ready", "ready_with_warnings"}:
            return
        if report.review_summary.get("blocking_finding_count", 0):
            return
        state.blocked_reasons = [
            reason
            for reason in state.blocked_reasons
            if not self._completion_blocker_resolved_by_final_report(reason)
        ]

    @staticmethod
    def _completion_blocker_resolved_by_final_report(reason: str) -> bool:
        normalized = str(reason).strip().lower()
        if normalized in {
            "required_files_inspected",
            "required_changes_applied",
            "required_verifications_passed",
            "unresolved_failures_empty",
            "workspace_health_acceptable",
            "risks_acknowledged",
            "completion_criteria_unmet",
            "missing_required_evidence",
            "verification_contract_satisfaction",
        }:
            return True
        return "sandbox backend unavailable" in normalized

    def extract_lessons(self, final_report: Any | None = None, *, accept: bool = False) -> list[Any]:
        return self.lesson_extractor.extract(
            self.final_report if final_report is None else final_report,
            memory_pipeline=self.memory_pipeline,
            accept=accept,
        )

    def _run_final_review(self, *, trace_summary: dict[str, Any] | None) -> Any:
        review_pipeline = self.review_pipeline
        if review_pipeline is None:
            from singularity.review import ReviewPipeline

            review_pipeline = ReviewPipeline(
                self.workspace_root,
                trace=self.trace,
                planner=self,
                enable_model_critic=False,
            )
            self.review_pipeline = review_pipeline
        elif getattr(review_pipeline, "planner", None) is None:
            with suppress(Exception):
                review_pipeline.planner = self
        report = review_pipeline.final_review(
            task_state=self._state(),
            task_plan=self._plan(),
            evidence_ledger=self.evidence,
            trace_summary=trace_summary,
        )
        payload = report.model_dump(mode="json") if hasattr(report, "model_dump") else dict(report)
        review_id = payload.get("review_id")
        if not review_id or not any(item.get("review_id") == review_id for item in self.evidence.review_results):
            self.record_review_observation(payload)
        return report

    def _run_final_reviewer_assessment(self) -> Any:
        """Run the per-criterion FinalReviewer gate before finalize."""
        state = self._state()
        contract = self._contract()
        plan: Any = None
        if state.risk_points or state.verification_strategies or state.repair_policy:
            from singularity.planner.semantic_objects import SemanticPlan

            plan = SemanticPlan.from_dict(
                {
                    "rolling_plan": state.rolling_plan or {},
                    "risk_points": state.risk_points,
                    "verification_strategies": state.verification_strategies,
                    "repair_policy": state.repair_policy,
                    "producer_source": "rules",
                }
            )
        return self.final_reviewer.assess(
            contract=contract,
            plan=plan,
            evidence=self.evidence,
            state=state,
            context_payload=self._producer_context(),
        )

    def interrupt(self, reason: str = "interrupted") -> TaskState:
        state = self._state()
        state.status = TaskStatus.INTERRUPTED
        state.blocked_reasons.append(reason)
        state.touch()
        self._persist()
        self._record_event(decision="interrupt", reason=reason)
        return state

    def resume(
        self,
        session_id: str,
        *,
        workspace_health: dict[str, Any] | None = None,
    ) -> Planner:
        self._throw_if_cancelled()
        state, plan, evidence, budget, final_report = self.store.load(session_id)
        self.session_id = session_id
        self.task_id = state.task_id
        self.state = state
        self.plan = plan
        self.evidence = evidence
        self.budget = budget
        self.final_report = final_report
        if workspace_health and (
            workspace_health.get("status") == "conflicted"
            or workspace_health.get("external_changes")
        ):
            self.state.status = TaskStatus.NEEDS_REVIEW
            self.state.current_phase = "inspecting_workspace"
            self.plan.current_phase = "inspecting_workspace"
            self.state.blocked_reasons.append("workspace conflict on resume")
            self.evidence.external_changes.extend(workspace_health.get("external_changes") or [])
        elif self.state.status == TaskStatus.INTERRUPTED:
            self.state.status = TaskStatus.RECOVERING
        self._persist()
        self._record_event(decision="resume", reason="Planner state resumed.")
        return self

    def continue_with_instruction(self, instruction: str) -> TaskState:
        self._throw_if_cancelled()
        state = self._state()
        normalized = " ".join(instruction.split())
        revision = {
            "source": "session_continue",
            "instruction": instruction,
            "revised_goal": (
                f"{state.effective_goal or state.normalized_goal}\n"
                f"Additional instruction: {normalized}"
            ),
            "timestamp": _now(),
        }
        state.goal_revisions.append(revision)
        state.effective_goal = str(revision["revised_goal"])
        if state.status in {TaskStatus.COMPLETED, TaskStatus.INTERRUPTED}:
            state.status = TaskStatus.RECOVERING
        state.touch()
        self._persist()
        self._record_event(
            decision="session_continue",
            reason="User appended an instruction to this session.",
            extra={"goal_revision": revision},
        )
        return state

    def abort(self, reason: str = "aborted") -> TaskState:
        self._throw_if_cancelled()
        state = self._state()
        state.status = TaskStatus.FAILED
        state.blocked_reasons.append(reason)
        state.touch()
        self._persist()
        self._record_event(decision="abort", reason=reason)
        return state

    def planner_context_message(self) -> dict[str, str]:
        return {
            "role": "system",
            "content": self.renderer.render(
                state=self._state(),
                plan=self._plan(),
                evidence=self.evidence,
            ),
        }

    def decide_tool_exposure(
        self,
        available_tools: list[ToolSpec],
        *,
        policy_profile: str | None = None,
        sandbox_mode: str | None = None,
        workspace_state: dict[str, Any] | None = None,
    ) -> ToolExposureDecision:
        self._throw_if_cancelled()
        state = self._state()
        phase = self._plan().phase(state.current_phase)
        allowed = set(phase.allowed_tools)
        current_step = self.semantic_rolling_plan().current_step()
        if current_step is not None:
            allowed.update(current_step.allowed_capabilities)
        repair_execution_block = self._repair_contract_execution_block()
        if repair_execution_block:
            allowed &= self._repair_contract_evidence_tools()
        else:
            repair_allowed = self._active_repair_allowed_tools()
            if repair_allowed:
                allowed &= repair_allowed
        benchmark_allowed = self._benchmark_allowed_tools()
        if benchmark_allowed:
            allowed &= benchmark_allowed
        decision = self.tool_router.decide(
            phase=phase.phase_id,
            phase_allowed_tool_names=allowed,
            available_tools=available_tools,
            task_state=state,
            policy_profile=policy_profile,
            sandbox_mode=sandbox_mode or _sandbox_mode(state.sandbox_capability),
            active_user_constraints=self._active_user_constraints(),
            workspace_state=workspace_state,
        )
        if self.trace is not None:
            self.trace.record("tool.exposure_decided", decision.to_trace_data())
        return decision

    def filtered_tools(
        self,
        tools: list[dict[str, Any]],
        *,
        tool_specs: list[ToolSpec] | None = None,
        policy_profile: str | None = None,
        sandbox_mode: str | None = None,
        workspace_state: dict[str, Any] | None = None,
    ) -> list[dict[str, Any]]:
        self._throw_if_cancelled()
        if tool_specs is not None:
            allowed = set(
                self.decide_tool_exposure(
                    tool_specs,
                    policy_profile=policy_profile,
                    sandbox_mode=sandbox_mode,
                    workspace_state=workspace_state,
                ).selected_tool_names
            )
        else:
            allowed = set(self._plan().phase(self._state().current_phase).allowed_tools)
            current_step = self.semantic_rolling_plan().current_step()
            if current_step is not None:
                allowed.update(current_step.allowed_capabilities)
            repair_execution_block = self._repair_contract_execution_block()
            if repair_execution_block:
                allowed &= self._repair_contract_evidence_tools()
            else:
                repair_allowed = self._active_repair_allowed_tools()
                if repair_allowed:
                    allowed &= repair_allowed
            benchmark_allowed = self._benchmark_allowed_tools()
            if benchmark_allowed:
                allowed &= benchmark_allowed
        return [
            tool
            for tool in tools
            if tool.get("function", {}).get("name") in allowed
        ]

    def semantic_rolling_plan(self) -> RollingPlan:
        state = self._state()
        if state.rolling_plan:
            return RollingPlan.from_dict(state.rolling_plan)
        contract = self._contract()
        if contract is not None:
            plan = self.semantic_planner.initial_plan(contract)
        else:
            plan = self.semantic_planner.initial_plan(
                {"user_goal": state.normalized_goal, "acceptance_criteria": []}
            )
        state.rolling_plan = plan.to_dict()
        self._persist()
        return plan

    def _consume_repair_signal(self, signal: dict[str, Any]) -> None:
        contract = _repair_contract_payload(signal)
        if not contract or not self._state().task_contract:
            return
        self._state().rolling_plan = self.semantic_planner.repair_plan(
            contract,
            task_contract=self._state().task_contract,
        ).to_dict()

    def _active_repair_contract(self) -> dict[str, Any]:
        # The contract is most relevant during repair, but satisfaction
        # assessment and finalization also need access after phase advancement.
        if self.state is None or self.state.current_phase not in {
            "repairing_failures",
            "finalizing",
            "running_verification",
        }:
            return {}
        if not self.evidence.repair_plans:
            return {}
        # Search backwards for a repair plan that carries a repair_contract.
        # VerificationRunner may append additional blocked plans with empty
        # verification contracts after a failed rerun; prefer the newest
        # contract that still carries executable verification steps.
        fallback: dict[str, Any] = {}
        for plan in reversed(self.evidence.repair_plans):
            if not isinstance(plan, dict):
                continue
            contract = _repair_contract_payload(plan)
            if not contract:
                continue
            if not fallback:
                fallback = contract
            if _repair_contract_has_verification_steps(contract):
                return contract
        return fallback

    def _repair_contract_execution_block(self) -> tuple[str, str] | None:
        if self.state is None or self.state.current_phase != "repairing_failures":
            return None
        contract = self._active_repair_contract()
        if not contract:
            return (
                "repair_contract_missing",
                "repairing_failures requires FailureAnalyzer/RepairPlanner contract evidence",
            )
        blocked_reason = _repair_contract_blocked_reason(contract, {})
        if not blocked_reason:
            return None
        if blocked_reason == "repair_contract_requires_user_input":
            return blocked_reason, blocked_reason
        if blocked_reason.startswith("repair_contract_invalid"):
            return "repair_contract_invalid", blocked_reason
        if blocked_reason == "repair_contract_low_confidence":
            return blocked_reason, blocked_reason
        return "repair_contract_blocked", blocked_reason

    @staticmethod
    def _repair_contract_evidence_tools() -> set[str]:
        return set(READ_TOOLS | DIFF_TOOLS | {"get_verification_result"})

    def _active_repair_allowed_tools(self) -> set[str]:
        contract = self._active_repair_contract()
        if not contract or contract.get("needs_user_input") or contract.get("blocked_reason"):
            return set()
        tools = {str(item) for item in contract.get("allowed_tool_names") or [] if item}
        if tools:
            return tools
        for candidate in contract.get("action_candidates") or []:
            if isinstance(candidate, dict):
                tools.update(str(item) for item in candidate.get("tool_hints") or [] if item)
        return tools

    def _benchmark_allowed_tools(self) -> set[str]:
        return {
            str(item)
            for item in self._benchmark_constraints.get("allowed_tools") or []
            if item
        }

    def _active_repair_target_files(self) -> set[str]:
        contract = self._active_repair_contract()
        if not contract or contract.get("needs_user_input") or contract.get("blocked_reason"):
            return set()
        return {
            _normalize_planner_path(item)
            for item in contract.get("target_files") or []
            if item
        }

    def get_active_verification_contract(self) -> VerificationContract:
        """Public entrypoint: return the active VerificationContract for the current repair phase.

        External callers (e.g. VerificationRunner) should use this instead of
        reaching into private methods.  Delegates to
        :meth:`_active_repair_verification_contract`.
        """
        return self._active_repair_verification_contract()

    def _active_repair_verification_contract(self) -> VerificationContract:
        """Extract the structured verification contract from the active repair.

        When benchmark constraints declare a ``verification_command``, augment
        the contract with that command as an allowed (non-required) step so the
        gate at :meth:`authorize_action` does not deny the canonical
        manifest-declared verification command. This does not bypass the gate
        — it makes the contract correct.
        """
        contract = self._active_repair_contract()
        if not contract:
            return VerificationContract.empty()
        payload = contract.get("verification_contract")
        if isinstance(payload, dict) and payload.get("steps"):
            base = VerificationContract.from_dict(payload)
        else:
            plan_strings = contract.get("verification_plan") or []
            if plan_strings:
                base = VerificationContract.from_plan_strings(plan_strings)
            else:
                base = VerificationContract.empty()
        return self._augment_with_benchmark_verification_command(base)

    def _augment_with_benchmark_verification_command(
        self, contract: VerificationContract
    ) -> VerificationContract:
        """Augment ``contract`` with the benchmark ``verification_command`` step.

        The benchmark step is an allowance (``required=False``), not a hard
        requirement — it ensures the gate allows the manifest-declared
        verification command without affecting satisfaction assessment.
        Empty contracts already allow all commands, so they are returned unchanged.
        """
        benchmark_cmd = str(
            self._benchmark_constraints.get("verification_command") or ""
        ).strip()
        if not benchmark_cmd:
            return contract
        if not contract.steps:
            return contract
        benchmark_step = VerificationStep(
            step_id="vstep_benchmark",
            command=benchmark_cmd,
            kind="smoke",
            required=False,
        )
        if any(
            step.matches_command(benchmark_step.command_argv)
            for step in contract.steps
        ):
            return contract
        return VerificationContract(
            contract_id=contract.contract_id,
            steps=[*contract.steps, benchmark_step],
            status=contract.status,
            validation_errors=list(contract.validation_errors),
        )

    @staticmethod
    def _apply_benchmark_verification_requirement(
        task_contract: dict[str, Any], verification_command: str
    ) -> dict[str, Any]:
        """Override rules-based ``verification_requirements`` with the benchmark command.

        The rules-based ``TaskContractBuilder.from_rules`` synthesizes
        ``["python", <path>]`` from the goal text, which rarely matches the
        manifest-declared ``verification_command`` (e.g. ``python -m pytest ...``).
        When a benchmark ``verification_command`` is present, replace the
        ``command`` field of existing requirements so the model sees and runs
        the correct verification command from the start.
        """
        import shlex

        try:
            cmd_argv = shlex.split(verification_command)
        except ValueError:
            cmd_argv = verification_command.split()
        if not cmd_argv:
            return task_contract
        existing = list(task_contract.get("verification_requirements") or [])
        if existing:
            existing[0] = {**existing[0], "command": cmd_argv}
        else:
            existing = [
                {
                    "description": f"Run verification: {verification_command}",
                    "command": cmd_argv,
                    "required": True,
                }
            ]
        return {**task_contract, "verification_requirements": existing}

    def assess_verification_contract_satisfaction(self) -> ContractSatisfaction:
        """Evaluate whether the active verification contract is satisfied.

        Uses step-level evidence (step_id → check_id → status) when available.
        Falls back to blocking when step_evidence is absent but contract has
        steps — cannot assume satisfaction without evidence.
        """
        active_repair = self._active_repair_contract()
        contract = self._active_repair_verification_contract()
        if not contract.steps:
            if not active_repair:
                if (
                    self.state is not None
                    and self.state.current_phase in {
                        "repairing_failures",
                        "running_verification",
                        "finalizing",
                    }
                    and self.evidence.repair_plans
                ):
                    return ContractSatisfaction(
                        contract_id=contract.contract_id,
                        satisfied=False,
                        completed_steps=[],
                        failed_steps=[],
                        skipped_steps=[],
                        reason="repair_contract_missing",
                    )
                return ContractSatisfaction(
                    contract_id=contract.contract_id,
                    satisfied=True,
                    completed_steps=[],
                    failed_steps=[],
                    skipped_steps=[],
                    reason="no_verification_steps",
                )
            return ContractSatisfaction(
                contract_id=contract.contract_id,
                satisfied=False,
                completed_steps=[],
                failed_steps=[],
                skipped_steps=[],
                reason="no_verification_steps",
            )
        verification_results = self.evidence.verification_results
        if not verification_results:
            return ContractSatisfaction(
                contract_id=contract.contract_id,
                satisfied=False,
                completed_steps=[],
                failed_steps=[step.step_id for step in contract.steps],
                skipped_steps=[],
                reason="no_verification_results",
            )
        latest = verification_results[-1]
        # Primary path: use step_evidence from the observation
        raw_step_evidence = (latest.get("verification") or latest).get("step_evidence") or []
        if raw_step_evidence:
            return self._satisfaction_from_step_evidence(
                contract=contract,
                step_evidence_raw=raw_step_evidence,
            )
        # Fallback: no step_evidence but contract has steps → cannot determine
        # satisfaction from global status alone; require step-level alignment.
        return ContractSatisfaction(
            contract_id=contract.contract_id,
            satisfied=False,
            completed_steps=[],
            failed_steps=[step.step_id for step in contract.steps if step.required],
            skipped_steps=[step.step_id for step in contract.steps if not step.required],
            reason="step_evidence_missing",
        )

    @staticmethod
    def _satisfaction_from_step_evidence(
        *,
        contract: VerificationContract,
        step_evidence_raw: list[dict[str, Any]],
    ) -> ContractSatisfaction:
        evidence_by_step: dict[str, dict[str, Any]] = {}
        for item in step_evidence_raw:
            if isinstance(item, dict) and item.get("step_id"):
                evidence_by_step[item["step_id"]] = item

        completed: list[str] = []
        failed: list[str] = []
        skipped: list[str] = []
        step_evidence: list[StepEvidence] = []
        for step in contract.steps:
            ev = evidence_by_step.get(step.step_id)
            if ev is None:
                # Step not covered by evidence
                if step.required:
                    failed.append(step.step_id)
                    step_evidence.append(StepEvidence(
                        step_id=step.step_id, check_id=None, command_id=None,
                        status="no_evidence",
                    ))
                else:
                    skipped.append(step.step_id)
                    step_evidence.append(StepEvidence(
                        step_id=step.step_id, check_id=None, command_id=None,
                        status="skipped",
                    ))
                continue
            status = str(ev.get("status") or "unknown")
            evidence_entry = StepEvidence(
                step_id=step.step_id,
                check_id=ev.get("check_id"),
                command_id=ev.get("command_id"),
                status=status,
                artifact_ref=ev.get("artifact_ref"),
            )
            step_evidence.append(evidence_entry)
            if status == "passed":
                completed.append(step.step_id)
            elif status in {"failed", "blocked", "timeout", "flaky", "not_executed"}:
                if step.required:
                    failed.append(step.step_id)
                else:
                    skipped.append(step.step_id)
            elif not step.required:
                skipped.append(step.step_id)
            else:
                failed.append(step.step_id)

        satisfied = not failed
        return ContractSatisfaction(
            contract_id=contract.contract_id,
            satisfied=satisfied,
            completed_steps=completed,
            failed_steps=failed,
            skipped_steps=skipped,
            reason=None if satisfied else f"failed_steps={len(failed)}",
            step_evidence=step_evidence,
        )

    def _record_repair_signal_consumed(
        self,
        signal: dict[str, Any],
        decision: ReplanDecision,
    ) -> None:
        if self.trace is None:
            return
        payload = {
            "signal_id": signal.get("signal_id"),
            "repair_plan_id": signal.get("repair_plan_id"),
            "analysis_id": signal.get("analysis_id"),
            "contract_id": signal.get("contract_id"),
            "failure_category": signal.get("failure_category"),
            "target_files": signal.get("target_files") or [],
            "verification_plan": signal.get("verification_plan") or [],
            "confidence": signal.get("confidence"),
            "decision": decision.to_dict(),
        }
        if hasattr(self.trace, "record"):
            self.trace.record("repair_signal_consumed", payload)

    def _maybe_advance_after_tool(self, tool_name: str) -> None:
        state = self._state()
        plan = self._plan()
        if state.status == TaskStatus.UNDERSTANDING_TASK:
            state.status = TaskStatus.INSPECTING_WORKSPACE
            state.current_phase = "inspecting_workspace"
            plan.current_phase = "inspecting_workspace"
        if state.current_phase == "inspecting_workspace" and self.evidence.inspected_files:
            state.status = TaskStatus.PLANNING_CHANGES
            state.current_phase = "planning_changes"
            plan.current_phase = "planning_changes"
        elif state.current_phase == "planning_changes" and tool_name in (READ_TOOLS | EDIT_PLAN_TOOLS):
            state.status = TaskStatus.APPLYING_CHANGES
            state.current_phase = "applying_changes"
            plan.current_phase = "applying_changes"

    def _mutation_contract_ready_for_verification(self) -> bool:
        expected = self._benchmark_expected_file_changes()
        if not expected:
            return True
        return not self._missing_benchmark_expected_file_changes()

    def _benchmark_expected_file_changes(self) -> set[str]:
        return {
            _normalize_planner_path(item)
            for item in self._benchmark_constraints.get("expected_file_changes") or []
            if item
        }

    def _missing_benchmark_expected_file_changes(self) -> list[str]:
        expected = self._benchmark_expected_file_changes()
        if not expected:
            return []
        changed = {_normalize_planner_path(item) for item in self._changed_files()}
        return sorted(expected - changed)

    def _auto_advance_before_step(self) -> None:
        state = self._state()
        plan = self._plan()
        if state.current_phase == "planning_changes" and self.evidence.inspected_files:
            state.status = TaskStatus.APPLYING_CHANGES
            state.current_phase = "applying_changes"
            plan.current_phase = "applying_changes"
        if (
            state.current_phase == "applying_changes"
            and self.evidence.applied_changes
            and self._mutation_contract_ready_for_verification()
        ):
            state.status = TaskStatus.RUNNING_VERIFICATION
            state.current_phase = "running_verification"
            plan.current_phase = "running_verification"
        if (
            state.current_phase == "running_verification"
            and state.final_assessment.get("status") in {"ready", "ready_with_warnings"}
        ):
            state.status = TaskStatus.FINALIZING
            state.current_phase = "finalizing"
            plan.current_phase = "finalizing"

    def _default_plan(self, task_id: str) -> TaskPlan:
        phases = [
            TaskPhase(
                phase_id="understanding_task",
                name="Understand Task",
                purpose="Normalize the user goal and identify constraints.",
                allowed_tools=sorted(READ_TOOLS),
                allowed_actions=[
                    ActionKind.INSPECT_WORKSPACE,
                    ActionKind.READ_RELEVANT_FILES,
                    ActionKind.SEARCH_CODE,
                    ActionKind.ANALYZE_ISSUE,
                ],
                required_evidence=["goal"],
            ),
            TaskPhase(
                phase_id="inspecting_workspace",
                name="Inspect Workspace",
                purpose="Read only the files needed to understand the task.",
                allowed_tools=sorted(READ_TOOLS),
                allowed_actions=[
                    ActionKind.INSPECT_WORKSPACE,
                    ActionKind.READ_RELEVANT_FILES,
                    ActionKind.SEARCH_CODE,
                    ActionKind.ANALYZE_ISSUE,
                ],
                required_evidence=["inspected_files"],
            ),
            TaskPhase(
                phase_id="planning_changes",
                name="Plan Changes",
                purpose="Propose a small changeset from inspected evidence.",
                allowed_tools=sorted(READ_TOOLS | EDIT_PLAN_TOOLS),
                allowed_actions=[
                    ActionKind.ANALYZE_ISSUE,
                    ActionKind.PROPOSE_CHANGE_SET,
                    ActionKind.READ_RELEVANT_FILES,
                    ActionKind.SEARCH_CODE,
                ],
                required_evidence=["inspected_files"],
            ),
            TaskPhase(
                phase_id="applying_changes",
                name="Apply Changes",
                purpose="Apply mutations through EditExecutor, which delegates writes to WorkspaceMutationManager.",
                allowed_tools=sorted(MUTATION_TOOLS | READ_TOOLS | DIFF_TOOLS),
                allowed_actions=[
                    ActionKind.APPLY_MUTATION,
                    ActionKind.READ_RELEVANT_FILES,
                    ActionKind.SEARCH_CODE,
                    ActionKind.ANALYZE_ISSUE,
                ],
                required_evidence=["applied_changes"],
            ),
            TaskPhase(
                phase_id="running_verification",
                name="Run Verification",
                purpose="Run planned verification through VerificationRunner.",
                allowed_tools=sorted(VERIFICATION_TOOLS | DIFF_TOOLS | {"read_file", "workspace_health"}),
                allowed_actions=[
                    ActionKind.RUN_VERIFICATION,
                    ActionKind.READ_RELEVANT_FILES,
                    ActionKind.ANALYZE_ISSUE,
                ],
                required_evidence=["verification_results"],
            ),
            TaskPhase(
                phase_id="repairing_failures",
                name="Repair Failures",
                purpose="Repair failures using parsed evidence and bounded retries.",
                allowed_tools=sorted(MUTATION_TOOLS | EDIT_PLAN_TOOLS | READ_TOOLS | DIFF_TOOLS | VERIFICATION_TOOLS),
                allowed_actions=[
                    ActionKind.APPLY_MUTATION,
                    ActionKind.ANALYZE_ISSUE,
                    ActionKind.PARSE_FAILURE,
                    ActionKind.REPAIR_CHANGE,
                    ActionKind.READ_RELEVANT_FILES,
                    ActionKind.RUN_VERIFICATION,
                ],
                required_evidence=["parsed_failures"],
            ),
            TaskPhase(
                phase_id="finalizing",
                name="Finalize",
                purpose="Generate a final report from component evidence.",
                allowed_tools=["get_verification_result", "inspect_diff", "read_file", "workspace_health"],
                allowed_actions=[
                    ActionKind.FINALIZE,
                    ActionKind.ANALYZE_ISSUE,
                    ActionKind.READ_RELEVANT_FILES,
                ],
                required_evidence=["final_report"],
            ),
        ]
        return TaskPlan(
            plan_id=f"plan_{uuid4().hex[:12]}",
            task_id=task_id,
            phases=phases,
            current_phase="understanding_task",
        )

    def _changed_files(self) -> list[str]:
        changed: set[str] = set()
        for change in self.evidence.applied_changes:
            for path in change.get("changed_files") or []:
                changed.add(str(path))
        return sorted(changed)

    def _record_dynamic_retrieval(
        self,
        *,
        trigger: str,
        failure_analysis: dict[str, Any] | None = None,
        changed_files: list[str] | None = None,
    ) -> None:
        result = self.retrieval_orchestrator.retrieve(
            current_step=self.semantic_rolling_plan().current_step(),
            failure_analysis=failure_analysis,
            changed_files=changed_files or [],
            task_contract=self._state().task_contract,
            project_index=self.project_index,
            trigger=trigger,
        )
        if result not in self.evidence.retrieval_results:
            self.evidence.retrieval_results.append(result)
        self._record_event(
            decision="dynamic_retrieval",
            reason=f"Recorded {trigger} retrieval guidance.",
            extra={"retrieval": result},
        )

    def _persist(self) -> None:
        if self.state is None or self.plan is None:
            return
        self.state.touch()
        self.store.save(
            state=self.state,
            plan=self.plan,
            evidence=self.evidence,
            budget=self.budget,
            final_report=self.final_report,
        )

    def _record_event(
        self,
        *,
        action: AgentAction | None = None,
        action_id: str | None = None,
        action_kind: str | None = None,
        decision: str | None = None,
        reason: str | None = None,
        evidence_refs: list[str] | None = None,
        replan_decision: dict[str, Any] | None = None,
        completion_assessment: dict[str, Any] | None = None,
        extra: dict[str, Any] | None = None,
    ) -> None:
        if self.state is None:
            return
        self.store.append_event(
            self.state.session_id,
            task_id=self.state.task_id,
            phase=self.state.current_phase,
            action_id=action.action_id if action else action_id,
            action_kind=action.kind.value if action else action_kind,
            decision=decision,
            reason=reason,
            evidence_refs=evidence_refs,
            budget_state=self.budget.to_dict(),
            risk_level=self.state.risk_level.value,
            replan_decision=replan_decision,
            completion_assessment=completion_assessment,
            extra=extra,
        )
        if self.trace is not None:
            payload = {
                    "task_id": self.state.task_id,
                    "session_id": self.state.session_id,
                    "phase": self.state.current_phase,
                    "action_id": action.action_id if action else action_id,
                    "action_kind": action.kind.value if action else action_kind,
                    "decision": decision,
                    "reason": reason,
                    "evidence_refs": evidence_refs or [],
                    "budget_state": self.budget.to_dict(),
                    "risk_level": self.state.risk_level.value,
                    "replan_decision": replan_decision,
                    "completion_assessment": completion_assessment,
                }
            payload.update(extra or {})
            self.trace.record("planner", payload)

    def _state(self) -> TaskState:
        if self.state is None:
            raise RuntimeError("Planner task has not started.")
        return self.state

    def _active_user_constraints(self) -> list[str]:
        state = self._state()
        constraints = list(state.constraints)
        for item in (state.task_contract or {}).get("constraints") or []:
            if isinstance(item, dict):
                text = item.get("text") or item.get("description") or item.get("value")
                if text:
                    self._append_unique(constraints, text)
            else:
                self._append_unique(constraints, item)
        return constraints

    def _throw_if_cancelled(self) -> None:
        token = getattr(self, "cancellation_token", None)
        if token is not None and hasattr(token, "throw_if_cancelled"):
            token.throw_if_cancelled()

    def _plan(self) -> TaskPlan:
        if self.plan is None:
            raise RuntimeError("Planner task has not started.")
        return self.plan

    @staticmethod
    def _content_payload(result: Any) -> dict[str, Any]:
        if isinstance(result, dict) and "content" in result and isinstance(result["content"], dict):
            return result["content"]
        return result if isinstance(result, dict) else {}

    @staticmethod
    def _dict_payload(value: Any) -> dict[str, Any]:
        return value if isinstance(value, dict) else {}

    @staticmethod
    def _dict_list(value: Any) -> list[dict[str, Any]]:
        if not isinstance(value, list):
            return []
        return [item for item in value if isinstance(item, dict)]

    @staticmethod
    def _string_list(value: Any) -> list[str]:
        if value is None:
            return []
        if isinstance(value, str):
            return [value]
        if isinstance(value, list | tuple | set):
            return [str(item) for item in value if item is not None]
        return [str(value)]

    @staticmethod
    def _append_unique(values: list[str], value: Any) -> None:
        if value is None:
            return
        text = str(value)
        if text not in values:
            values.append(text)

    @staticmethod
    def _is_read_only_goal(user_goal: str) -> bool:
        lowered = user_goal.lower()
        read_markers = {
            "read",
            "inspect",
            "summarize",
            "explain",
            "find",
            "list",
            "say",
            "阅读",
            "总结",
            "说明",
            "解释",
            "查找",
            "看看",
        }
        write_markers = {
            "change",
            "modify",
            "implement",
            "fix",
            "add",
            "delete",
            "refactor",
            "upgrade",
            "修改",
            "实现",
            "修复",
            "新增",
            "删除",
            "升级",
        }
        return any(marker in lowered for marker in read_markers) and not any(
            marker in lowered for marker in write_markers
        )

    @staticmethod
    def _requires_workspace_evidence(user_goal: str) -> bool:
        lowered = user_goal.lower()
        markers = {
            "read",
            "inspect",
            "summarize",
            "explain",
            "find",
            "list",
            "file",
            "readme",
            "阅读",
            "总结",
            "说明",
            "解释",
            "查找",
            "看看",
            "文件",
        }
        return any(marker in lowered for marker in markers)


def _dict_like(value: Any) -> dict[str, Any]:
    if value is None:
        return {}
    if hasattr(value, "to_dict"):
        payload = value.to_dict()
        return payload if isinstance(payload, dict) else {}
    return value if isinstance(value, dict) else {}


def _repair_contract_payload(*payloads: dict[str, Any]) -> dict[str, Any]:
    for payload in payloads:
        if not isinstance(payload, dict):
            continue
        contract = payload.get("repair_contract")
        if isinstance(contract, dict):
            return contract
        if "contract_id" in payload and "action_candidates" in payload:
            return payload
    return {}


def _repair_contract_blocked_reason(
    contract: dict[str, Any],
    signal: dict[str, Any],
) -> str | None:
    blocked_reason = (
        contract.get("blocked_reason")
        or signal.get("blocked_reason")
    )
    if blocked_reason:
        return str(blocked_reason)
    if contract.get("needs_user_input") or signal.get("needs_user_input"):
        return "repair_contract_requires_user_input"
    validation_errors = contract.get("validation_errors")
    if validation_errors:
        return f"repair_contract_invalid: {validation_errors}"
    try:
        confidence = float(contract.get("confidence", signal.get("confidence", 1.0)))
    except (TypeError, ValueError):
        return "repair_contract_invalid_confidence"
    if confidence < 0.45:
        return "repair_contract_low_confidence"
    return None


def _repair_contract_has_verification_steps(contract: dict[str, Any]) -> bool:
    payload = contract.get("verification_contract")
    if isinstance(payload, dict) and payload.get("steps"):
        return True
    return bool(contract.get("verification_plan"))


def _normalize_planner_path(path: Any) -> str:
    text = str(path or "").strip().replace("\\", "/")
    while text.startswith("./"):
        text = text[2:]
    return text


def create_or_resume_planner(
    *,
    workspace_root: Path,
    session_id: str | None,
    task_id: str,
    user_goal: str,
    trace: TraceRecorderProtocol | None,
    workspace_health: Any,
    fallback_session_id: str | None = None,
    session_run_mode: str = "new",
) -> Planner:
    planner = Planner(
        workspace_root,
        session_id=session_id or fallback_session_id or task_id,
        task_id=task_id,
        trace=trace,
    )
    if session_id:
        try:
            planner.resume(session_id, workspace_health=workspace_health.to_dict())
        except FileNotFoundError:
            planner.session_id = session_id
            planner.task_id = task_id
            normalized_goal = " ".join(user_goal.split())
            planner.state = TaskState(
                task_id=task_id,
                session_id=session_id,
                user_goal=user_goal,
                normalized_goal=normalized_goal,
                effective_goal=normalized_goal,
                status=TaskStatus.NEEDS_REVIEW,
                current_phase="inspecting_workspace",
                blocked_reasons=["planner state missing during session recovery"],
            )
            planner.plan = planner._default_plan(task_id)
            planner.evidence = EvidenceLedger()
            planner.budget = ExecutionBudget()
            planner._persist()
            planner._record_event(
                decision="resume_failed",
                reason="Planner state missing during session recovery.",
            )
        if session_run_mode == "continue":
            planner.continue_with_instruction(user_goal)
        return planner
    planner.start_task(user_goal)
    return planner


def _sandbox_mode(sandbox_capability: dict[str, Any]) -> str | None:
    value = sandbox_capability.get("mode") or sandbox_capability.get("filesystem_mode")
    return str(value) if value else None
