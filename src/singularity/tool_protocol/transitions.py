from __future__ import annotations

from typing import Any

from singularity.tool_protocol.models import ToolCallEnvelope, ToolCallPhase, ToolProtocolResultEnvelope
from singularity.tool_protocol.state import ToolProtocolStateStore


class ToolProtocolStateTransitioner:
    def __init__(self, state_store: ToolProtocolStateStore) -> None:
        self.state_store = state_store

    def validated(self, call: ToolCallEnvelope, *, batch_id: str) -> Any:
        return self.state_store.upsert_record(
            call,
            batch_id=batch_id,
            phase=ToolCallPhase.VALIDATED,
        )

    def scheduled(self, call: ToolCallEnvelope) -> None:
        self.state_store.transition(call.tool_call_id, ToolCallPhase.SCHEDULED)

    def running(self, call: ToolCallEnvelope) -> None:
        self.state_store.transition(call.tool_call_id, ToolCallPhase.RUNNING)

    def replay_recovered(
        self,
        call: ToolCallEnvelope,
        result: ToolProtocolResultEnvelope,
    ) -> None:
        self.state_store.transition(
            call.tool_call_id,
            ToolCallPhase.RECOVERED,
            tool_result_digest=result.content_digest,
            error_kind=result.error_kind,
        )

    def synthetic_result(
        self,
        call: ToolCallEnvelope,
        *,
        phase: ToolCallPhase,
        result: ToolProtocolResultEnvelope,
        error_message: str,
    ) -> None:
        self.state_store.transition(
            call.tool_call_id,
            phase,
            error_kind=result.error_kind,
            error_message=error_message,
            tool_result_digest=result.content_digest,
        )

    def completed(
        self,
        call: ToolCallEnvelope,
        *,
        result: ToolProtocolResultEnvelope,
        phase: ToolCallPhase,
    ) -> None:
        self.state_store.transition(
            call.tool_call_id,
            phase,
            policy_decision_id=result.policy_decision_id,
            approval_grant_id=result.approval_grant_id,
            error_kind=result.error_kind,
            error_message=result.error_code,
            tool_result_digest=result.content_digest,
        )
