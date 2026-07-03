import json
from pathlib import Path
from typing import Any

from pydantic import BaseModel, ConfigDict

from singularity.jsonl_trace import JsonlTraceRecorder
from singularity.policy import Capability, OperationKind, ResourceRef
from singularity.tools import (
    PermissionLevel,
    ToolCachePolicy,
    ToolExecutionBackendKind,
    ToolExecutor,
    ToolIdempotencyPolicy,
    ToolOrigin,
    ToolOriginKind,
    ToolOutputEnvelope,
    ToolRegistry,
    ToolRetryPolicy,
    ToolSensitivityLevel,
    ToolSideEffectKind,
    ToolSpec,
)
from singularity.tools.policy import ToolPolicy
from tests.tool_executor_helpers import make_test_policy_engine


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
    component = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )

    result = component.execute_tool_call(make_tool_call("bad_output", {"value": "x"}))

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
    component = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )

    result = component.execute_tool_call(
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

    schema = registry.schema_export(strict=True)[0]["input_schema"]

    assert schema["additionalProperties"] is False
    nested_schema = schema["$defs"]["Nested"]
    assert nested_schema["additionalProperties"] is False


def test_schema_export_is_provider_neutral(tmp_path: Path) -> None:
    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="neutral",
            version="1.2.3",
            description="Neutral schema.",
            input_model=StrictInput,
            handler=lambda args: {"value": args.value},
            permission_level=PermissionLevel.READ_ONLY,
            cache_policy=ToolCachePolicy(cacheable=True, ttl_seconds=5),
            idempotency_policy=ToolIdempotencyPolicy(idempotent=True),
        )
    )

    exported = registry.schema_export(strict=True)

    assert len(exported) == 1
    tool = exported[0]
    assert tool["name"] == "neutral"
    assert tool["description"] == "Neutral schema."
    assert tool["input_schema"]["additionalProperties"] is False
    assert tool["permission_level"] == "read_only"
    assert tool["side_effects"] == "read_workspace"
    assert tool["cache_policy"]["cacheable"] is True
    assert tool["idempotency_policy"]["idempotent"] is True
    assert tool["origin"]["kind"] == "builtin"
    assert "type" not in tool
    assert "function" not in tool
    assert "parameters" not in tool


def test_schema_export_origin_uses_minimal_safe_projection(tmp_path: Path) -> None:
    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="plugin_tool",
            description="Plugin tool.",
            input_model=StrictInput,
            handler=lambda args: {"value": args.value},
        ),
        origin=ToolOrigin(
            kind=ToolOriginKind.PLUGIN,
            plugin_id="sample_plugin",
            local_tool_name="local",
            exposed_name="sample_plugin__local",
            manifest_hash="manifest-hash",
            source_path=str(tmp_path / ".singularity" / "plugins" / "sample_plugin"),
            required_permissions=("read_workspace",),
            approved_permissions=("read_workspace",),
            activation_hash="activation-hash",
            schema_digest="schema-digest",
        ),
    )

    origin = registry.schema_export()[0]["origin"]

    assert origin == {
        "kind": "plugin",
        "plugin_id": "sample_plugin",
        "exposed_name": "sample_plugin__local",
    }


def test_schema_export_filters_non_model_visible_tools(tmp_path: Path) -> None:
    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="visible",
            description="Visible tool.",
            input_model=StrictInput,
            handler=lambda args: {"value": args.value},
        )
    )
    registry.register(
        ToolSpec(
            name="disabled",
            description="Disabled tool.",
            input_model=StrictInput,
            handler=lambda args: {"value": args.value},
            enabled=False,
        )
    )
    registry.register(
        ToolSpec(
            name="not_admitted",
            description="Not admitted tool.",
            input_model=StrictInput,
            handler=lambda args: {"value": args.value},
        ),
        admitted=False,
        admission_reason="failed_check",
    )

    exported_names = [tool["name"] for tool in registry.schema_export()]

    assert exported_names == ["visible"]


def test_to_openai_tools_filters_non_model_visible_tools(tmp_path: Path) -> None:
    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="visible",
            description="Visible tool.",
            input_model=StrictInput,
            handler=lambda args: {"value": args.value},
        )
    )
    registry.register(
        ToolSpec(
            name="disabled",
            description="Disabled tool.",
            input_model=StrictInput,
            handler=lambda args: {"value": args.value},
            enabled=False,
        )
    )
    registry.register(
        ToolSpec(
            name="not_admitted",
            description="Not admitted tool.",
            input_model=StrictInput,
            handler=lambda args: {"value": args.value},
        ),
        admitted=False,
        admission_reason="failed_check",
    )

    exported_names = [tool["function"]["name"] for tool in registry.to_openai_tools()]

    assert exported_names == ["visible"]


def test_to_openai_tools_keeps_openai_function_shape(tmp_path: Path) -> None:
    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="openai_visible",
            description="OpenAI visible tool.",
            input_model=StrictInput,
            handler=lambda args: {"value": args.value},
        )
    )

    exported = registry.to_openai_tools(strict=True)

    assert exported == [
        {
            "type": "function",
            "function": {
                "name": "openai_visible",
                "description": "OpenAI visible tool.",
                "parameters": exported[0]["function"]["parameters"],
                "strict": True,
            },
        }
    ]
    assert exported[0]["function"]["parameters"]["additionalProperties"] is False


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

