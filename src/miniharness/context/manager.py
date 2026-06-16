from __future__ import annotations

import json
from dataclasses import dataclass, field
from typing import Any
from uuid import uuid4


TOOL_RESULT_PREVIEW_LIMIT = 4000


@dataclass(frozen=True)
class ToolObservation:
    id: str
    tool_name: str
    tool_call_id: str | None
    ok: bool
    raw_result: dict[str, Any]
    preview: str
    truncated: bool
    metadata: dict[str, Any] = field(default_factory=dict)


class ContextManager:
    def __init__(self, *, system_prompt: str, user_goal: str) -> None:
        self._messages: list[dict[str, Any]] = [
            {"role": "system", "content": system_prompt},
            {"role": "user", "content": user_goal},
        ]
        self.tool_observations: list[ToolObservation] = []

    def messages(self) -> list[dict[str, Any]]:
        return list(self._messages)

    def add_assistant_message(self, message: dict[str, Any]) -> None:
        self._messages.append(dict(message))

    def add_tool_result(
        self, *, tool_call: dict[str, Any], result: dict[str, Any]
    ) -> ToolObservation:
        function = tool_call.get("function") or {}
        tool_name = function.get("name", "<unknown>")
        tool_call_id = tool_call.get("id")
        preview, truncated = self._preview_result(result)
        observation = ToolObservation(
            id=uuid4().hex,
            tool_name=tool_name,
            tool_call_id=tool_call_id,
            ok=bool(result.get("ok")),
            raw_result=result,
            preview=preview,
            truncated=truncated,
            metadata={
                "result_keys": sorted(result.keys()),
            },
        )
        self.tool_observations.append(observation)
        self._messages.append(
            {
                "role": "tool",
                "tool_call_id": tool_call_id,
                "name": tool_name,
                "content": json.dumps(
                    {
                        "ok": observation.ok,
                        "tool_name": observation.tool_name,
                        "tool_call_id": observation.tool_call_id,
                        "content": observation.preview,
                        "truncated": observation.truncated,
                    },
                    ensure_ascii=False,
                ),
            }
        )
        return observation

    @staticmethod
    def _preview_result(result: dict[str, Any]) -> tuple[str, bool]:
        raw_content = result.get("content")
        if isinstance(raw_content, str):
            source = raw_content
        else:
            source = json.dumps(result, ensure_ascii=False)
        if len(source) <= TOOL_RESULT_PREVIEW_LIMIT:
            return source, False
        return source[:TOOL_RESULT_PREVIEW_LIMIT], True
