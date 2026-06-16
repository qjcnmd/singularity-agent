from __future__ import annotations

import json
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from miniharness.context.store import ObservationStore


@dataclass(frozen=True)
class RecoveredContext:
    run_id: str
    messages: list[dict[str, Any]]
    last_completed_tool_call_ids: set[str]
    next_action: str
    trace_last_event: str | None = None


class RecoveryManager:
    def __init__(self, db_path: Path, *, trace_path: Path | None = None) -> None:
        self.store = ObservationStore(db_path)
        self.trace_path = trace_path

    def recover(self, run_id: str) -> RecoveredContext:
        messages = self.store.load_messages(run_id)
        completed_tool_call_ids = {
            message["tool_call_id"]
            for message in messages
            if message.get("role") == "tool" and message.get("tool_call_id")
        }
        trace_last_event = self._last_trace_event()
        next_action = self._next_action(
            messages,
            completed_tool_call_ids,
            trace_last_event=trace_last_event,
        )
        return RecoveredContext(
            run_id=run_id,
            messages=messages,
            last_completed_tool_call_ids=completed_tool_call_ids,
            next_action=next_action,
            trace_last_event=trace_last_event,
        )

    def _last_trace_event(self) -> str | None:
        if self.trace_path is None or not self.trace_path.exists():
            return None
        last_event: str | None = None
        for line in self.trace_path.read_text(encoding="utf-8").splitlines():
            if not line.strip():
                continue
            try:
                event = json.loads(line).get("event")
            except json.JSONDecodeError:
                continue
            if isinstance(event, str):
                last_event = event
        return last_event

    @staticmethod
    def _next_action(
        messages: list[dict[str, Any]],
        completed_tool_call_ids: set[str],
        *,
        trace_last_event: str | None,
    ) -> str:
        if trace_last_event == "tool_result":
            return "request_model"
        if trace_last_event == "model_request":
            return "await_model_response"
        if not messages:
            return "start"
        last = messages[-1]
        if last.get("role") == "tool":
            return "request_model"
        if last.get("role") == "assistant" and last.get("tool_calls"):
            pending = [
                call.get("id")
                for call in last.get("tool_calls", [])
                if call.get("id") not in completed_tool_call_ids
            ]
            if pending:
                return "execute_tool"
        return "request_model"
