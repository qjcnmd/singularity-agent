import json
from pathlib import Path
from typing import Any

from pydantic import BaseModel, ConfigDict

from singularity.tools import (
    ToolIdempotencyPolicy,
    ToolPolicy,
    ToolRegistry,
    ToolExecutor,
    ToolSpec,
)
from singularity.jsonl_trace import JsonlTraceRecorder
from tests.tool_executor_helpers import make_test_policy_engine


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
    component = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )

    first = component.execute_tool_call(make_tool_call("echo", {"value": "x"}))
    second = component.execute_tool_call(make_tool_call("echo", {"value": "x"}))

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
    component = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )

    component.execute_tool_call(make_tool_call("echo", {"value": "x"}))
    second = component.execute_tool_call(make_tool_call("echo", {"value": "y"}))

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
    component = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )

    first = component.execute_tool_call(make_tool_call("not_replayable", {"value": "x"}))
    second = component.execute_tool_call(make_tool_call("not_replayable", {"value": "x"}))

    assert first.ok is True
    assert second.ok is False
    assert second.error_code == "replay_not_allowed"
    assert calls == ["x"]

