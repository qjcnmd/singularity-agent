from __future__ import annotations

from dataclasses import dataclass
from enum import StrEnum
from pathlib import Path
from typing import Any

from rich.console import Console

from singularity.agent_loop_completion import CompletionGate
from singularity.agent_loop_failure_recovery import FailureRecoveryCoordinator
from singularity.agent_loop_turns import (
    TurnCoordinator,
    TurnCoordinatorCallbacks,
    TurnRuntimeDependencies,
)
from singularity.context import ContextManager
from singularity.error_codes import ErrorCode
from singularity.execution_outcome import ExecutionOutcome, ExecutionOutcomeStatus
from singularity.failure_analysis.analyzer import FailureAnalyzer
from singularity.instructions import PromptAssemblyPipeline
from singularity.interaction import InteractionController, ProgressEvent
from singularity.model import (
    ChatCompletionProvider,
    ModelErrorKind,
    ModelRunner,
)
from singularity.observability.protocols import TraceStorageProtocol
from singularity.planner import Planner
from singularity.repair import RepairPlanner
from singularity.run_controller import RunController
from singularity.tool_protocol.engine import ToolProtocolEngine
from singularity.tools import ToolExecutor, ToolRegistry
from singularity.utils.attributes import nested_getattr

SYSTEM_PROMPT = """You are Singularity, a local coding agent harness.

Use only the tools exposed in the current request schema. The agent loop records
tool protocol metadata outside model-visible instructions.

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
    MAX_TURNS_EXCEEDED = ErrorCode.MAX_TURNS_EXCEEDED.value
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
        provider: ChatCompletionProvider | None = None,
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
        self.failure_recovery = FailureRecoveryCoordinator(
            failure_analyzer=self.failure_analyzer,
            repair_planner=self.repair_planner,
            failure_analysis_fingerprints=self._failure_analysis_fingerprints,
            failure_replan_signals=self._failure_replan_signals,
            failure_analysis_snapshots=self._failure_analysis_snapshots,
            completion_rejection_state=self._completion_rejection_state,
        )
        self.completion_gate = self._build_completion_gate()
        self.strict = strict

    def _build_completion_gate(self) -> CompletionGate:
        return CompletionGate(
            trace=self.trace,
            result_factory=lambda **kwargs: AgentLoopResult(
                status=AgentLoopStatus(kwargs["status"]),
                final_answer=kwargs["final_answer"],
                turn=kwargs["turn"],
                error_code=kwargs.get("error_code"),
                diagnostics=kwargs.get("diagnostics"),
            ),
            terminal_result_from_outcome=self._terminal_result_from_outcome,
            record_outcome_context=self._record_outcome_context,
            maybe_analyze_failure=self._maybe_analyze_failure,
        )

    def _completion_gate(self) -> CompletionGate:
        gate = getattr(self, "completion_gate", None)
        if isinstance(gate, CompletionGate):
            return gate
        gate = self._build_completion_gate()
        self.completion_gate = gate
        return gate

    def _turn_coordinator(self) -> TurnCoordinator:
        coordinator = getattr(self, "turn_coordinator", None)
        if isinstance(coordinator, TurnCoordinator):
            return coordinator
        coordinator = TurnCoordinator(
            trace=self.trace,
            dependencies=TurnRuntimeDependencies(
                model_runner=self.model_runner,
                tools=self.tools,
                tool_executor=self.tool_executor,
                tool_protocol=self.tool_protocol,
            ),
            prompt_assembly=self.prompt_assembly,
            strict=self.strict,
            callbacks=TurnCoordinatorCallbacks(
                publish_progress=self._publish_progress,
                record_model_failure=self._record_model_failure,
                outcome_from_model_failure=self._outcome_from_model_failure,
                terminal_result_from_outcome=self._terminal_result_from_outcome,
                record_outcome_context=self._record_outcome_context,
                assistant_message_from_result=self._assistant_message_from_result,
                attempt_finalize=self._attempt_finalize,
                maybe_analyze_failure=self._maybe_analyze_failure,
                should_auto_finalize_after_tools=self._should_auto_finalize_after_tools,
            ),
        )
        self.turn_coordinator = coordinator
        return coordinator

    def _failure_recovery_coordinator(self) -> FailureRecoveryCoordinator:
        coordinator = getattr(self, "failure_recovery", None)
        if isinstance(coordinator, FailureRecoveryCoordinator):
            return coordinator
        coordinator = FailureRecoveryCoordinator(
            failure_analyzer=getattr(self, "failure_analyzer", None),
            repair_planner=getattr(self, "repair_planner", None),
            failure_analysis_fingerprints=getattr(self, "_failure_analysis_fingerprints", None),
            failure_replan_signals=getattr(self, "_failure_replan_signals", None),
            failure_analysis_snapshots=getattr(self, "_failure_analysis_snapshots", None),
            completion_rejection_state=getattr(self, "_completion_rejection_state", None),
        )
        self.failure_recovery = coordinator
        return coordinator

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
        tool_schemas = self.tools.openai_tools(strict=self.strict)

        def run_turn(turn: int) -> AgentLoopResult | None:
            return self._turn_coordinator().run_turn(
                turn,
                user_goal=user_goal,
                planner=planner,
                controller=controller,
                context=context,
                tool_schemas=tool_schemas,
            )

        def on_max_turns(max_turns: int) -> AgentLoopResult:
            message = f"Stopped after max_turns={max_turns}; the model did not produce a final answer."
            outcome = ExecutionOutcome(
                status=ExecutionOutcomeStatus.BLOCKED,
                source="agent_loop",
                reason=message,
                error_code=ErrorCode.MAX_TURNS_EXCEEDED.value,
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
                error_code=ErrorCode.MAX_TURNS_EXCEEDED.value,
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
        return self._completion_gate().attempt_finalize(
            planner,
            controller=controller,
            context=context,
            turn=turn,
            model_answer=model_answer,
        )

    @staticmethod
    def _should_auto_finalize_after_tools(
        planner: Planner,
        protocol_result: Any,
    ) -> bool:
        return CompletionGate.should_auto_finalize_after_tools(planner, protocol_result)

    def _outcome_from_model_failure(self, result: Any) -> ExecutionOutcome:
        message = (
            result.error.message
            if result.error is not None
            else ", ".join(result.validation.errors if result.validation else [])
        )
        retryable = bool(getattr(result.error, "retryable", False)) if result.error else True
        error_code = ErrorCode.MODEL_RUNNER_FAILED.value
        lowered = message.lower()
        if "invalid_json" in lowered or "invalid json" in lowered:
            error_code = ErrorCode.INVALID_JSON.value
        elif "unknown_tool" in lowered or "unknown tool" in lowered:
            error_code = ErrorCode.UNKNOWN_TOOL.value
        elif "schema" in lowered:
            error_code = ErrorCode.SCHEMA_MISMATCH.value
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
        return self._failure_recovery_coordinator().maybe_analyze_failure(
            planner,
            context,
            failure_source=failure_source,
            outcome=outcome,
            turn=turn,
        )

    def _should_analyze_outcome(self, planner: Planner, outcome: ExecutionOutcome) -> bool:
        return self._failure_recovery_coordinator().should_analyze_outcome(planner, outcome)

    def _should_escalate_completion_rejection(self, planner: Planner, outcome: ExecutionOutcome) -> bool:
        return self._failure_recovery_coordinator().should_escalate_completion_rejection(
            planner,
            outcome,
        )

    @staticmethod
    def _evidence_snapshot(planner: Planner) -> dict[str, int]:
        return FailureRecoveryCoordinator.evidence_snapshot(planner)

    @staticmethod
    def _failure_snapshot(planner: Planner) -> dict[str, int]:
        return FailureRecoveryCoordinator.failure_snapshot(planner)

    @staticmethod
    def _is_stalled_completion_gate_failure(
        *,
        failure_source: str,
        outcome: ExecutionOutcome | None,
    ) -> bool:
        return FailureRecoveryCoordinator.is_stalled_completion_gate_failure(
            failure_source=failure_source,
            outcome=outcome,
        )

    @staticmethod
    def _repair_phase_completion_blocked_outcome(
        planner: Planner,
        *,
        assessment: dict[str, Any],
    ) -> ExecutionOutcome | None:
        return CompletionGate.repair_phase_completion_blocked_outcome(
            planner,
            assessment=assessment,
        )

    def _duplicate_failure_has_new_evidence(self, fingerprint: str, snapshot: dict[str, int]) -> bool:
        return self._failure_recovery_coordinator().duplicate_failure_has_new_evidence(
            fingerprint,
            snapshot,
        )

    @staticmethod
    def _has_repairable_planner_failure(planner: Planner) -> bool:
        return FailureRecoveryCoordinator.has_repairable_planner_failure(planner)

    def _publish_progress(self, turn: int) -> None:
        if self.interaction_controller is None:
            return
        phase = nested_getattr(self.planner, "state.current_phase") or "model"
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
