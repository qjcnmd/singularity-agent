from __future__ import annotations

from collections.abc import Callable
from pathlib import Path
from typing import Any

from singularity.context import ContextManager
from singularity.kernel.cancellation import throw_if_cancelled
from singularity.model import ModelCapabilities, ModelRole, ModelTurnResult
from singularity.planner import Planner
from singularity.tool_protocol.binding import ToolProtocolResultBinder
from singularity.tool_protocol.context_projection import ToolProtocolContextProjector
from singularity.tool_protocol.executor import (
    ToolProtocolPlanExecutor,
    build_tool_protocol_turn_result,
)
from singularity.tool_protocol.models import (
    ToolCallBatch,
    ToolCallEnvelope,
    ToolCallFailureKind,
    ToolExecutionPlan,
    ToolProtocolResultEnvelope,
    ToolProtocolTurnResult,
    ToolProtocolTurnStatus,
    ToolProtocolValidationResult,
)
from singularity.tool_protocol.parallel import ParallelToolExecutor
from singularity.tool_protocol.recovery import ToolProtocolRecoveryManager
from singularity.tool_protocol.result import ToolProtocolResultBuilder
from singularity.tool_protocol.scheduler import ToolProtocolScheduler
from singularity.tool_protocol.state import ToolProtocolStateStore
from singularity.tool_protocol.synthetic_result import ToolProtocolSyntheticResultFactory
from singularity.tool_protocol.trace import ToolProtocolTrace
from singularity.tool_protocol.transitions import ToolProtocolStateTransitioner
from singularity.tool_protocol.validator import ToolProtocolValidator
from singularity.tools import ToolExecutor, ToolRegistry


class ToolProtocolEngine:
    def __init__(
        self,
        *,
        registry: ToolRegistry,
        trace: Any | None = None,
        state_store: ToolProtocolStateStore | None = None,
        workspace_state_hook: Callable[..., None] | None = None,
        result_builder: ToolProtocolResultBuilder | None = None,
        parallel_executor: ParallelToolExecutor | None = None,
    ) -> None:
        self.registry = registry
        self.trace = ToolProtocolTrace(trace)
        default_state_path = _default_state_path(registry, trace)
        self.state_store = state_store or ToolProtocolStateStore(default_state_path)
        self.validator = ToolProtocolValidator(registry)
        self.scheduler = ToolProtocolScheduler(registry)
        self.result_builder = result_builder or ToolProtocolResultBuilder()
        self.synthetic_results = ToolProtocolSyntheticResultFactory()
        self.context_projector = ToolProtocolContextProjector(self.state_store)
        self.state_transitions = ToolProtocolStateTransitioner(self.state_store)
        self.result_binder = ToolProtocolResultBinder(self.state_store, self.context_projector)
        self.parallel_executor = parallel_executor or ParallelToolExecutor()
        self.plan_executor = ToolProtocolPlanExecutor(
            registry=self.registry,
            state_transitions=self.state_transitions,
            result_binder=self.result_binder,
            parallel_executor=self.parallel_executor,
            result_builder=self.result_builder,
            synthetic_results=self.synthetic_results,
            trace=self.trace,
            batch_for_plan=self._batch_for_plan,
            turn_result_factory=build_tool_protocol_turn_result,
            trace_ids_factory=self._trace_ids,
            call_trace_ids_factory=self._call_trace_ids,
            throw_if_cancelled=lambda: throw_if_cancelled(self),
        )
        self.recovery_manager = ToolProtocolRecoveryManager(self.state_store)
        self.workspace_state_hook = workspace_state_hook
        self.cancellation_token: Any | None = None

    def close(self) -> None:
        self.state_store.close()

    def process_model_turn(
        self,
        *,
        request: Any | None = None,
        result: ModelTurnResult,
        turn: int = 0,
        context: ContextManager,
        tool_executor: ToolExecutor,
        planner: Planner | None = None,
    ) -> ToolProtocolTurnResult:
        return self.handle_model_turn_result(
            result,
            request=request,
            turn=turn,
            context=context,
            tool_executor=tool_executor,
            planner=planner,
        )

    def handle_model_turn_result(
        self,
        model_result: ModelTurnResult,
        *,
        request: Any | None = None,
        turn: int = 0,
        context: ContextManager,
        tool_executor: ToolExecutor,
        planner: Planner | None,
    ) -> ToolProtocolTurnResult:
        throw_if_cancelled(self)
        assistant_message = self._assistant_message_from_model_result(model_result)
        if assistant_message is None:
            return ToolProtocolTurnResult(
                status=ToolProtocolTurnStatus.INVALID_ASSISTANT,
                next_action="fail_safe",
                metadata={"reason": "missing_assistant_message"},
            )

        validation = self.validate_batch(
            model_result,
            request=request,
            context=context,
            assistant_message=assistant_message,
            tool_executor=tool_executor,
            planner=planner,
        )
        batch = validation.batch
        if batch is None:
            return ToolProtocolTurnResult(
                status=ToolProtocolTurnStatus.INVALID_ASSISTANT,
                next_action="fail_safe",
                metadata={"reason": "invalid_batch"},
            )

        self.state_store.save_batch(batch)
        throw_if_cancelled(self)
        self.trace.emit(
            "tool_protocol.batch_created",
            summary="Tool protocol batch created.",
            payload={
                "batch_id": batch.batch_id,
                "tool_call_count": len(batch.tool_calls),
                "batch_digest": batch.batch_digest,
            },
            ids=self._trace_ids(batch),
        )
        context.add_assistant_message(assistant_message)

        if not batch.tool_calls:
            return ToolProtocolTurnResult(
                status=ToolProtocolTurnStatus.NO_TOOL_CALLS,
                batch_id=batch.batch_id,
                executed_count=0,
                failed_count=0,
                rejected_count=0,
                pending_approval_count=0,
                appended_tool_message_count=0,
                next_action="finalize",
            )

        plan = self.build_execution_plan(batch, validation=validation)
        throw_if_cancelled(self)
        self.trace.emit(
            "tool_protocol.plan_built",
            summary="Tool protocol execution plan built.",
            payload={
                "plan_id": plan.plan_id,
                "batch_id": plan.batch_id,
                "execution_mode": plan.execution_mode.value,
                "ordered_call_ids": [call.tool_call_id for call in plan.ordered_calls],
                "blocked_call_ids": [call.tool_call_id for call in plan.blocked_calls],
                "reason_count": len(plan.reasons),
            },
            ids=self._trace_ids(batch),
        )
        execution = self.execute_plan(
            plan,
            context=context,
            tool_executor=tool_executor,
            planner=planner,
            turn=turn,
        )
        throw_if_cancelled(self)
        if self.workspace_state_hook is not None:
            self._inject_workspace_state(
                context,
                batch=batch,
                tool_call_id=execution.metadata.get("last_tool_call_id"),
            )
        return execution

    def validate_batch(
        self,
        model_result: ModelTurnResult,
        *,
        request: Any | None = None,
        context: ContextManager,
        assistant_message: dict[str, Any] | None = None,
        tool_executor: ToolExecutor | None = None,
        planner: Planner | None = None,
    ) -> ToolProtocolValidationResult:
        _ = planner
        assistant_message = assistant_message or self._assistant_message_dict(model_result)
        return self.validator.validate_assistant_message(
            run_id=context.run_id,
            session_id=context.session_id,
            task_id=context.task_id,
            phase_id=context.phase_id,
            model_request_id=model_result.request_id,
            model_response_id=model_result.response_id,
            assistant_message=assistant_message,
            assistant_message_id=model_result.response_id,
            allowed_tool_names=_allowed_tool_names_from_request(request),
            tool_choice=getattr(request, "tool_choice", None),
            provider_capabilities=_provider_capabilities_from_result(model_result),
            max_tool_calls=None,
        )

    def build_execution_plan(
        self,
        batch: ToolCallBatch,
        *,
        validation: ToolProtocolValidationResult | None = None,
    ) -> ToolExecutionPlan:
        _ = validation
        return self.scheduler.schedule(batch)

    def execute_plan(
        self,
        plan: ToolExecutionPlan,
        *,
        context: ContextManager,
        tool_executor: ToolExecutor,
        planner: Planner | None,
        turn: int = 0,
    ) -> ToolProtocolTurnResult:
        return self.plan_executor.execute(
            plan,
            context=context,
            tool_executor=tool_executor,
            planner=planner,
            turn=turn,
        )

    def _batch_for_plan(
        self,
        plan: ToolExecutionPlan,
        context: ContextManager,
    ) -> ToolCallBatch:
        return self.state_store.batch_by_assistant_message_id(plan.batch_id) or ToolCallBatch(
            batch_id=plan.batch_id,
            run_id=context.run_id,
            session_id=context.session_id,
            task_id=context.task_id,
            phase_id=context.phase_id,
            model_request_id=plan.batch_id,
            model_response_id=plan.batch_id,
            assistant_message={"role": "assistant", "content": None, "tool_calls": []},
        )

    def append_results_to_context(
        self,
        context: ContextManager,
        *,
        envelope: ToolCallEnvelope,
        result: ToolProtocolResultEnvelope,
        turn: int = 0,
    ) -> str | None:
        return self.context_projector.append_result(
            context,
            envelope=envelope,
            result=result,
            turn=turn,
        )

    def recover_pending(
        self,
        *,
        context: ContextManager | None = None,
        run_id: str | None = None,
        session_id: str | None = None,
        task_id: str | None = None,
    ) -> ToolProtocolTurnResult:
        resolved_run_id = run_id or (context.run_id if context is not None else "")
        resolved_session_id = session_id or (context.session_id if context is not None else None)
        resolved_task_id = task_id or (context.task_id if context is not None else None)
        return self.recovery_manager.recover(
            run_id=resolved_run_id,
            session_id=resolved_session_id,
            task_id=resolved_task_id,
        )

    def _inject_workspace_state(
        self,
        context: ContextManager,
        *,
        batch: ToolCallBatch,
        tool_call_id: str | None,
    ) -> None:
        hook = self.workspace_state_hook
        if hook is None:
            return
        hook(
            context,
            batch=batch,
            tool_call_id=tool_call_id,
        )

    def _assistant_message_from_model_result(self, model_result: ModelTurnResult) -> dict[str, Any] | None:
        message = model_result.assistant_message
        if message is None and not model_result.tool_calls:
            return None
        assistant_message: dict[str, Any] = {
            "role": "assistant",
            "content": message.text if message is not None else "",
        }
        if model_result.tool_calls:
            assistant_message["tool_calls"] = [
                tool_call.to_provider_tool_call() for tool_call in model_result.tool_calls
            ]
            if not assistant_message["content"]:
                assistant_message["content"] = None
        if message is not None and message.role != ModelRole.ASSISTANT:
            assistant_message["role"] = message.role.value
        return assistant_message

    def _assistant_message_dict(self, model_result: ModelTurnResult) -> dict[str, Any]:
        assistant_message = self._assistant_message_from_model_result(model_result)
        return assistant_message or {"role": "assistant", "content": None, "tool_calls": []}

    def _synthetic_result(
        self,
        envelope: ToolCallEnvelope,
        *,
        error_kind: ToolCallFailureKind,
        message: str,
        error_code: str | None,
    ) -> ToolProtocolResultEnvelope:
        return self.synthetic_results.create(
            envelope,
            error_kind=error_kind,
            message=message,
            error_code=error_code,
        )

    def _trace_ids(self, batch: ToolCallBatch, *, call: ToolCallEnvelope | None = None) -> dict[str, Any]:
        ids = {
            "run_id": batch.run_id,
            "session_id": batch.session_id,
            "task_id": batch.task_id,
            "phase_id": batch.phase_id,
            "batch_id": batch.batch_id,
        }
        if call is not None:
            ids["action_id"] = call.tool_call_id
        return ids

    def _call_trace_ids(self, batch: ToolCallBatch, call: ToolCallEnvelope) -> dict[str, Any]:
        return self._trace_ids(batch, call=call)


def _allowed_tool_names_from_request(request: Any | None) -> list[str] | None:
    if request is None:
        return None
    tool_choice = getattr(request, "tool_choice", None)
    names = list(getattr(tool_choice, "allowed_tool_names", None) or [])
    if names:
        return names
    tools = getattr(request, "tools", None) or []
    resolved: list[str] = []
    for tool in tools:
        name = getattr(tool, "name", None)
        if name:
            resolved.append(str(name))
    return resolved or None


def _provider_capabilities_from_result(model_result: ModelTurnResult) -> ModelCapabilities | None:
    payload = model_result.metadata.get("provider_capabilities") if model_result.metadata else None
    if isinstance(payload, ModelCapabilities):
        return payload
    if isinstance(payload, dict):
        return ModelCapabilities.from_dict(payload)
    return None


def _default_state_path(registry: ToolRegistry, trace: Any | None) -> Any:
    run_dir = _trace_run_dir(trace)
    if run_dir is not None:
        return run_dir / "tool_protocol.sqlite3"
    return registry.project_root / ".singularity" / "runs" / "default" / "tool_protocol.sqlite3"


def _trace_run_dir(trace: Any | None) -> Path | None:
    store = getattr(trace, "store", None)
    run_dir = getattr(store, "run_dir", None)
    if run_dir is not None:
        return Path(run_dir)
    path = getattr(trace, "path", None)
    run_id = getattr(trace, "run_id", None)
    if path is not None and run_id:
        trace_path = Path(path)
        if trace_path.name == "events.jsonl" and trace_path.parent.name == str(run_id):
            return trace_path.parent
        if trace_path.name == str(run_id):
            return trace_path
        return trace_path.parent / str(run_id)
    return None
