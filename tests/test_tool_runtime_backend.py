import json
import time
from pathlib import Path
from typing import Any

from pydantic import BaseModel, ConfigDict

from miniharness.policy import Capability, OperationKind
from miniharness.tools import (
    PermissionLevel,
    ToolExecutionBackendKind,
    ToolPolicy,
    ToolRegistry,
    ToolRuntime,
    ToolSpec,
)
from miniharness.trace import TraceWriter
from tests.tool_runtime_helpers import runtime_default_policy_runtime, make_test_policy_runtime


class EmptyInput(BaseModel):
    model_config = ConfigDict(extra="forbid")


def make_tool_call(name: str, arguments: dict[str, Any] | None = None) -> dict[str, Any]:
    return {
        "id": f"call_{name}",
        "type": "function",
        "function": {"name": name, "arguments": json.dumps(arguments or {})},
    }


def test_timeout_is_structured_and_audited_with_backend(tmp_path: Path) -> None:
    def slow(_args: EmptyInput) -> dict[str, str]:
        time.sleep(0.2)
        return {"ok": "late"}

    trace = TraceWriter.create(tmp_path)
    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="slow",
            description="slow",
            input_model=EmptyInput,
            handler=slow,
            timeout_seconds=0.01,
        )
    )
    runtime = ToolRuntime(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=trace,
        workspace_root=tmp_path,
        policy_runtime=make_test_policy_runtime(tmp_path),
    )

    result = runtime.execute_tool_call(make_tool_call("slow"))

    assert result.ok is False
    assert result.error_code == "timeout"
    assert result.metadata["backend"] == "in_process"
    assert result.metadata["timeout_type"] == "execution"
    assert result.metadata["timeout_untrusted_state"] is True
    assert "in_process" in trace.path.read_text(encoding="utf-8")


def test_delegated_command_tool_does_not_run_in_process_handler(tmp_path: Path) -> None:
    called = False

    def handler(_args: EmptyInput) -> dict[str, str]:
        nonlocal called
        called = True
        return {"ran": "handler"}

    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="delegated_command",
            description="delegated",
            input_model=EmptyInput,
            handler=handler,
            permission_level=PermissionLevel.SHELL,
            capabilities=(Capability.EXECUTE_COMMAND,),
            operation=OperationKind.EXECUTE_COMMAND,
            execution_backend=ToolExecutionBackendKind.DELEGATED_COMMAND_RUNTIME,
            uses_command_runtime=True,
        )
    )
    runtime = ToolRuntime(
        registry=registry,
        policy=ToolPolicy.coding_agent(),
        trace=TraceWriter.create(tmp_path),
        workspace_root=tmp_path,
        standalone_can_execute=False,
        policy_runtime=runtime_default_policy_runtime(tmp_path),
    )

    result = runtime.execute_tool_call(make_tool_call("delegated_command"))

    assert result.ok is False
    assert result.error_code in {"delegated_backend_unavailable", "approval_required", "sandbox_required"}
    assert called is False


def test_delegated_backend_contract_does_not_require_legacy_boolean(tmp_path: Path) -> None:
    called = False

    def handler(_args: EmptyInput) -> dict[str, str]:
        nonlocal called
        called = True
        return {"ran": "handler"}

    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="backend_only_command",
            description="delegated",
            input_model=EmptyInput,
            handler=handler,
            permission_level=PermissionLevel.SHELL,
            capabilities=(Capability.EXECUTE_COMMAND,),
            operation=OperationKind.EXECUTE_COMMAND,
            execution_backend=ToolExecutionBackendKind.DELEGATED_COMMAND_RUNTIME,
        )
    )
    runtime = ToolRuntime(
        registry=registry,
        policy=ToolPolicy.coding_agent(),
        trace=TraceWriter.create(tmp_path),
        workspace_root=tmp_path,
        standalone_can_execute=False,
        policy_runtime=runtime_default_policy_runtime(tmp_path),
    )

    result = runtime.execute_tool_call(make_tool_call("backend_only_command"))

    assert result.ok is False
    assert result.error_code == "delegated_backend_unavailable"
    assert called is False

