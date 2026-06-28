import json
import time
from pathlib import Path
from typing import Any

from pydantic import BaseModel, ConfigDict

from singularity.jsonl_trace import JsonlTraceRecorder
from singularity.policy import Capability, OperationKind
from singularity.tools import (
    PermissionLevel,
    ToolExecutionBackendKind,
    ToolExecutor,
    ToolPolicy,
    ToolRegistry,
    ToolSpec,
)
from tests.tool_executor_helpers import default_policy_engine, make_test_policy_engine


class EmptyInput(BaseModel):
    model_config = ConfigDict(extra="forbid")


def picklable_delegated_handler(_args: EmptyInput) -> dict[str, str]:
    return {"ran": "handler"}


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

    trace = JsonlTraceRecorder.create(tmp_path)
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
    component = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=trace,
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )

    result = component.execute_tool_call(make_tool_call("slow"))

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
            execution_backend=ToolExecutionBackendKind.DELEGATED_COMMAND_EXECUTOR,
            uses_command_executor=True,
        )
    )
    component = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.coding_agent(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        standalone_can_execute=False,
        policy_engine=default_policy_engine(tmp_path),
    )

    result = component.execute_tool_call(make_tool_call("delegated_command"))

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
            execution_backend=ToolExecutionBackendKind.DELEGATED_COMMAND_EXECUTOR,
        )
    )
    component = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.coding_agent(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        standalone_can_execute=False,
        policy_engine=default_policy_engine(tmp_path),
    )

    result = component.execute_tool_call(make_tool_call("backend_only_command"))

    assert result.ok is False
    assert result.error_code == "sandbox_required"
    assert called is False


def test_delegated_backend_does_not_use_process_isolation(tmp_path: Path) -> None:
    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="delegated_picklable_command",
            description="delegated",
            input_model=EmptyInput,
            handler=picklable_delegated_handler,
            permission_level=PermissionLevel.SHELL,
            capabilities=(Capability.EXECUTE_COMMAND,),
            operation=OperationKind.EXECUTE_COMMAND,
            execution_backend=ToolExecutionBackendKind.DELEGATED_COMMAND_EXECUTOR,
            uses_command_executor=True,
        )
    )
    component = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.coding_agent(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        standalone_can_execute=True,
        policy_engine=default_policy_engine(tmp_path),
    )

    result = component.execute_tool_call(make_tool_call("delegated_picklable_command"))

    assert result.metadata["backend"] == "delegated_command_executor"
    assert result.metadata["handler_isolation"] == "thread"

