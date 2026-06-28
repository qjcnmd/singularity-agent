from __future__ import annotations

import json
from dataclasses import dataclass
from enum import StrEnum
from pathlib import Path
from typing import Any

from rich.console import Console

from singularity.context import ContextManager
from singularity.execution_outcome import ExecutionOutcome, ExecutionOutcomeStatus
from singularity.failure_analysis import FailureAnalysisRequest, FailureAnalyzer
from singularity.instructions import PromptAssemblyPipeline
from singularity.interaction import InteractionController, ProgressEvent
from singularity.model import ModelErrorKind, ModelPurpose, ModelRunner, ModelTurnStatus
from singularity.observability.protocols import TraceStorageProtocol
from singularity.planner import Planner, TaskStatus
from singularity.provider import OpenAICompatibleProvider
from singularity.repair import RepairPlanner
from singularity.run_controller import RunController
from singularity.tool_protocol.engine import ToolProtocolEngine
from singularity.tools import ToolExecutor, ToolRegistry

SYSTEM_PROMPT = """You are Singularity, a local coding agent harness.

Use only the tools exposed in the current request schema. The agent loop provides
a per-turn tool protocol summary with the available tools and preferred execution
paths.

Never claim that you inspected, edited, or verified anything unless the
corresponding tool executor returned evidence. File mutations must go through
the exposed EditExecutor or patch tools. Verification behavior such as tests,
lint, typecheck, builds, syntax checks, and smoke checks must use the exposed
VerificationRunner tools instead of ad-hoc command tools.
Never claim a coding task is complete unless the latest VerificationRunner
CompletionAssessment says it is ready or ready_with_warnings, and report any
warnings or remaining risks.
Do not claim that you browsed the web, stored memory, or contacted other agents.
When you have enough information, answer the user directly.
"""


class AgentLoopStatus(StrEnum):
    COMPLETED = "completed"
    BLOCKED = "blocked"
    MAX_TURNS_EXCEEDED = "max_turns_exceeded"
    FAILED = "failed"


@dataclass(frozen=True, eq=False)
class AgentLoopResult:
    status: AgentLoopStatus
    final_answer: str
    turn: int
    error_code: str | None = None
    diagnostics: dict[str, Any] | None = None

    def __str__(self) -> str:
        return self.final_answer

    def __contains__(self, value: object) -> bool:
        return str(value) in self.final_answer

    def __eq__(self, other: object) -> bool:
        if isinstance(other, str):
            return self.final_answer == other
        if isinstance(other, AgentLoopResult):
            return self.to_dict() == other.to_dict()
        return False

    def startswith(self, prefix: str, *args: Any) -> bool:
        return self.final_answer.startswith(prefix, *args)

    def to_dict(self) -> dict[str, Any]:
        return {
            "status": self.status.value,
            "final_answer": self.final_answer,
            "turn": self.turn,
            "error_code": self.error_code,
            "diagnostics": self.diagnostics or {},
        }


class AgentLoop:
    def __init__(
        self,
        *,
        provider: OpenAICompatibleProvider | None = None,
        model_runner: ModelRunner,
        tools: ToolRegistry,
        trace: TraceStorageProtocol,
        console: Console,
        max_turns: int,
        planner: Planner,
        tool_executor: ToolExecutor,
        tool_protocol: ToolProtocolEngine,
        prompt_assembly: PromptAssemblyPipeline,
        interaction_controller: InteractionController | None = None,
        context_manager: ContextManager | None = None,
        context_db_path: Path | None = None,
        failure_analyzer: FailureAnalyzer | None = None,
        repair_planner: RepairPlanner | None = None,
        strict: bool = False,
    ) -> None:
        if model_runner is None:
            raise ValueError("model_runner is required; AgentLoop does not assemble model runners.")
        if planner is None:
            raise ValueError("planner is required; AgentLoop does not assemble Planner.")
        if tool_executor is None:
            raise ValueError("tool_executor is required; AgentLoop does not assemble ToolExecutor.")
        if tool_protocol is None:
            raise ValueError(
                "tool_protocol is required; AgentLoop does not assemble ToolProtocolEngine."
            )
        if prompt_assembly is None:
            raise ValueError(
                "prompt_assembly is required; AgentLoop does not assemble PromptAssemblyPipeline."
            )
        self.provider = provider
        self.model_runner = model_runner
        self.tools = tools
        self.trace = trace
        self.console = console
        self.max_turns = max_turns
        self.planner = planner
        self.tool_executor = tool_executor
        self.tool_protocol = tool_protocol
        self.prompt_assembly = prompt_assembly
        self.interaction_controller = interaction_controller
        self.context_manager = context_manager
        self.context_db_path = context_db_path
        self.failure_analyzer = failure_analyzer or FailureAnalyzer(
            model_runner=model_runner,
            trace=trace,
        )
        self.repair_planner = repair_planner or RepairPlanner(trace=trace)
        self._failure_analysis_fingerprints: set[str] = set()
        self._failure_replan_signals: dict[str, Any] = {}
        self._failure_analysis_snapshots: dict[str, dict[str, int]] = {}
        self._completion_rejection_state: dict[str, dict[str, Any]] = {}
        self.strict = strict

    def run(self, user_goal: str) -> AgentLoopResult:
        planner = self.planner
        controller = RunController(planner=planner, trace=self.trace)
        if planner.state is None:
            controller.start(user_goal)
        effective_goal = getattr(planner.state, "effective_goal", None) or user_goal
        context = self.context_manager
        if context is None:
            context = ContextManager(
                system_prompt=SYSTEM_PROMPT,
                user_goal=effective_goal,
                provider=self.provider,
                model_runner=self.model_runner,
                run_id=self.trace.run_id,
                session_id=getattr(planner, "session_id", self.trace.run_id),
                task_id=getattr(planner, "task_id", self.trace.run_id),
                db_path=self.context_db_path or self._context_db_path(),
                trace=self.trace,
            )
        else:
            context.set_user_goal(effective_goal)
        model_runner = self.model_runner
        prompt_assembly = self.prompt_assembly
        tool_schemas = self.tools.openai_tools(strict=self.strict)
        tool_executor = self.tool_executor
        tool_protocol = self.tool_protocol

        def run_turn(turn: int) -> AgentLoopResult | None:
            self._publish_progress(turn)
            planner.step()
            effective_goal = getattr(planner.state, "effective_goal", None) or user_goal
            context.set_user_goal(effective_goal)
            active_tool_schemas = planner.filtered_tools(tool_schemas, tool_specs=self.tools.list())
            allowed_tool_names = [
                tool.get("function", {}).get("name")
                for tool in active_tool_schemas
                if tool.get("function", {}).get("name")
            ]
            request = model_runner.build_request_from_context(
                context,
                run_id=self.trace.run_id,
                session_id=getattr(planner, "session_id", self.trace.run_id),
                task_id=getattr(planner, "task_id", self.trace.run_id),
                phase_id=planner.state.current_phase if planner.state else "model",
                action_id=f"turn_{turn}",
                purpose=ModelPurpose.PLAN_NEXT_ACTION,
                allowed_tool_names=allowed_tool_names,
                planner_context=planner.planner_context_message(),
                prompt_assembly=prompt_assembly,
                user_task=effective_goal,
                strict_tools=self.strict,
            )
            planner.record_instruction_prompt_observation(dict(prompt_assembly.summary()))
            result = model_runner.run_turn(request)
            context.record_model_usage(result)
            if result.status != ModelTurnStatus.SUCCESS:
                self._record_model_failure(planner, result, turn=turn)
                outcome = self._outcome_from_model_failure(result)
                controller.apply_outcome(outcome)
                self._record_outcome_context(context, planner, outcome)
                terminal = self._terminal_result_from_outcome(outcome, turn=turn)
                if terminal is not None:
                    return terminal
                return None

            assistant_message = self._assistant_message_from_result(result)
            if not result.tool_calls:
                context.add_assistant_message(assistant_message)
                final = self._attempt_finalize(
                    planner,
                    controller=controller,
                    context=context,
                    turn=turn,
                    model_answer=assistant_message.get("content") or "",
                )
                if final is not None:
                    return final
                return None

            observation_start = len(context.tool_observations)
            protocol_result = tool_protocol.process_model_turn(
                request=request,
                result=result,
                turn=turn,
                context=context,
                tool_executor=tool_executor,
                planner=planner,
            )
            if protocol_result.next_action == "finalize":
                final = self._attempt_finalize(
                    planner,
                    controller=controller,
                    context=context,
                    turn=turn,
                    model_answer=assistant_message.get("content") or "",
                )
                if final is not None:
                    return final
                return None

            observations = context.tool_observations[observation_start:]
            controller.apply_protocol_result(protocol_result, observations=observations)
            reduced_outcome = controller.reduce_protocol_result(
                protocol_result,
                observations=observations,
            )
            if reduced_outcome is not None:
                controller.apply_outcome(reduced_outcome)
                self._record_outcome_context(context, planner, reduced_outcome)
                blocked = self._maybe_analyze_failure(
                    planner,
                    context,
                    outcome=reduced_outcome,
                    failure_source="tool",
                    turn=turn,
                )
                if blocked is not None:
                    controller.apply_outcome(blocked)
                    self._record_outcome_context(context, planner, blocked)
                    terminal = self._terminal_result_from_outcome(blocked, turn=turn)
                    if terminal is not None:
                        return terminal
                terminal = self._terminal_result_from_outcome(reduced_outcome, turn=turn)
                if terminal is not None:
                    return terminal
            blocked = self._maybe_analyze_failure(
                planner,
                context,
                failure_source="verification",
                turn=turn,
            )
            if blocked is not None:
                controller.apply_outcome(blocked)
                self._record_outcome_context(context, planner, blocked)
                terminal = self._terminal_result_from_outcome(blocked, turn=turn)
                if terminal is not None:
                    return terminal
            if self._should_auto_finalize_after_tools(planner, protocol_result):
                final = self._attempt_finalize(
                    planner,
                    controller=controller,
                    context=context,
                    turn=turn,
                    model_answer=assistant_message.get("content") or "",
                )
                if final is not None:
                    return final
            return None

        def on_max_turns(max_turns: int) -> AgentLoopResult:
            message = f"Stopped after max_turns={max_turns}; the model did not produce a final answer."
            outcome = ExecutionOutcome(
                status=ExecutionOutcomeStatus.BLOCKED,
                source="agent_loop",
                reason=message,
                error_code="max_turns_exceeded",
                next_action="blocked",
                observation_summary=message,
                retry_allowed=False,
                metadata={"max_turns": max_turns},
            )
            controller.apply_outcome(outcome)
            self._record_outcome_context(context, planner, outcome)
            self.trace.record("error", {"type": "MaxTurnsExceeded", "message": message})
            self.trace.record("final_answer", {"turn": max_turns, "content": message})
            return AgentLoopResult(
                status=AgentLoopStatus.MAX_TURNS_EXCEEDED,
                final_answer=message,
                turn=max_turns,
                error_code="max_turns_exceeded",
            )

        return controller.run_loop(
            effective_goal,
            max_turns=self.max_turns,
            run_turn=run_turn,
            on_max_turns=on_max_turns,
        )

    def _attempt_finalize(
        self,
        planner: Planner,
        *,
        controller: RunController,
        context: ContextManager,
        turn: int,
        model_answer: str,
    ) -> AgentLoopResult | None:
        assessment = planner.assess_completion(mark_blocked=False)
        if assessment["status"] != TaskStatus.COMPLETED.value:
            outcome = ExecutionOutcome(
                status=ExecutionOutcomeStatus.REPLAN_REQUIRED,
                source="completion",
                reason="completion_rejected",
                error_code="completion_rejected",
                missing_evidence=list(assessment["unmet"]),
                next_action="continue",
                observation_summary=(
                    "Completion rejected because evidence is missing: "
                    + ", ".join(assessment["unmet"])
                ),
                retry_allowed=True,
                metadata={"assessment": assessment},
            )
            controller.apply_outcome(outcome)
            self._record_outcome_context(context, planner, outcome)
            blocked = self._maybe_analyze_failure(
                planner,
                context,
                outcome=outcome,
                failure_source="completion",
                turn=turn,
            )
            if blocked is not None:
                controller.apply_outcome(blocked)
                self._record_outcome_context(context, planner, blocked)
                return self._terminal_result_from_outcome(blocked, turn=turn)
            repair_blocked = self._repair_phase_completion_blocked_outcome(
                planner,
                assessment=assessment,
            )
            if repair_blocked is not None:
                controller.apply_outcome(repair_blocked)
                self._record_outcome_context(context, planner, repair_blocked)
                return self._terminal_result_from_outcome(repair_blocked, turn=turn)
            return None
        if (
            planner.state is not None
            and not planner.state.completion_criteria.required_changes_applied
            and not planner.state.completion_criteria.required_verifications_passed
        ):
            outcome = ExecutionOutcome(
                status=ExecutionOutcomeStatus.SUCCESS,
                source="completion",
                reason="completion_ready",
                next_action="finalize",
                observation_summary="Completion evidence satisfied.",
                retry_allowed=False,
            )
            controller.apply_outcome(outcome)
            self._record_outcome_context(context, planner, outcome)
            self.trace.record("final_answer", {"turn": turn, "content": model_answer})
            return AgentLoopResult(
                status=AgentLoopStatus.COMPLETED,
                final_answer=model_answer,
                turn=turn,
            )
        report = planner.finalize()
        report.context_usage_diagnostic = context.context_usage_diagnostic()
        if report.status != TaskStatus.COMPLETED:
            retry_allowed = report.status in {
                TaskStatus.INSPECTING_WORKSPACE,
                TaskStatus.PLANNING_CHANGES,
                TaskStatus.APPLYING_CHANGES,
                TaskStatus.RUNNING_VERIFICATION,
                TaskStatus.REPAIRING_FAILURES,
                TaskStatus.FINALIZING,
            }
            outcome = ExecutionOutcome(
                status=(
                    ExecutionOutcomeStatus.REPLAN_REQUIRED
                    if retry_allowed
                    else ExecutionOutcomeStatus.BLOCKED
                ),
                source="completion",
                reason=f"Final report did not complete: {report.status.value}.",
                error_code="final_review_rejected",
                missing_evidence=list(report.next_steps or ["final_report_completed"]),
                next_action="continue" if retry_allowed else "blocked",
                observation_summary=(
                    f"Final report status={report.status.value}; "
                    f"verification={report.verification_summary.get('status', 'unknown')}."
                ),
                retry_allowed=retry_allowed,
                metadata={
                    "final_report_status": report.status.value,
                    "verification_summary": report.verification_summary,
                    "review_summary": report.review_summary,
                },
            )
            controller.apply_outcome(outcome)
            self._record_outcome_context(context, planner, outcome)
            blocked = self._maybe_analyze_failure(
                planner,
                context,
                outcome=outcome,
                failure_source="completion_review",
                turn=turn,
            )
            if blocked is not None:
                controller.apply_outcome(blocked)
                self._record_outcome_context(context, planner, blocked)
                return self._terminal_result_from_outcome(blocked, turn=turn)
            return self._terminal_result_from_outcome(outcome, turn=turn)
        final_answer = "\n".join(
            [
                f"status: {report.status.value}",
                f"files_changed: {', '.join(report.files_changed) if report.files_changed else '-'}",
                f"verification: {report.verification_summary.get('status', 'unknown')}",
                f"unresolved_issues: {len(report.unresolved_issues)}",
                f"risks: {len(report.risks)}",
            ]
        )
        outcome = ExecutionOutcome(
            status=ExecutionOutcomeStatus.SUCCESS,
            source="completion",
            reason="completion_ready",
            next_action="finalize",
            observation_summary="Completion evidence satisfied.",
            retry_allowed=False,
            metadata={"verification_summary": report.verification_summary},
        )
        controller.apply_outcome(outcome)
        self._record_outcome_context(context, planner, outcome)
        self.trace.record("final_answer", {"turn": turn, "content": final_answer})
        return AgentLoopResult(
            status=AgentLoopStatus.COMPLETED,
            final_answer=final_answer,
            turn=turn,
        )

    @staticmethod
    def _should_auto_finalize_after_tools(
        planner: Planner,
        protocol_result: Any,
    ) -> bool:
        state = planner.state
        if state is None:
            return False
        if state.status != TaskStatus.FINALIZING and state.current_phase != TaskStatus.FINALIZING.value:
            return False
        if int(getattr(protocol_result, "pending_approval_count", 0) or 0):
            return False
        if int(getattr(protocol_result, "failed_count", 0) or 0):
            return False
        return not int(getattr(protocol_result, "rejected_count", 0) or 0)

    def _outcome_from_model_failure(self, result: Any) -> ExecutionOutcome:
        message = (
            result.error.message
            if result.error is not None
            else ", ".join(result.validation.errors if result.validation else [])
        )
        retryable = bool(getattr(result.error, "retryable", False)) if result.error else True
        error_code = "model_runner_failed"
        lowered = message.lower()
        if "invalid_json" in lowered or "invalid json" in lowered:
            error_code = "invalid_json"
        elif "unknown_tool" in lowered or "unknown tool" in lowered:
            error_code = "unknown_tool"
        elif "schema" in lowered:
            error_code = "schema_mismatch"
        error_kind = getattr(result.error, "kind", None) if result.error else None
        blocked_external_dependency = (
            not retryable
            and error_kind in {ModelErrorKind.NETWORK_ERROR, ModelErrorKind.AUTH_ERROR}
        )
        return ExecutionOutcome(
            status=(
                ExecutionOutcomeStatus.RETRYABLE
                if retryable
                else (
                    ExecutionOutcomeStatus.BLOCKED
                    if blocked_external_dependency
                    else ExecutionOutcomeStatus.FATAL
                )
            ),
            source="model",
            reason=f"Model turn did not produce a valid response: {message}",
            error_code=error_code,
            next_action="retry" if retryable else ("blocked" if blocked_external_dependency else "abort"),
            observation_summary=message,
            retry_allowed=retryable,
            metadata={"model_status": result.status.value},
        )

    def _terminal_result_from_outcome(
        self,
        outcome: ExecutionOutcome,
        *,
        turn: int,
    ) -> AgentLoopResult | None:
        if outcome.status in {ExecutionOutcomeStatus.RETRYABLE, ExecutionOutcomeStatus.REPLAN_REQUIRED}:
            return None
        status = (
            AgentLoopStatus.FAILED
            if outcome.status == ExecutionOutcomeStatus.FATAL
            else AgentLoopStatus.BLOCKED
        )
        final_answer = outcome.observation_summary or outcome.reason
        self.trace.record("final_answer", {"turn": turn, "content": final_answer})
        return AgentLoopResult(
            status=status,
            final_answer=final_answer,
            turn=turn,
            error_code=outcome.error_code,
            diagnostics={"outcome": outcome.to_dict()},
        )

    def _record_outcome_context(
        self,
        context: ContextManager,
        planner: Planner,
        outcome: ExecutionOutcome,
    ) -> None:
        self.trace.record("execution_outcome", outcome.to_dict())
        context.add_planner_state(
            {
                "current_phase": planner.state.current_phase if planner.state else "unknown",
                "status": planner.state.status.value if planner.state else "unknown",
                "execution_outcome": outcome.to_dict(),
            }
        )

    def _context_db_path(self) -> Any:
        if hasattr(self.trace, "store"):
            return self.trace.store.run_dir / "context.sqlite3"
        return self.trace.path.parent / self.trace.run_id / "context.sqlite3"

    def _record_model_failure(
        self,
        planner: Planner,
        result: Any,
        *,
        turn: int,
    ) -> None:
        details = {
            "turn": turn,
            "status": result.status.value,
            "error": result.error.to_dict() if result.error else None,
            "validation": result.validation.to_dict() if result.validation else None,
        }
        self.trace.record("model_failure", details)

    def _maybe_analyze_failure(
        self,
        planner: Planner,
        context: ContextManager,
        *,
        failure_source: str,
        turn: int,
        outcome: ExecutionOutcome | None = None,
    ) -> ExecutionOutcome | None:
        if outcome is not None and not self._should_analyze_outcome(planner, outcome):
            return None
        if outcome is None and not self._has_repairable_planner_failure(planner):
            return None
        request = FailureAnalysisRequest.from_planner(
            planner,
            context,
            failure_source=failure_source,
            outcome=outcome,
            turn=turn,
        )
        if not request.has_failure:
            return None
        snapshot = self._failure_snapshot(planner)
        if request.fingerprint in self._failure_analysis_fingerprints:
            if not self._duplicate_failure_has_new_evidence(request.fingerprint, snapshot):
                if self._is_stalled_completion_gate_failure(
                    failure_source=failure_source,
                    outcome=outcome,
                ):
                    signal_payload = self._failure_replan_signals.get(request.fingerprint)
                    return ExecutionOutcome(
                        status=ExecutionOutcomeStatus.USER_INPUT_REQUIRED,
                        source="failure_analysis",
                        reason="Repeated completion/final review failure without new repair evidence.",
                        error_code="repair_budget_exceeded",
                        next_action="ask_user",
                        observation_summary=(
                            "Completion gate is still blocked after failure analysis and no new repair evidence."
                        ),
                        retry_allowed=False,
                        metadata={"replan_signal": signal_payload or {}},
                    )
                return None
            signal_payload = self._failure_replan_signals.get(request.fingerprint)
            if signal_payload is None:
                return None
            self._failure_analysis_snapshots[request.fingerprint] = snapshot
            decision = planner.replan(signal_payload)
            if getattr(getattr(decision, "decision", None), "value", "") == "ask_user":
                return ExecutionOutcome(
                    status=ExecutionOutcomeStatus.USER_INPUT_REQUIRED,
                    source="failure_analysis",
                    reason=decision.reason,
                    error_code="repair_budget_exceeded",
                    next_action="ask_user",
                    observation_summary=decision.reason,
                    retry_allowed=False,
                    metadata={"replan_signal": signal_payload},
                )
            return None
        self._failure_analysis_fingerprints.add(request.fingerprint)
        analysis = self.failure_analyzer.analyze(request)
        repair_plan = self.repair_planner.plan(analysis, repair_policy=request.repair_policy)
        replan_signal = self.repair_planner.to_replan_signal(
            request=request,
            analysis=analysis,
            plan=repair_plan,
        )
        replan_signal_payload = replan_signal.to_dict()
        planner.record_failure_analysis(
            analysis,
            repair_plan,
            replan_signal=replan_signal_payload,
        )
        context.add_failure(
            {
                "failure_analysis": analysis.to_dict(),
                "repair_plan": repair_plan.to_dict(),
                "replan_signal": replan_signal_payload,
            }
        )
        self._failure_analysis_snapshots[request.fingerprint] = snapshot
        if repair_plan.needs_user_input or repair_plan.blocked_reason:
            return self.repair_planner.blocked_outcome(repair_plan)
        self._failure_replan_signals[request.fingerprint] = replan_signal_payload
        decision = planner.replan(replan_signal_payload)
        if getattr(getattr(decision, "decision", None), "value", "") == "ask_user":
            return ExecutionOutcome(
                status=ExecutionOutcomeStatus.USER_INPUT_REQUIRED,
                source="failure_analysis",
                reason=decision.reason,
                error_code="repair_budget_exceeded",
                next_action="ask_user",
                observation_summary=decision.reason,
                retry_allowed=False,
                metadata={"repair_plan": repair_plan.to_dict(), "replan_signal": replan_signal_payload},
            )
        return None

    def _should_analyze_outcome(self, planner: Planner, outcome: ExecutionOutcome) -> bool:
        if outcome.status != ExecutionOutcomeStatus.REPLAN_REQUIRED:
            return False
        if outcome.error_code in {
            "approval_required",
            "approval_denied",
            "permission_denied",
            "policy_blocked",
            "policy_denied",
            "policy_ask_user_required",
            "action_not_allowed",
            "risk_escalated",
            "sandbox_required",
            "sandbox_capability_failed",
            "sandbox_violation",
            "policy_escalation_required",
        }:
            return False
        if outcome.error_code == "completion_rejected":
            return self._should_escalate_completion_rejection(planner, outcome)
        return True

    def _should_escalate_completion_rejection(self, planner: Planner, outcome: ExecutionOutcome) -> bool:
        missing = sorted(str(item) for item in outcome.missing_evidence)
        key = json.dumps({"missing": missing}, ensure_ascii=False, sort_keys=True)
        phase = getattr(getattr(planner, "state", None), "current_phase", "")
        snapshot = self._evidence_snapshot(planner)
        previous = self._completion_rejection_state.get("latest")
        if not previous or previous.get("key") != key:
            self._completion_rejection_state["latest"] = {
                "key": key,
                "count": 1,
                "phase": phase,
                "snapshot": snapshot,
            }
            return False
        count = int(previous.get("count") or 0) + 1
        phase_stalled = previous.get("phase") == phase
        evidence_stalled = previous.get("snapshot") == snapshot
        self._completion_rejection_state["latest"] = {
            "key": key,
            "count": count,
            "phase": phase,
            "snapshot": snapshot,
        }
        return count >= 2 and phase_stalled and evidence_stalled

    @staticmethod
    def _evidence_snapshot(planner: Planner) -> dict[str, int]:
        evidence = planner.evidence
        return {
            "inspected_files": len(evidence.inspected_files),
            "applied_changes": len(evidence.applied_changes),
            "command_results": len(evidence.command_results),
            "verification_results": len(evidence.verification_results),
            "tool_results": len(evidence.tool_results),
            "edit_results": len(evidence.edit_results),
            "review_results": len(evidence.review_results),
        }

    @staticmethod
    def _failure_snapshot(planner: Planner) -> dict[str, int]:
        evidence = planner.evidence
        return {
            "failed_command_results": len(
                [
                    item
                    for item in evidence.command_results
                    if item.get("semantic_status") not in {None, "succeeded", "SUCCEEDED"}
                    or item.get("error_code")
                ]
            ),
            "failed_verification_results": len(
                [
                    item
                    for item in evidence.verification_results
                    if isinstance(item, dict)
                    and (
                        (item.get("completion_assessment") or {}).get("status")
                        in {"failed", "blocked", "needs_review"}
                        or any(
                            result.get("status") in {"failed", "blocked", "timeout", "flaky"}
                            for result in item.get("results") or []
                            if isinstance(result, dict)
                        )
                    )
                ]
            ),
            "failed_tool_results": len(
                [
                    item
                    for item in evidence.tool_results
                    if item.get("ok") is False or item.get("error_code")
                ]
            ),
            "failed_edit_results": len(
                [
                    item
                    for item in evidence.edit_results
                    if item.get("error_code") or item.get("status") in {"failed", "blocked"}
                ]
            ),
            "failed_review_results": len(
                [
                    item
                    for item in evidence.review_results
                    if isinstance(item.get("decision"), dict)
                    and item["decision"].get("action") in {
                        "repair",
                        "reject",
                        "needs_human_approval",
                    }
                ]
            ),
        }

    @staticmethod
    def _is_stalled_completion_gate_failure(
        *,
        failure_source: str,
        outcome: ExecutionOutcome | None,
    ) -> bool:
        if failure_source in {"completion", "completion_review"}:
            return True
        return bool(
            outcome is not None
            and outcome.error_code in {"completion_rejected", "final_review_rejected"}
        )

    @staticmethod
    def _repair_phase_completion_blocked_outcome(
        planner: Planner,
        *,
        assessment: dict[str, Any],
    ) -> ExecutionOutcome | None:
        state = getattr(planner, "state", None)
        if getattr(state, "current_phase", "") != "repairing_failures":
            return None
        unmet = [str(item) for item in assessment.get("unmet") or []]
        if "verification_contract_satisfaction" not in unmet:
            return None
        return ExecutionOutcome(
            status=ExecutionOutcomeStatus.BLOCKED,
            source="completion",
            reason="Repair phase completion rejected because the active repair contract is unsatisfied.",
            error_code="repair_budget_exceeded",
            missing_evidence=unmet,
            next_action="blocked",
            observation_summary=(
                "Repair phase cannot complete because the active repair verification contract is unsatisfied."
            ),
            retry_allowed=False,
            metadata={"assessment": assessment},
        )

    def _duplicate_failure_has_new_evidence(self, fingerprint: str, snapshot: dict[str, int]) -> bool:
        previous = self._failure_analysis_snapshots.get(fingerprint)
        if previous is None:
            return True
        return any(snapshot.get(key, 0) > previous.get(key, 0) for key in snapshot)

    @staticmethod
    def _has_repairable_planner_failure(planner: Planner) -> bool:
        if planner.state is None:
            return False
        latest = planner.evidence.verification_results[-1] if planner.evidence.verification_results else {}
        assessment = latest.get("completion_assessment") if isinstance(latest, dict) else {}
        if isinstance(assessment, dict) and assessment.get("status") in {"ready", "ready_with_warnings"}:
            return False
        if isinstance(assessment, dict) and assessment.get("status") in {"failed", "blocked", "needs_review"}:
            return True
        for failure in planner.evidence.unresolved_failures[-5:]:
            if not isinstance(failure, dict):
                return True
            code = (
                failure.get("error_code")
                or (failure.get("execution_outcome") or {}).get("error_code")
                or failure.get("status")
            )
            if code not in {
                "approval_required",
                "approval_denied",
                "permission_denied",
                "policy_blocked",
                "policy_denied",
                "policy_ask_user_required",
                "action_not_allowed",
                "risk_escalated",
                "sandbox_required",
                "sandbox_capability_failed",
                "sandbox_violation",
                "policy_escalation_required",
            }:
                return True
        return False

    def _publish_progress(self, turn: int) -> None:
        if self.interaction_controller is None:
            return
        phase = (
            getattr(getattr(self.planner, "state", None), "current_phase", None)
            or "model"
        )
        self.interaction_controller.publish(
            ProgressEvent(
                phase=str(phase),
                status="started",
                summary=f"model turn {turn}",
                current=turn,
                total=self.max_turns,
                run_id=getattr(self.trace, "run_id", None),
                session_id=getattr(self.planner, "session_id", None),
                task_id=getattr(self.planner, "task_id", None),
                action_id=f"turn_{turn}",
            )
        )

    @staticmethod
    def _assistant_message_from_result(result: Any) -> dict[str, Any]:
        message = result.assistant_message
        assistant_message: dict[str, Any] = {
            "role": "assistant",
            "content": message.text if message is not None else "",
        }
        if result.tool_calls:
            assistant_message["tool_calls"] = [
                tool_call.to_provider_tool_call() for tool_call in result.tool_calls
            ]
            if not assistant_message["content"]:
                assistant_message["content"] = None
        return assistant_message
