from __future__ import annotations

import json
from dataclasses import dataclass, field
from enum import Enum
from typing import Any

from singularity.model.models import ModelToolCall, ModelToolParseStatus


class ProviderStreamEventType(str, Enum):
    TEXT_DELTA = "text_delta"
    TOOL_CALL_DELTA = "tool_call_delta"
    TOOL_CALL_COMPLETED = "tool_call_completed"
    USAGE_DELTA = "usage_delta"
    RESPONSE_COMPLETED = "response_completed"
    ERROR = "error"


@dataclass
class ProviderStreamEvent:
    type: ProviderStreamEventType
    text_delta: str | None = None
    tool_call_id: str | None = None
    tool_name: str | None = None
    arguments_delta: str | None = None
    usage_delta: dict[str, Any] | None = None
    error: Any | None = None
    metadata: dict[str, Any] = field(default_factory=dict)


@dataclass
class StreamingMessage:
    content: str


@dataclass
class StreamingResponse:
    message: StreamingMessage
    tool_calls: list[ModelToolCall]
    executed_tool_count: int = 0


class StreamingAccumulator:
    def __init__(self) -> None:
        self._text: list[str] = []
        self._tool_names: dict[str, str] = {}
        self._tool_args: dict[str, list[str]] = {}

    def add(self, event: ProviderStreamEvent) -> None:
        if event.type == ProviderStreamEventType.TEXT_DELTA and event.text_delta:
            self._text.append(event.text_delta)
        elif event.type in {
            ProviderStreamEventType.TOOL_CALL_DELTA,
            ProviderStreamEventType.TOOL_CALL_COMPLETED,
        }:
            call_id = event.tool_call_id or "call_stream"
            if event.tool_name:
                self._tool_names[call_id] = event.tool_name
            self._tool_args.setdefault(call_id, [])
            if event.arguments_delta:
                self._tool_args[call_id].append(event.arguments_delta)

    def to_response(self) -> StreamingResponse:
        calls: list[ModelToolCall] = []
        for call_id, chunks in self._tool_args.items():
            raw = "".join(chunks)
            try:
                parsed = json.loads(raw or "{}")
                status = ModelToolParseStatus.VALID if isinstance(parsed, dict) else ModelToolParseStatus.SCHEMA_MISMATCH
                arguments = parsed if isinstance(parsed, dict) else {}
            except json.JSONDecodeError:
                status = ModelToolParseStatus.INVALID_JSON
                arguments = {}
            calls.append(
                ModelToolCall(
                    tool_call_id=call_id,
                    tool_name=self._tool_names.get(call_id, "<unknown>"),
                    arguments=arguments,
                    raw_arguments=raw,
                    parse_status=status,
                )
            )
        return StreamingResponse(message=StreamingMessage(content="".join(self._text)), tool_calls=calls)

