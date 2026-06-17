import json
from pathlib import Path
from typing import Any

from pydantic import BaseModel, ConfigDict

from miniharness.tools import (
    ToolIdempotencyPolicy,
    ToolPolicy,
    ToolRegistry,
    ToolRuntime,
    ToolSpec,
)
from miniharness.trace import TraceWriter
from tests.tool_runtime_helpers import make_test_policy_runtime


class ValueInput(BaseModel):
    model_config = ConfigDict(extra="forbid")
    value: str


def make_tool_call(
    name: str, arguments: dict[str, Any], *, tool_call_id: str = "call_same"
) -> dict[str, Any]:
    return {
        "id": tool_call_id,
        "type": "function",
        "function": {"name": name, "arguments": json.dumps(arguments)},
    }


def test_duplicate_tool_call_id_same_args_returns_replay(tmp_path: Path) -> None:
    calls: list[str] = []

    def handler(args: ValueInput) -> dict[str, str]:
        calls.append(args.value)
        return {"value": args.value}

    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="echo",
            description="echo",
            input_model=ValueInput,
            handler=handler,
            idempotency_policy=ToolIdempotencyPolicy(idempotent=True, replay_returns_previous=True),
        )
    )
    runtime = ToolRuntime(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=TraceWriter.create(tmp_path),
        workspace_root=tmp_path,
        policy_runtime=make_test_policy_runtime(tmp_path),
    )

    first = runtime.execute_tool_call(make_tool_call("echo", {"value": "x"}))
    second = runtime.execute_tool_call(make_tool_call("echo", {"value": "x"}))

    assert first.ok is True
    assert second.ok is True
    assert second.metadata["replay"] is True
    assert calls == ["x"]


def test_duplicate_tool_call_id_different_args_is_rejected(tmp_path: Path) -> None:
    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="echo",
            description="echo",
            input_model=ValueInput,
            handler=lambda args: {"value": args.value},
        )
    )
    runtime = ToolRuntime(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=TraceWriter.create(tmp_path),
        workspace_root=tmp_path,
        policy_runtime=make_test_policy_runtime(tmp_path),
    )

    runtime.execute_tool_call(make_tool_call("echo", {"value": "x"}))
    second = runtime.execute_tool_call(make_tool_call("echo", {"value": "y"}))

    assert second.ok is False
    assert second.error_code == "conflicting_replay"


def test_non_idempotent_duplicate_does_not_auto_replay(tmp_path: Path) -> None:
    calls: list[str] = []

    def handler(args: ValueInput) -> dict[str, str]:
        calls.append(args.value)
        return {"value": args.value}

    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="not_replayable",
            description="no replay",
            input_model=ValueInput,
            handler=handler,
            idempotent=False,
        )
    )
    runtime = ToolRuntime(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=TraceWriter.create(tmp_path),
        workspace_root=tmp_path,
        policy_runtime=make_test_policy_runtime(tmp_path),
    )

    first = runtime.execute_tool_call(make_tool_call("not_replayable", {"value": "x"}))
    second = runtime.execute_tool_call(make_tool_call("not_replayable", {"value": "x"}))

    assert first.ok is True
    assert second.ok is False
    assert second.error_code == "replay_not_allowed"
    assert calls == ["x"]

