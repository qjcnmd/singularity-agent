from __future__ import annotations

from pathlib import Path
from dataclasses import dataclass
from enum import Enum
from typing import Any

from rich.console import Console

from singularity.context import ContextManager
from singularity.execution_outcome import ExecutionOutcome, ExecutionOutcomeStatus
from singularity.instructions import PromptAssemblyPipeline
from singularity.interaction import InteractionController, ProgressEvent
from singularity.model import ModelErrorKind, ModelPurpose, ModelRunner, ModelTurnStatus
from singularity.planner import Planner, TaskStatus
from singularity.provider import OpenAICompatibleProvider
from singularity.policy import PolicyEngine
from singularity.run_controller import RunController
from singularity.tool_protocol.engine import ToolProtocolEngine
from singularity.tools import ToolRegistry, ToolExecutor
from singularity.observability.protocols import TraceStorageProtocol


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


class AgentLoopStatus(str, Enum):
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
        policy_engine: PolicyEngine | None = None,
        tool_executor: ToolExecutor,
        tool_protocol: ToolProtocolEngine,
        prompt_assembly: PromptAssemblyPipeline,
        interaction_controller: InteractionController | None = None,
        context_manager: ContextManager | None = None,
        context_db_path: Path | None = None,
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
        self.policy_engine = policy_engine
        self.tool_executor = tool_executor
        self.tool_protocol = tool_protocol
        self.prompt_assembly = prompt_assembly
        self.interaction_controller = interaction_controller
        self.context_manager = context_manager
        self.context_db_path = context_db_path
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
                terminal = self._terminal_result_from_outcome(reduced_outcome, turn=turn)
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
        if int(getattr(protocol_result, "rejected_count", 0) or 0):
            return False
        return True

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
