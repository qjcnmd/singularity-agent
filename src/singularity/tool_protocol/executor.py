from __future__ import annotations

from collections.abc import Callable
from dataclasses import dataclass
from typing import Any

from singularity.context import ContextManager
from singularity.error_mapping import (
    tool_protocol_validation_error_code,
    tool_protocol_validation_error_kind,
)
from singularity.observability.models import TraceSeverity
from singularity.planner import Planner
from singularity.tool_protocol.binding import ToolProtocolResultBinder
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
)
from singularity.tool_protocol.parallel import ParallelToolExecutor
from singularity.tool_protocol.result import ToolProtocolResultBuilder
from singularity.tool_protocol.synthetic_result import ToolProtocolSyntheticResultFactory
from singularity.tool_protocol.trace import ToolProtocolTrace
from singularity.tool_protocol.transitions import ToolProtocolStateTransitioner
from singularity.tools import ToolExecutionRequest, ToolExecutor, ToolRegistry
from singularity.tools.execution_pipeline import (
    PLANNER_ACTION_ID_METADATA_KEY,
    PLANNER_UPDATE_DEFERRED_METADATA_KEY,
)


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


class ToolProtocolPlanExecutor:
    def __init__(
        self,
        *,
        registry: ToolRegistry,
        state_transitions: ToolProtocolStateTransitioner,
        result_binder: ToolProtocolResultBinder,
        parallel_executor: ParallelToolExecutor,
        result_builder: ToolProtocolResultBuilder,
        synthetic_results: ToolProtocolSyntheticResultFactory,
        trace: ToolProtocolTrace,
        batch_for_plan: Callable[[ToolExecutionPlan, ContextManager], ToolCallBatch],
        turn_result_factory: Callable[
            [ToolExecutionPlan, _ToolExecutionCounters],
            ToolProtocolTurnResult,
        ],
        trace_ids_factory: Callable[
            [ToolCallBatch],
            dict[str, Any],
        ],
        call_trace_ids_factory: Callable[
            [ToolCallBatch, ToolCallEnvelope],
            dict[str, Any],
        ],
        throw_if_cancelled: Callable[[], None],
    ) -> None:
        self.registry = registry
        self.state_transitions = state_transitions
        self.result_binder = result_binder
        self.parallel_executor = parallel_executor
        self.result_builder = result_builder
        self.synthetic_results = synthetic_results
        self.trace = trace
        self.batch_for_plan = batch_for_plan
        self.turn_result_factory = turn_result_factory
        self.trace_ids_factory = trace_ids_factory
        self.call_trace_ids_factory = call_trace_ids_factory
        self.throw_if_cancelled = throw_if_cancelled

    def execute(
        self,
        plan: ToolExecutionPlan,
        *,
        context: ContextManager,
        tool_executor: ToolExecutor,
        planner: Planner | None,
        turn: int = 0,
    ) -> ToolProtocolTurnResult:
        self.throw_if_cancelled()
        counters = _ToolExecutionCounters()
        batch = self.batch_for_plan(plan, context)
        if plan.execution_mode == ToolExecutionMode.PARALLEL_READONLY and plan.parallel_groups:
            return self._execute_parallel_readonly_plan(
                plan,
                batch=batch,
                context=context,
                tool_executor=tool_executor,
                planner=planner,
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
            self.throw_if_cancelled()
            execution_request = ToolExecutionRequest.from_envelope(call, batch=batch)
            tool_result = tool_executor.execute_request(execution_request)
            self.throw_if_cancelled()
            self._complete_call(
                batch=batch,
                context=context,
                prepared=prepared,
                tool_result=tool_result,
                turn=turn,
                counters=counters,
            )

        return self.turn_result_factory(plan, counters)

    def _execute_parallel_readonly_plan(
        self,
        plan: ToolExecutionPlan,
        *,
        batch: ToolCallBatch,
        context: ContextManager,
        tool_executor: ToolExecutor,
        planner: Planner | None,
        turn: int,
    ) -> ToolProtocolTurnResult:
        counters = _ToolExecutionCounters()

        for group in plan.parallel_groups:
            pending_calls: list[ToolCallEnvelope] = []
            pending_records: dict[str, _PreparedToolCall] = {}
            self.trace.emit(
                "tool_protocol.parallel_group_started",
                summary="Parallel read-only tool group started.",
                payload={
                    "plan_id": plan.plan_id,
                    "batch_id": plan.batch_id,
                    "tool_call_ids": [call.tool_call_id for call in group],
                },
                ids=self.trace_ids_factory(batch),
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
                defer_planner_update=planner is not None,
            ):
                self.throw_if_cancelled()
                call = execution.call
                self._apply_deferred_planner_update(
                    planner,
                    call=call,
                    tool_result=execution.result,
                )
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
                ids=self.trace_ids_factory(batch),
                severity=TraceSeverity.WARNING
                if counters.failed_count or counters.pending_approval_count
                else TraceSeverity.INFO,
            )

        return self.turn_result_factory(
            plan,
            counters,
            metadata={"parallel_group_count": len(plan.parallel_groups)},
        )

    @staticmethod
    def _apply_deferred_planner_update(
        planner: Planner | None,
        *,
        call: ToolCallEnvelope,
        tool_result: Any,
    ) -> None:
        if planner is None:
            return
        metadata = getattr(tool_result, "metadata", {}) or {}
        if not metadata.get(PLANNER_UPDATE_DEFERRED_METADATA_KEY):
            return
        planner.update_from_tool_result(
            tool_call_id=call.tool_call_id,
            tool_name=call.tool_name,
            result=tool_result,
            action_id=metadata.get(PLANNER_ACTION_ID_METADATA_KEY),
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
        self.throw_if_cancelled()
        counters.last_tool_call_id = call.tool_call_id
        call.metadata = dict(call.metadata)
        call.metadata.setdefault("batch_id", batch.batch_id)
        record = self.state_transitions.validated(call, batch_id=batch.batch_id)
        self._emit_call_validated(batch, call)

        if call.validation_errors:
            counters.rejected_count += 1
            message = "; ".join(call.validation_errors)
            synthetic = self._synthetic_result(
                call,
                error_kind=_error_kind_from_validation(call.validation_errors),
                message=message,
                error_code=_error_code_from_validation(call.validation_errors),
            )
            self._bind_synthetic_result(
                batch=batch,
                context=context,
                call=call,
                record=record,
                result=synthetic,
                phase=ToolCallPhase.REJECTED,
                error_message=message,
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
                ids=self.call_trace_ids_factory(batch, call),
                severity=TraceSeverity.WARNING,
            )
            return None

        spec = self.registry.get(call.tool_name)
        replay_decision = self.result_binder.state_store.check_replay(
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
                ids=self.call_trace_ids_factory(batch, call),
                severity=TraceSeverity.WARNING,
            )
            return None
        replay = replay_decision.previous_result
        if replay is not None:
            self.state_transitions.replay_recovered(call, replay)
            observation_id = self.result_binder.bind_and_append(
                context,
                record=record,
                result=replay,
                turn=turn,
            )
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
                ids=self.call_trace_ids_factory(batch, call),
            )
            return None

        self.state_transitions.scheduled(call)
        self.trace.emit(
            "tool_protocol.call_scheduled",
            summary="Tool call scheduled.",
            payload={
                "tool_call_id": call.tool_call_id,
                "tool_name": call.tool_name,
                "argument_digest": call.argument_digest,
            },
            ids=self.call_trace_ids_factory(batch, call),
        )
        self.state_transitions.running(call)
        self.trace.emit(
            "tool_protocol.call_started",
            summary="Tool call started.",
            payload={
                "tool_call_id": call.tool_call_id,
                "tool_name": call.tool_name,
                "argument_digest": call.argument_digest,
            },
            ids=self.call_trace_ids_factory(batch, call),
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
        observation_id = self.result_binder.bind_synthetic(
            context,
            transitions=self.state_transitions,
            call=call,
            record=record,
            result=result,
            phase=phase,
            error_message=error_message,
            turn=turn,
        )
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
            ids=self.call_trace_ids_factory(batch, call),
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
        self.result_binder.bind(record=record, result=protocol_result)
        phase = (
            ToolCallPhase.SUCCEEDED
            if protocol_result.ok
            else (
                ToolCallPhase.WAITING_APPROVAL
                if protocol_result.error_code == "approval_required"
                else ToolCallPhase.FAILED
            )
        )
        self.state_transitions.completed(call, result=protocol_result, phase=phase)
        counters.executed_count += 1
        if not protocol_result.ok and protocol_result.error_code == "approval_required":
            counters.pending_approval_count += 1
        elif not protocol_result.ok:
            counters.failed_count += 1
        observation_id = self.result_binder.append(
            context,
            record=record,
            result=protocol_result,
            turn=turn,
        )
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
            ids=self.call_trace_ids_factory(batch, call),
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
            ids=self.call_trace_ids_factory(batch, call),
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
            ids=self.call_trace_ids_factory(batch, call),
            severity=TraceSeverity.WARNING if call.validation_errors else TraceSeverity.INFO,
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


def build_tool_protocol_turn_result(
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


def _error_kind_from_validation(errors: list[str]) -> ToolCallFailureKind:
    return tool_protocol_validation_error_kind(errors)


def _error_code_from_validation(errors: list[str]) -> str | None:
    return tool_protocol_validation_error_code(errors)
