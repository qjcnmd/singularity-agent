import json
from pathlib import Path
from typing import Any

from pydantic import BaseModel, ConfigDict

from miniharness.policy import Capability, OperationKind, ResourceRef
from miniharness.tools import (
    PermissionLevel,
    ToolCachePolicy,
    ToolExecutionBackendKind,
    ToolIdempotencyPolicy,
    ToolOutputEnvelope,
    ToolRegistry,
    ToolRetryPolicy,
    ToolRuntime,
    ToolSensitivityLevel,
    ToolSideEffectKind,
    ToolSpec,
)
from miniharness.tools.policy import ToolPolicy
from miniharness.trace import TraceWriter
from tests.tool_runtime_helpers import make_test_policy_runtime


class StrictInput(BaseModel):
    model_config = ConfigDict(extra="forbid")

    value: str


class OutputModel(BaseModel):
    value: str


def make_tool_call(name: str, arguments: dict[str, Any]) -> dict[str, Any]:
    return {
        "id": "call_contract",
        "type": "function",
        "function": {"name": name, "arguments": json.dumps(arguments)},
    }


def test_tool_spec_keeps_legacy_constructor_defaults() -> None:
    spec = ToolSpec(
        name="echo",
        description="Echo value.",
        input_model=StrictInput,
        handler=lambda args: {"value": args.value},
    )

    assert spec.permission_level == PermissionLevel.READ_ONLY
    assert spec.capabilities == (Capability.READ_WORKSPACE,)
    assert spec.operation == OperationKind.READ_FILE
    assert spec.side_effects == ToolSideEffectKind.READ_WORKSPACE
    assert spec.sensitivity == ToolSensitivityLevel.WORKSPACE
    assert spec.cache_policy.cacheable is False
    assert spec.idempotency_policy.idempotent is True
    assert spec.retry_policy.max_attempts == 1
    assert spec.execution_backend == ToolExecutionBackendKind.IN_PROCESS
    assert spec.enabled is True
    assert spec.output_model is None


def test_tool_spec_output_model_is_enforced(tmp_path: Path) -> None:
    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="bad_output",
            description="Returns the wrong shape.",
            input_model=StrictInput,
            output_model=OutputModel,
            handler=lambda _args: {"missing": "field"},
        )
    )
    runtime = ToolRuntime(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=TraceWriter.create(tmp_path),
        workspace_root=tmp_path,
        policy_runtime=make_test_policy_runtime(tmp_path),
    )

    result = runtime.execute_tool_call(make_tool_call("bad_output", {"value": "x"}))

    assert result.ok is False
    assert result.error_code == "output_validation_error"


def test_strict_input_validation_rejects_unknown_fields(tmp_path: Path) -> None:
    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="strict_echo",
            description="Strict echo.",
            input_model=StrictInput,
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

    result = runtime.execute_tool_call(
        make_tool_call("strict_echo", {"value": "x", "ignored": "nope"})
    )

    assert result.ok is False
    assert result.error_code == "validation_error"


def test_strict_schema_export_recursively_forbids_extra_properties(tmp_path: Path) -> None:
    class Nested(BaseModel):
        model_config = ConfigDict(extra="forbid")
        name: str

    class WithNested(BaseModel):
        model_config = ConfigDict(extra="forbid")
        nested: Nested

    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="nested",
            description="Nested input.",
            input_model=WithNested,
            handler=lambda args: {"name": args.nested.name},
        )
    )

    schema = registry.to_openai_tools(strict=True)[0]["function"]["parameters"]

    assert schema["additionalProperties"] is False
    nested_schema = schema["$defs"]["Nested"]
    assert nested_schema["additionalProperties"] is False


def test_contract_policy_models_are_serializable() -> None:
    envelope = ToolOutputEnvelope(
        content={"ok": True},
        sensitivity=ToolSensitivityLevel.PUBLIC,
        metadata={"x": 1},
    )
    spec = ToolSpec(
        name="serializable",
        description="Serializable.",
        input_model=StrictInput,
        handler=lambda args: {"value": args.value},
        resource_resolver=lambda args, root: [
            ResourceRef("file", args.value, workspace_relative=True)
        ],
        cache_policy=ToolCachePolicy(cacheable=True, ttl_seconds=5),
        idempotency_policy=ToolIdempotencyPolicy(idempotent=True),
        retry_policy=ToolRetryPolicy(max_attempts=1),
    )

    assert envelope.model_dump(mode="json")["sensitivity"] == "public"
    assert spec.model_dump(mode="json")["cache_policy"]["ttl_seconds"] == 5

