from __future__ import annotations

from collections.abc import Mapping
from typing import Any


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
