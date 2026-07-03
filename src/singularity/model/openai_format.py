from __future__ import annotations

import json
from collections.abc import Mapping
from typing import Any

from singularity.model.messages import MessageConverter
from singularity.model.models import (
    ModelCapabilities,
    ModelMessage,
    ModelToolSchema,
    ToolChoiceMode,
    ToolChoicePolicy,
)


def model_messages_to_openai(
    messages: list[ModelMessage],
    capabilities: ModelCapabilities,
) -> list[dict[str, Any]]:
    converter = MessageConverter()
    provider_messages = converter.to_provider_messages(
        messages,
        capabilities=capabilities,
    )
    for index, payload in enumerate(provider_messages):
        metadata = payload.pop("metadata", {}) or {}
        tool_calls = messages[index].metadata.get("tool_calls") or metadata.get("tool_calls")
        if tool_calls:
            payload["tool_calls"] = [safe_provider_tool_call(tool_call) for tool_call in tool_calls]
    return provider_messages


def safe_provider_tool_call(tool_call: Any) -> dict[str, Any]:
    if not isinstance(tool_call, dict):
        return {
            "id": "",
            "type": "function",
            "function": {"name": "<unknown>", "arguments": "{}"},
        }
    raw_function = tool_call.get("function")
    function = raw_function if isinstance(raw_function, dict) else {}
    arguments = function.get("arguments", "{}")
    if not isinstance(arguments, str):
        arguments = json.dumps(arguments, ensure_ascii=False, sort_keys=True, default=str)
    return {
        "id": str(tool_call.get("id") or ""),
        "type": str(tool_call.get("type") or "function"),
        "function": {
            "name": str(function.get("name") or "<unknown>"),
            "arguments": arguments or "{}",
        },
    }


def model_tool_to_openai(
    tool: ModelToolSchema,
    *,
    strict: bool | None = None,
) -> dict[str, Any]:
    function: dict[str, Any] = {
        "name": tool.name,
        "description": tool.description,
        "parameters": tool.parameters_schema,
    }
    strict_value = bool(tool.metadata.get("strict")) if strict is None else strict
    if strict_value:
        function["strict"] = True
    return {"type": "function", "function": function}


def tool_schema_to_openai(
    tool: Mapping[str, Any],
    *,
    strict: bool = False,
) -> dict[str, Any]:
    function: dict[str, Any] = {
        "name": str(tool["name"]),
        "description": str(tool.get("description") or ""),
        "parameters": dict(tool.get("input_schema") or {}),
    }
    if strict:
        function["strict"] = True
    return {"type": "function", "function": function}


def serialize_tool_choice(policy: ToolChoicePolicy | ToolChoiceMode | str) -> Any:
    if isinstance(policy, ToolChoiceMode):
        return policy.value
    if isinstance(policy, str):
        return policy
    if policy.mode == ToolChoiceMode.SPECIFIC_TOOL and policy.tool_name:
        return {"type": "function", "function": {"name": policy.tool_name}}
    if policy.mode == ToolChoiceMode.ALLOWED_TOOLS:
        return ToolChoiceMode.AUTO.value
    return policy.mode.value
