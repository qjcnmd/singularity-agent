import json
import time
from pathlib import Path
from typing import Any

from pydantic import BaseModel, Field

from miniharness.tools import (
    PermissionLevel,
    ToolPolicy,
    ToolRegistry,
    ToolRuntime,
    ToolSpec,
)
from miniharness.trace import TraceWriter


def make_tool_call(
    name: str, arguments: dict[str, Any], *, tool_call_id: str = "call_1"
) -> dict[str, Any]:
    return {
        "id": tool_call_id,
        "type": "function",
        "function": {"name": name, "arguments": json.dumps(arguments)},
    }


def make_raw_tool_call(
    name: str, arguments: str, *, tool_call_id: str = "call_1"
) -> dict[str, Any]:
    return {
        "id": tool_call_id,
        "type": "function",
        "function": {"name": name, "arguments": arguments},
    }


def make_runtime(tmp_path: Path) -> ToolRuntime:
    return ToolRuntime(
        registry=ToolRegistry(tmp_path),
        policy=ToolPolicy.read_only(),
        trace=TraceWriter.create(tmp_path),
        workspace_root=tmp_path,
    )


def test_runtime_successfully_calls_registered_read_only_tool(tmp_path: Path) -> None:
    readme = tmp_path / "README.md"
    readme.write_text("hello from runtime", encoding="utf-8")
    runtime = make_runtime(tmp_path)

    result = runtime.execute_tool_call(
        make_tool_call("read_file", {"path": "README.md", "max_bytes": 100})
    )

    assert result.ok is True
    assert result.error_code is None
    assert result.content["path"] == "README.md"
    assert result.content["content"] == "hello from runtime"


def test_runtime_rejects_unknown_tool(tmp_path: Path) -> None:
    runtime = make_runtime(tmp_path)

    result = runtime.execute_tool_call(make_tool_call("missing_tool", {}))

    assert result.ok is False
    assert result.error_code == "tool_not_found"


def test_runtime_rejects_bad_arguments_json(tmp_path: Path) -> None:
    runtime = make_runtime(tmp_path)

    result = runtime.execute_tool_call(make_raw_tool_call("read_file", "{not json"))

    assert result.ok is False
    assert result.error_code == "bad_arguments_json"


def test_runtime_rejects_pydantic_validation_errors(tmp_path: Path) -> None:
    runtime = make_runtime(tmp_path)

    result = runtime.execute_tool_call(
        make_tool_call("read_file", {"path": "README.md", "max_bytes": 0})
    )

    assert result.ok is False
    assert result.error_code == "validation_error"


class WriteInput(BaseModel):
    path: str


def test_runtime_policy_denies_write_tools_by_default(tmp_path: Path) -> None:
    called = False

    def write_handler(_args: WriteInput) -> dict[str, str]:
        nonlocal called
        called = True
        return {"status": "wrote"}

    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="write_file",
            version="0.0.1",
            description="Pretend to write a file.",
            input_model=WriteInput,
            handler=write_handler,
            permission_level=PermissionLevel.WRITE,
            risk_tags=("write",),
        )
    )
    runtime = ToolRuntime(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=TraceWriter.create(tmp_path),
        workspace_root=tmp_path,
    )

    result = runtime.execute_tool_call(make_tool_call("write_file", {"path": "x.txt"}))

    assert result.ok is False
    assert result.error_code in {"policy_denied", "permission_denied"}
    assert called is False


class EmptyInput(BaseModel):
    pass


def test_runtime_timeout_returns_timeout_error(tmp_path: Path) -> None:
    def slow_handler(_args: EmptyInput) -> str:
        time.sleep(0.2)
        return "done"

    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="slow_read",
            version="0.0.1",
            description="Slow read.",
            input_model=EmptyInput,
            handler=slow_handler,
            permission_level=PermissionLevel.READ_ONLY,
            risk_tags=("read",),
            timeout_seconds=0.01,
        )
    )
    runtime = ToolRuntime(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=TraceWriter.create(tmp_path),
        workspace_root=tmp_path,
    )

    result = runtime.execute_tool_call(make_tool_call("slow_read", {}))

    assert result.ok is False
    assert result.error_code == "timeout"


def test_runtime_truncates_oversized_output_with_head_and_tail(tmp_path: Path) -> None:
    def large_handler(_args: EmptyInput) -> str:
        return "A" * 80 + "Z" * 80

    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="large_read",
            version="0.0.1",
            description="Large read.",
            input_model=EmptyInput,
            handler=large_handler,
            permission_level=PermissionLevel.READ_ONLY,
            risk_tags=("read",),
            max_output_chars=60,
        )
    )
    runtime = ToolRuntime(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=TraceWriter.create(tmp_path),
        workspace_root=tmp_path,
    )

    result = runtime.execute_tool_call(make_tool_call("large_read", {}))

    assert result.ok is True
    assert result.truncated is True
    assert result.metadata["original_chars"] == 160
    assert result.metadata["returned_chars"] <= 60
    assert str(result.content).startswith("A")
    assert str(result.content).endswith("Z")


def test_runtime_writes_audit_trace(tmp_path: Path) -> None:
    trace = TraceWriter.create(tmp_path)
    runtime = ToolRuntime(
        registry=ToolRegistry(tmp_path),
        policy=ToolPolicy.read_only(),
        trace=trace,
        workspace_root=tmp_path,
    )

    runtime.execute_tool_call(
        make_tool_call("list_files", {"path": ".", "max_depth": 1}, tool_call_id="call_list")
    )

    events = [json.loads(line) for line in trace.path.read_text(encoding="utf-8").splitlines()]
    tool_events = [event for event in events if event["event"] == "tool_call"]
    assert len(tool_events) == 1
    audit = tool_events[0]["data"]
    assert audit["tool_call_id"] == "call_list"
    assert audit["tool_name"] == "list_files"
    assert audit["validated_args"] == {"path": ".", "max_depth": 1}
    assert audit["permission_level"] == "read_only"
    assert audit["status"] == "ok"
    assert audit["error_code"] is None
    assert audit["cache_hit"] is False
    assert "duration_seconds" in audit
    assert "output_digest" in audit


class EchoInput(BaseModel):
    value: str = Field(..., min_length=1)


def test_runtime_uses_run_cache_for_cacheable_read_only_tools(tmp_path: Path) -> None:
    calls: list[str] = []

    def echo_handler(args: EchoInput) -> dict[str, str]:
        calls.append(args.value)
        return {"value": args.value}

    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="echo_read",
            version="0.0.1",
            description="Echo input.",
            input_model=EchoInput,
            handler=echo_handler,
            permission_level=PermissionLevel.READ_ONLY,
            risk_tags=("read",),
            cacheable=True,
        )
    )
    trace = TraceWriter.create(tmp_path)
    runtime = ToolRuntime(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=trace,
        workspace_root=tmp_path,
    )

    first = runtime.execute_tool_call(make_tool_call("echo_read", {"value": "same"}))
    second = runtime.execute_tool_call(make_tool_call("echo_read", {"value": "same"}))

    assert first.ok is True
    assert second.ok is True
    assert second.metadata["cache_hit"] is True
    assert calls == ["same"]

    events = [json.loads(line) for line in trace.path.read_text(encoding="utf-8").splitlines()]
    assert [event["data"]["cache_hit"] for event in events if event["event"] == "tool_call"] == [
        False,
        True,
    ]
