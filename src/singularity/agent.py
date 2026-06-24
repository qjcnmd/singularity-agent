from __future__ import annotations

from pathlib import Path
from dataclasses import dataclass
from enum import Enum
from typing import Any

from rich.console import Console

from singularity.context import ContextManager
from singularity.execution_outcome import ExecutionOutcome, ExecutionOutcomeStatus
from singularity.instructions import InstructionRuntime
from singularity.interaction import InteractionRuntime, ProgressEvent
from singularity.model import ModelErrorKind, ModelPurpose, ModelRuntime, ModelTurnStatus
from singularity.planner import PlannerRuntime, TaskStatus
from singularity.provider import OpenAICompatibleProvider
from singularity.policy import PolicyRuntime
from singularity.task_controller import TaskController
from singularity.tool_protocol.runtime import ToolCallingProtocolRuntime
from singularity.tools import ToolRegistry, ToolRuntime
from singularity.trace import TraceWriter


SYSTEM_PROMPT = """You are Singularity, a local coding agent runtime.

Use only the tools exposed in the current request schema. The runtime provides
a per-turn tool protocol summary with the available tools and preferred runtime
paths.

Never claim that you inspected, edited, or verified anything unless the
corresponding runtime tool returned evidence. File mutations must go through
the exposed EditRuntime or patch tools. Verification behavior such as tests,
lint, typecheck, builds, syntax checks, and smoke checks must use the exposed
VerificationRuntime tools instead of ad-hoc command tools.
Never claim a coding task is complete unless the latest VerificationRuntime
CompletionAssessment says it is ready or ready_with_warnings, and report any
warnings or remaining risks.
Do not claim that you browsed the web, stored memory, or contacted other agents.
When you have enough information, answer the user directly.
"""


class SingularityAgentRunStatus(str, Enum):
    COMPLETED = "completed"
    BLOCKED = "blocked"
    MAX_TURNS_EXCEEDED = "max_turns_exceeded"
    FAILED = "failed"


@dataclass(frozen=True, eq=False)
class SingularityAgentRunResult:
    status: SingularityAgentRunStatus
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
        if isinstance(other, SingularityAgentRunResult):
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


class SingularityAgent:
    def __init__(
        self,
        *,
        provider: OpenAICompatibleProvider | None = None,
        model_runtime: ModelRuntime,
        tools: ToolRegistry,
        trace: TraceWriter,
        console: Console,
        max_turns: int,
        planner: PlannerRuntime,
        policy_runtime: PolicyRuntime | None = None,
        tool_runtime: ToolRuntime,
        protocol_runtime: ToolCallingProtocolRuntime,
        instruction_runtime: InstructionRuntime,
        interaction_runtime: InteractionRuntime | None = None,
        context_manager: ContextManager | None = None,
        context_db_path: Path | None = None,
        strict: bool = False,
    ) -> None:
        if model_runtime is None:
            raise ValueError("model_runtime is required; SingularityAgent does not assemble model runtimes.")
        if planner is None:
            raise ValueError("planner is required; SingularityAgent does not assemble PlannerRuntime.")
        if tool_runtime is None:
            raise ValueError("tool_runtime is required; SingularityAgent does not assemble ToolRuntime.")
        if protocol_runtime is None:
            raise ValueError(
                "protocol_runtime is required; SingularityAgent does not assemble ToolCallingProtocolRuntime."
            )
        if instruction_runtime is None:
            raise ValueError(
                "instruction_runtime is required; SingularityAgent does not assemble InstructionRuntime."
            )
        self.provider = provider
        self.model_runtime = model_runtime
        self.tools = tools
        self.trace = trace
        self.console = console
        self.max_turns = max_turns
        self.planner = planner
        self.policy_runtime = policy_runtime
        self.tool_runtime = tool_runtime
        self.protocol_runtime = protocol_runtime
        self.instruction_runtime = instruction_runtime
        self.interaction_runtime = interaction_runtime
        self.context_manager = context_manager
        self.context_db_path = context_db_path
        self.strict = strict

    def run(self, user_goal: str) -> SingularityAgentRunResult:
        planner = self.planner
        controller = TaskController(planner=planner, trace=self.trace)
        if planner.state is None:
            controller.start(user_goal)
        effective_goal = getattr(planner.state, "effective_goal", None) or user_goal
        context = self.context_manager
        if context is None:
            context = ContextManager(
                system_prompt=SYSTEM_PROMPT,
                user_goal=effective_goal,
                provider=self.provider,
                model_runtime=self.model_runtime,
                run_id=self.trace.run_id,
                session_id=getattr(planner, "session_id", self.trace.run_id),
                task_id=getattr(planner, "task_id", self.trace.run_id),
                db_path=self.context_db_path or self._context_db_path(),
                trace=self.trace,
            )
        else:
            context.set_user_goal(effective_goal)
        model_runtime = self.model_runtime
        instruction_runtime = self.instruction_runtime
        tool_schemas = self.tools.openai_tools(strict=self.strict)
        runtime = self.tool_runtime
        protocol_runtime = self.protocol_runtime

        def run_turn(turn: int) -> SingularityAgentRunResult | None:
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
            request = model_runtime.build_request_from_context(
                context,
                run_id=self.trace.run_id,
                session_id=getattr(planner, "session_id", self.trace.run_id),
                task_id=getattr(planner, "task_id", self.trace.run_id),
                phase_id=planner.state.current_phase if planner.state else "model",
                action_id=f"turn_{turn}",
                purpose=ModelPurpose.PLAN_NEXT_ACTION,
                allowed_tool_names=allowed_tool_names,
                planner_context=planner.planner_context_message(),
                instruction_runtime=instruction_runtime,
                user_task=effective_goal,
                strict_tools=self.strict,
            )
            planner.record_instruction_prompt_observation(instruction_runtime.summary())
            result = model_runtime.run_turn(request)
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
            protocol_result = protocol_runtime.process_model_turn(
                request=request,
                result=result,
                turn=turn,
                context=context,
                tool_runtime=runtime,
                planner=planner,
                policy_runtime=self.policy_runtime,
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

            controller.apply_protocol_result(protocol_result)
            outcome = self._reduce_protocol_result(
                protocol_result,
                context=context,
                observation_start=observation_start,
            )
            if outcome is not None:
                controller.apply_outcome(outcome)
                self._record_outcome_context(context, planner, outcome)
                terminal = self._terminal_result_from_outcome(outcome, turn=turn)
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

        def on_max_turns(max_turns: int) -> SingularityAgentRunResult:
            message = f"Stopped after max_turns={max_turns}; the model did not produce a final answer."
            self.trace.record("error", {"type": "MaxTurnsExceeded", "message": message})
            self.trace.record("final_answer", {"turn": max_turns, "content": message})
            return SingularityAgentRunResult(
                status=SingularityAgentRunStatus.MAX_TURNS_EXCEEDED,
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
        planner: PlannerRuntime,
        *,
        controller: TaskController,
        context: ContextManager,
        turn: int,
        model_answer: str,
    ) -> SingularityAgentRunResult | None:
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
            return SingularityAgentRunResult(
                status=SingularityAgentRunStatus.COMPLETED,
                final_answer=model_answer,
                turn=turn,
            )
        report = planner.finalize()
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
        return SingularityAgentRunResult(
            status=SingularityAgentRunStatus.COMPLETED,
            final_answer=final_answer,
            turn=turn,
        )

    @staticmethod
    def _should_auto_finalize_after_tools(
        planner: PlannerRuntime,
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
        if int(getattr(protocol_result, "rejected_count", 0) or 0):
            return False
        return True

    def _reduce_protocol_result(
        self,
        protocol_result: Any,
        *,
        context: ContextManager,
        observation_start: int,
    ) -> ExecutionOutcome | None:
        observations = context.tool_observations[observation_start:]
        error_codes = [
            str(observation.error_code)
            for observation in observations
            if getattr(observation, "error_code", None)
        ]
        next_action = str(getattr(protocol_result, "next_action", "") or "continue")
        status = str(getattr(getattr(protocol_result, "status", None), "value", getattr(protocol_result, "status", "")))
        failed_count = int(getattr(protocol_result, "failed_count", 0) or 0)
        rejected_count = int(getattr(protocol_result, "rejected_count", 0) or 0)
        pending_count = int(getattr(protocol_result, "pending_approval_count", 0) or 0)
        summary = self._observation_summary(observations, protocol_result)

        if pending_count or next_action == "pending_approval" or "approval_required" in error_codes:
            return ExecutionOutcome(
                status=ExecutionOutcomeStatus.APPROVAL_REQUIRED,
                source="protocol",
                reason="Tool execution is waiting for approval.",
                error_code="approval_required",
                next_action="wait_for_approval",
                observation_summary=summary,
                retry_allowed=False,
            )
        if "policy_ask_user_required" in error_codes:
            return ExecutionOutcome(
                status=ExecutionOutcomeStatus.USER_INPUT_REQUIRED,
                source="tool",
                reason="Policy requires user input.",
                error_code="policy_ask_user_required",
                next_action="ask_user",
                observation_summary=summary,
                retry_allowed=False,
            )
        blocked_codes = {
            "policy_denied",
            "approval_denied",
            "action_not_allowed",
            "risk_escalated",
            "sandbox_required",
            "policy_escalation_required",
        }
        if any(code in blocked_codes for code in error_codes):
            code = next(code for code in error_codes if code in blocked_codes)
            return ExecutionOutcome(
                status=ExecutionOutcomeStatus.BLOCKED,
                source="tool",
                reason=f"Tool execution blocked: {code}.",
                error_code=code,
                next_action="blocked",
                observation_summary=summary,
                retry_allowed=False,
            )
        replan_codes = {
            "snapshot_mismatch",
            "external_change_detected",
            "file_changed",
            "rollback_conflict",
            "semantic_failure",
            "verification_failed",
            "blocked_by_verification",
            "command_not_found",
            "timeout",
        }
        if any(code in replan_codes for code in error_codes):
            code = next(code for code in error_codes if code in replan_codes)
            return ExecutionOutcome(
                status=ExecutionOutcomeStatus.REPLAN_REQUIRED,
                source="tool",
                reason=f"Tool result requires replanning: {code}.",
                error_code=code,
                next_action="replan",
                observation_summary=summary,
                retry_allowed=True,
            )
        retryable_codes = {
            "bad_arguments_json",
            "invalid_json",
            "arguments_not_object",
            "validation_error",
            "schema_mismatch",
            "unknown_tool",
            "tool_not_found",
            "disallowed_tool",
            "protocol_violation",
            "internal_error",
        }
        if any(code in retryable_codes for code in error_codes):
            code = next(code for code in error_codes if code in retryable_codes)
            return ExecutionOutcome(
                status=ExecutionOutcomeStatus.RETRYABLE,
                source="protocol" if "json" in code or "schema" in code else "tool",
                reason=f"Tool call can be retried after correction: {code}.",
                error_code=code,
                next_action="retry",
                observation_summary=summary,
                retry_allowed=True,
            )
        if next_action == "fail_safe" or status in {"failed", "invalid_assistant"}:
            return ExecutionOutcome(
                status=ExecutionOutcomeStatus.RETRYABLE,
                source="protocol",
                reason="Protocol fail-safe requested another model turn.",
                error_code=str((getattr(protocol_result, "metadata", {}) or {}).get("reason") or "protocol_fail_safe"),
                next_action="retry",
                observation_summary=summary,
                retry_allowed=True,
            )
        if failed_count or rejected_count or next_action == "recover":
            return ExecutionOutcome(
                status=ExecutionOutcomeStatus.RETRYABLE,
                source="protocol",
                reason="Protocol reported recoverable tool failure.",
                error_code=error_codes[0] if error_codes else "tool_failure",
                next_action="retry",
                observation_summary=summary,
                retry_allowed=True,
            )
        return None

    def _outcome_from_model_failure(self, result: Any) -> ExecutionOutcome:
        message = (
            result.error.message
            if result.error is not None
            else ", ".join(result.validation.errors if result.validation else [])
        )
        retryable = bool(getattr(result.error, "retryable", False)) if result.error else True
        error_code = "model_runtime_failed"
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
    ) -> SingularityAgentRunResult | None:
        if outcome.status in {ExecutionOutcomeStatus.RETRYABLE, ExecutionOutcomeStatus.REPLAN_REQUIRED}:
            return None
        status = (
            SingularityAgentRunStatus.FAILED
            if outcome.status == ExecutionOutcomeStatus.FATAL
            else SingularityAgentRunStatus.BLOCKED
        )
        final_answer = outcome.observation_summary or outcome.reason
        self.trace.record("final_answer", {"turn": turn, "content": final_answer})
        return SingularityAgentRunResult(
            status=status,
            final_answer=final_answer,
            turn=turn,
            error_code=outcome.error_code,
            diagnostics={"outcome": outcome.to_dict()},
        )

    def _record_outcome_context(
        self,
        context: ContextManager,
        planner: PlannerRuntime,
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

    @staticmethod
    def _observation_summary(observations: list[Any], protocol_result: Any) -> str:
        if observations:
            parts = []
            for observation in observations[-3:]:
                status = "ok" if observation.ok else (observation.error_code or "failed")
                preview = str(observation.preview or "").replace("\n", " ")[:160]
                parts.append(f"{observation.tool_name}:{status}:{preview}")
            return "; ".join(parts)
        return (
            f"protocol next_action={getattr(protocol_result, 'next_action', None)} "
            f"status={getattr(getattr(protocol_result, 'status', None), 'value', getattr(protocol_result, 'status', None))}"
        )

    def _context_db_path(self) -> Any:
        if hasattr(self.trace, "store"):
            return self.trace.store.run_dir / "context.sqlite3"
        return self.trace.path.parent / self.trace.run_id / "context.sqlite3"

    def _record_model_failure(
        self,
        planner: PlannerRuntime,
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

    def _publish_progress(self, turn: int) -> None:
        if self.interaction_runtime is None:
            return
        phase = (
            getattr(getattr(self.planner, "state", None), "current_phase", None)
            or "model"
        )
        self.interaction_runtime.publish(
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

    @staticmethod
    def _extract_assistant_message(response: dict[str, Any]) -> dict[str, Any]:
        choices = response.get("choices") or []
        if not choices:
            raise ValueError("Model response did not include choices.")

        message = choices[0].get("message") or {}
        if message.get("role") != "assistant":
            message["role"] = "assistant"

        assistant_message: dict[str, Any] = {
            "role": "assistant",
            "content": message.get("content"),
        }
        if message.get("tool_calls"):
            assistant_message["tool_calls"] = message["tool_calls"]
        return assistant_message
