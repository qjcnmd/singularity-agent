from __future__ import annotations

from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from singularity.context import ContextManager
from singularity.error_mapping import (
    tool_protocol_validation_error_code,
    tool_protocol_validation_error_kind,
)
from singularity.model import ModelCapabilities, ModelRole, ModelTurnResult
from singularity.observability.models import TraceSeverity
from singularity.planner import Planner
from singularity.tool_protocol.context_projection import ToolProtocolContextProjector
from singularity.tool_protocol.models import (
    ToolCallBatch,
    ToolCallEnvelope,
    ToolCallFailureKind,
    ToolCallPhase,
    ToolExecutionMode,
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
from singularity.tool_protocol.validator import ToolProtocolValidator
from singularity.tools import ToolExecutionRequest, ToolExecutor, ToolRegistry


@dataclass
class _ToolExecutionCounters:
    executed_count: int = 0
    failed_count: int = 0
    rejected_count: int = 0
    pending_approval_count: int = 0
    appended_tool_message_count: int = 0
    last_tool_call_id: str | None = None


@dataclass(frozen=True)
class _PreparedToolCall:
    call: ToolCallEnvelope
    record: Any


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
        self.parallel_executor = parallel_executor or ParallelToolExecutor()
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
        self._throw_if_cancelled()
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
        self._throw_if_cancelled()
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
        self._throw_if_cancelled()
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
        self._throw_if_cancelled()
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
        self._throw_if_cancelled()
        _ = planner
        counters = _ToolExecutionCounters()
        batch = self._batch_for_plan(plan, context)
        if plan.execution_mode == ToolExecutionMode.PARALLEL_READONLY and plan.parallel_groups:
            return self._execute_parallel_readonly_plan(
                plan,
                batch=batch,
                context=context,
                tool_executor=tool_executor,
                turn=turn,
            )

        for call in plan.ordered_calls:
            prepared = self._prepare_call(
                batch=batch,
                context=context,
                call=call,
                turn=turn,
                counters=counters,
            )
            if prepared is None:
                continue
            self._throw_if_cancelled()
            execution_request = ToolExecutionRequest.from_envelope(call, batch=batch)
            tool_result = tool_executor.execute_request(execution_request)
            self._throw_if_cancelled()
            self._complete_call(
                batch=batch,
                context=context,
                prepared=prepared,
                tool_result=tool_result,
                turn=turn,
                counters=counters,
            )

        return self._turn_result(plan, counters)

    def _execute_parallel_readonly_plan(
        self,
        plan: ToolExecutionPlan,
        *,
        batch: ToolCallBatch,
        context: ContextManager,
        tool_executor: ToolExecutor,
        turn: int,
    ) -> ToolProtocolTurnResult:
        counters = _ToolExecutionCounters()
        pending_records: dict[str, Any] = {}

        for group in plan.parallel_groups:
            pending_calls: list[ToolCallEnvelope] = []
            pending_records = {}
            self.trace.emit(
                "tool_protocol.parallel_group_started",
                summary="Parallel read-only tool group started.",
                payload={
                    "plan_id": plan.plan_id,
                    "batch_id": plan.batch_id,
                    "tool_call_ids": [call.tool_call_id for call in group],
                },
                ids=self._trace_ids(batch),
            )
            for call in group:
                prepared = self._prepare_call(
                    batch=batch,
                    context=context,
                    call=call,
                    turn=turn,
                    counters=counters,
                )
                if prepared is None:
                    continue
                pending_calls.append(call)
                pending_records[call.tool_call_id] = prepared

            for execution in self.parallel_executor.execute(
                pending_calls,
                tool_executor=tool_executor,
                batch=batch,
            ):
                self._throw_if_cancelled()
                call = execution.call
                self._complete_call(
                    batch=batch,
                    context=context,
                    prepared=pending_records[call.tool_call_id],
                    tool_result=execution.result,
                    turn=turn,
                    counters=counters,
                )
            self.trace.emit(
                "tool_protocol.parallel_group_completed",
                summary="Parallel read-only tool group completed.",
                payload={
                    "plan_id": plan.plan_id,
                    "batch_id": plan.batch_id,
                    "executed_count": counters.executed_count,
                    "failed_count": counters.failed_count,
                    "pending_approval_count": counters.pending_approval_count,
                },
                ids=self._trace_ids(batch),
                severity=TraceSeverity.WARNING
                if counters.failed_count or counters.pending_approval_count
                else TraceSeverity.INFO,
            )

        return self._turn_result(
            plan,
            counters,
            metadata={"parallel_group_count": len(plan.parallel_groups)},
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

    def _prepare_call(
        self,
        *,
        batch: ToolCallBatch,
        context: ContextManager,
        call: ToolCallEnvelope,
        turn: int,
        counters: _ToolExecutionCounters,
    ) -> _PreparedToolCall | None:
        self._throw_if_cancelled()
        counters.last_tool_call_id = call.tool_call_id
        call.metadata = dict(call.metadata)
        call.metadata.setdefault("batch_id", batch.batch_id)
        record = self.state_store.upsert_record(
            call,
            batch_id=batch.batch_id,
            phase=ToolCallPhase.VALIDATED,
        )
        self._emit_call_validated(batch, call)

        if call.validation_errors:
            counters.rejected_count += 1
            synthetic = self._synthetic_result(
                call,
                error_kind=_error_kind_from_validation(call.validation_errors),
                message="; ".join(call.validation_errors),
                error_code=_error_code_from_validation(call.validation_errors),
            )
            self._bind_synthetic_result(
                batch=batch,
                context=context,
                call=call,
                record=record,
                result=synthetic,
                phase=ToolCallPhase.REJECTED,
                error_message="; ".join(call.validation_errors),
                counters=counters,
                turn=turn,
            )
            self.trace.emit(
                "tool_protocol.call_rejected",
                summary="Tool call rejected by protocol.",
                payload={
                    "tool_call_id": call.tool_call_id,
                    "tool_name": call.tool_name,
                    "error_kind": synthetic.error_kind.value if synthetic.error_kind else None,
                    "error_code": synthetic.error_code,
                    "argument_digest": call.argument_digest,
                },
                ids=self._trace_ids(batch, call=call),
                severity=TraceSeverity.WARNING,
            )
            return None

        spec = self.registry.get(call.tool_name)
        replay_decision = self.state_store.check_replay(
            call,
            side_effects=spec.side_effects if spec is not None else None,
            idempotent=spec.idempotent if spec is not None else True,
        )
        if not replay_decision.allowed and replay_decision.status in {
            "side_effect_replay",
            ToolCallFailureKind.conflicting_replay.value,
        }:
            counters.rejected_count += 1
            synthetic = self._synthetic_result(
                call,
                error_kind=(
                    ToolCallFailureKind.conflicting_replay
                    if replay_decision.status == ToolCallFailureKind.conflicting_replay.value
                    else ToolCallFailureKind.replay_detected
                ),
                message=replay_decision.message,
                error_code=replay_decision.status,
            )
            self._bind_synthetic_result(
                batch=batch,
                context=context,
                call=call,
                record=record,
                result=synthetic,
                phase=ToolCallPhase.REJECTED,
                error_message=replay_decision.message,
                counters=counters,
                turn=turn,
            )
            self.trace.emit(
                "tool_protocol.replay_blocked",
                summary="Replay blocked for non-idempotent or conflicting tool call.",
                payload={
                    "tool_call_id": call.tool_call_id,
                    "tool_name": call.tool_name,
                    "replay_status": replay_decision.status,
                    "argument_digest": call.argument_digest,
                },
                ids=self._trace_ids(batch, call=call),
                severity=TraceSeverity.WARNING,
            )
            return None
        replay = replay_decision.previous_result
        if replay is not None:
            self.state_store.transition(
                call.tool_call_id,
                ToolCallPhase.RECOVERED,
                tool_result_digest=replay.content_digest,
                error_kind=replay.error_kind,
            )
            self.state_store.bind_result(
                record.record_id,
                result=replay,
                raw_result_ref=replay.raw_result_ref,
            )
            observation_id = self._append_result(context, record, replay, turn=turn)
            counters.appended_tool_message_count += 1 if observation_id else 0
            self.trace.emit(
                "tool_protocol.replay_detected",
                summary="Replay returned previous result.",
                payload={
                    "tool_call_id": call.tool_call_id,
                    "tool_name": call.tool_name,
                    "argument_digest": call.argument_digest,
                    "content_digest": replay.content_digest,
                },
                ids=self._trace_ids(batch, call=call),
            )
            return None

        self.state_store.transition(call.tool_call_id, ToolCallPhase.SCHEDULED)
        self.trace.emit(
            "tool_protocol.call_scheduled",
            summary="Tool call scheduled.",
            payload={
                "tool_call_id": call.tool_call_id,
                "tool_name": call.tool_name,
                "argument_digest": call.argument_digest,
            },
            ids=self._trace_ids(batch, call=call),
        )
        self.state_store.transition(call.tool_call_id, ToolCallPhase.RUNNING)
        self.trace.emit(
            "tool_protocol.call_started",
            summary="Tool call started.",
            payload={
                "tool_call_id": call.tool_call_id,
                "tool_name": call.tool_name,
                "argument_digest": call.argument_digest,
            },
            ids=self._trace_ids(batch, call=call),
        )
        return _PreparedToolCall(call=call, record=record)

    def _bind_synthetic_result(
        self,
        *,
        batch: ToolCallBatch,
        context: ContextManager,
        call: ToolCallEnvelope,
        record: Any,
        result: ToolProtocolResultEnvelope,
        phase: ToolCallPhase,
        error_message: str,
        counters: _ToolExecutionCounters,
        turn: int,
    ) -> None:
        self.state_store.transition(
            call.tool_call_id,
            phase,
            error_kind=result.error_kind,
            error_message=error_message,
            tool_result_digest=result.content_digest,
        )
        self.state_store.bind_result(
            record.record_id,
            result=result,
            raw_result_ref=result.raw_result_ref,
        )
        observation_id = self._append_result(context, record, result, turn=turn)
        counters.appended_tool_message_count += 1 if observation_id else 0
        self.trace.emit(
            "tool_protocol.synthetic_result_created",
            summary="Synthetic tool result created.",
            payload={
                "tool_call_id": call.tool_call_id,
                "tool_name": call.tool_name,
                "error_code": result.error_code,
                "content_digest": result.content_digest,
            },
            ids=self._trace_ids(batch, call=call),
        )

    def _complete_call(
        self,
        *,
        batch: ToolCallBatch,
        context: ContextManager,
        prepared: _PreparedToolCall,
        tool_result: Any,
        turn: int,
        counters: _ToolExecutionCounters,
    ) -> None:
        call = prepared.call
        record = prepared.record
        protocol_result = self.result_builder.build(
            envelope=call,
            result=tool_result,
            raw_result_ref=tool_result.metadata.get("output_digest"),
            policy_decision_id=tool_result.metadata.get("policy_decision_id"),
            approval_grant_id=tool_result.metadata.get("approval_grant_id"),
        )
        self.state_store.bind_result(
            record.record_id,
            result=protocol_result,
            raw_result_ref=protocol_result.raw_result_ref,
        )
        phase = (
            ToolCallPhase.SUCCEEDED
            if protocol_result.ok
            else (
                ToolCallPhase.WAITING_APPROVAL
                if protocol_result.error_code == "approval_required"
                else ToolCallPhase.FAILED
            )
        )
        self.state_store.transition(
            call.tool_call_id,
            phase,
            policy_decision_id=protocol_result.policy_decision_id,
            approval_grant_id=protocol_result.approval_grant_id,
            error_kind=protocol_result.error_kind,
            error_message=protocol_result.error_code,
            tool_result_digest=protocol_result.content_digest,
        )
        counters.executed_count += 1
        if not protocol_result.ok and protocol_result.error_code == "approval_required":
            counters.pending_approval_count += 1
        elif not protocol_result.ok:
            counters.failed_count += 1
        observation_id = self._append_result(context, record, protocol_result, turn=turn)
        counters.appended_tool_message_count += 1 if observation_id else 0
        self.trace.emit(
            "tool_protocol.call_completed",
            summary="Tool call completed.",
            payload={
                "tool_call_id": call.tool_call_id,
                "tool_name": call.tool_name,
                "ok": protocol_result.ok,
                "status": protocol_result.status,
                "error_code": protocol_result.error_code,
                "content_digest": protocol_result.content_digest,
                "policy_decision_id": protocol_result.policy_decision_id,
            },
            ids=self._trace_ids(batch, call=call),
            severity=TraceSeverity.INFO if protocol_result.ok else TraceSeverity.WARNING,
        )
        self.trace.emit(
            "tool_protocol.result_bound",
            summary="Tool result bound.",
            payload={
                "tool_call_id": call.tool_call_id,
                "tool_name": call.tool_name,
                "content_digest": protocol_result.content_digest,
                "policy_decision_id": protocol_result.policy_decision_id,
                "observation_id": protocol_result.observation_id,
            },
            ids=self._trace_ids(batch, call=call),
        )

    def _emit_call_validated(
        self,
        batch: ToolCallBatch,
        call: ToolCallEnvelope,
    ) -> None:
        self.trace.emit(
            "tool_protocol.call_validated",
            summary="Tool call validated.",
            payload={
                "tool_call_id": call.tool_call_id,
                "tool_name": call.tool_name,
                "argument_digest": call.argument_digest,
                "valid": not bool(call.validation_errors),
            },
            ids=self._trace_ids(batch, call=call),
            severity=TraceSeverity.WARNING if call.validation_errors else TraceSeverity.INFO,
        )

    def _turn_result(
        self,
        plan: ToolExecutionPlan,
        counters: _ToolExecutionCounters,
        *,
        metadata: dict[str, Any] | None = None,
    ) -> ToolProtocolTurnResult:
        status = ToolProtocolTurnStatus.PROCESSED
        next_action = "continue"
        if counters.pending_approval_count:
            status = ToolProtocolTurnStatus.PENDING_APPROVAL
            next_action = "pending_approval"
        if (
            counters.rejected_count
            and counters.executed_count == 0
            and not counters.pending_approval_count
        ):
            status = ToolProtocolTurnStatus.REJECTED
        return ToolProtocolTurnResult(
            status=status,
            batch_id=plan.batch_id,
            executed_count=counters.executed_count,
            failed_count=counters.failed_count,
            rejected_count=counters.rejected_count,
            pending_approval_count=counters.pending_approval_count,
            appended_tool_message_count=counters.appended_tool_message_count,
            next_action=next_action,
            metadata={
                "last_tool_call_id": counters.last_tool_call_id,
                "execution_mode": plan.execution_mode.value,
                **(metadata or {}),
            },
        )

    def _throw_if_cancelled(self) -> None:
        token = getattr(self, "cancellation_token", None)
        if token is not None and hasattr(token, "throw_if_cancelled"):
            token.throw_if_cancelled()

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

    def _append_result(
        self,
        context: ContextManager,
        record: Any,
        result: ToolProtocolResultEnvelope,
        *,
        turn: int = 0,
    ) -> str | None:
        return self.append_results_to_context(
            context,
            envelope=record.envelope,
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

    def _context_has_tool_message(
        self,
        context: ContextManager,
        tool_call_id: str,
        *,
        content_digest: str | None = None,
    ) -> bool:
        return ToolProtocolContextProjector.has_tool_message(
            context,
            tool_call_id,
            content_digest=content_digest,
        )

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


def _error_kind_from_validation(errors: list[str]) -> ToolCallFailureKind:
    return tool_protocol_validation_error_kind(errors)


def _error_code_from_validation(errors: list[str]) -> str | None:
    return tool_protocol_validation_error_code(errors)


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
