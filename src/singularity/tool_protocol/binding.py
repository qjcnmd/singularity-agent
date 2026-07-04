from __future__ import annotations

from typing import Any

from singularity.context import ContextManager
from singularity.tool_protocol.context_projection import ToolProtocolContextProjector
from singularity.tool_protocol.models import ToolCallEnvelope, ToolCallPhase, ToolProtocolResultEnvelope
from singularity.tool_protocol.state import ToolProtocolStateStore
from singularity.tool_protocol.transitions import ToolProtocolStateTransitioner


class ToolProtocolResultBinder:
    def __init__(
        self,
        state_store: ToolProtocolStateStore,
        context_projector: ToolProtocolContextProjector,
    ) -> None:
        self.state_store = state_store
        self.context_projector = context_projector

    def bind(
        self,
        *,
        record: Any,
        result: ToolProtocolResultEnvelope,
    ) -> None:
        self.state_store.bind_result(
            record.record_id,
            result=result,
            raw_result_ref=result.raw_result_ref,
        )

    def append(
        self,
        context: ContextManager,
        *,
        record: Any,
        result: ToolProtocolResultEnvelope,
        turn: int = 0,
    ) -> str | None:
        return self.context_projector.append_result(
            context,
            envelope=record.envelope,
            result=result,
            turn=turn,
        )

    def bind_and_append(
        self,
        context: ContextManager,
        *,
        record: Any,
        result: ToolProtocolResultEnvelope,
        turn: int = 0,
    ) -> str | None:
        self.bind(record=record, result=result)
        return self.append(context, record=record, result=result, turn=turn)

    def bind_synthetic(
        self,
        context: ContextManager,
        *,
        transitions: ToolProtocolStateTransitioner,
        call: ToolCallEnvelope,
        record: Any,
        result: ToolProtocolResultEnvelope,
        phase: ToolCallPhase,
        error_message: str,
        turn: int = 0,
    ) -> str | None:
        transitions.synthetic_result(
            call,
            phase=phase,
            result=result,
            error_message=error_message,
        )
        return self.bind_and_append(context, record=record, result=result, turn=turn)
