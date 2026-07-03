from __future__ import annotations

import json
from typing import Any

from singularity.context import ContextManager
from singularity.tool_protocol.models import ToolCallEnvelope, ToolProtocolResultEnvelope
from singularity.tool_protocol.state import ToolProtocolStateStore


class ToolProtocolContextProjector:
    def __init__(self, state_store: ToolProtocolStateStore) -> None:
        self.state_store = state_store

    def append_result(
        self,
        context: ContextManager,
        *,
        envelope: ToolCallEnvelope,
        result: ToolProtocolResultEnvelope,
        turn: int = 0,
    ) -> str | None:
        record = self.state_store.record_by_tool_call_id(envelope.tool_call_id)
        if self.has_tool_message(
            context,
            envelope.tool_call_id,
            content_digest=result.content_digest,
        ):
            self.state_store.mark_result_appended(
                record.record_id,
                context_message_id=record.context_message_id,
            )
            return None
        observation = context.add_tool_protocol_result(result, turn=turn)
        self.state_store.mark_result_appended(
            record.record_id,
            context_message_id=observation.id,
        )
        return observation.id

    @staticmethod
    def has_tool_message(
        context: ContextManager,
        tool_call_id: str,
        *,
        content_digest: str | None = None,
    ) -> bool:
        for message in context.messages(persist=False):
            if message.get("role") != "tool" or message.get("tool_call_id") != tool_call_id:
                continue
            if content_digest is None:
                return True
            if _message_content_digest(message) == content_digest:
                return True
        return False


def _message_content_digest(message: dict[str, Any]) -> str | None:
    try:
        payload = json.loads(str(message.get("content") or "{}"))
    except json.JSONDecodeError:
        return None
    value = payload.get("content_digest")
    return str(value) if value is not None else None
