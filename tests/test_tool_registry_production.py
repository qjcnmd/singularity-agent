from pathlib import Path

import pytest
from pydantic import BaseModel, ConfigDict

from singularity.policy import Capability, OperationKind
from singularity.tools import (
    PermissionLevel,
    ToolExecutionBackendKind,
    ToolRegistry,
    ToolSpec,
    ToolSideEffectKind,
)


class EmptyInput(BaseModel):
    model_config = ConfigDict(extra="forbid")


def handler(_args: EmptyInput) -> dict[str, str]:
    return {"ok": "yes"}


def test_duplicate_tool_is_rejected(tmp_path: Path) -> None:
    registry = ToolRegistry(tmp_path, include_default_tools=False)
    spec = ToolSpec(name="same", description="same", input_model=EmptyInput, handler=handler)

    registry.register(spec)

    with pytest.raises(ValueError, match="already registered"):
        registry.register(spec)


def test_invalid_write_tool_without_mutation_backend_is_rejected(tmp_path: Path) -> None:
    registry = ToolRegistry(tmp_path, include_default_tools=False)

    with pytest.raises(ValueError, match="mutation runtime"):
        registry.register(
            ToolSpec(
                name="bad_write",
                description="bad",
                input_model=EmptyInput,
                handler=handler,
                permission_level=PermissionLevel.WRITE,
                side_effects=ToolSideEffectKind.MUTATE_WORKSPACE,
                capabilities=(Capability.MUTATE_WORKSPACE,),
                operation=OperationKind.MUTATE_FILE,
            )
        )


def test_edit_backend_requires_edit_runtime_and_mutation_delegation(tmp_path: Path) -> None:
    registry = ToolRegistry(tmp_path, include_default_tools=False)

    with pytest.raises(ValueError, match="uses_edit_runtime"):
        registry.register(
            ToolSpec(
                name="bad_edit",
                description="bad",
                input_model=EmptyInput,
                handler=handler,
                permission_level=PermissionLevel.WRITE,
                side_effects=ToolSideEffectKind.MUTATE_WORKSPACE,
                capabilities=(Capability.MUTATE_WORKSPACE,),
                operation=OperationKind.MUTATE_FILE,
                execution_backend=ToolExecutionBackendKind.DELEGATED_EDIT_RUNTIME,
                uses_mutation_runtime=True,
            )
        )

    with pytest.raises(ValueError, match="mutation runtime"):
        registry.register(
            ToolSpec(
                name="bad_edit_no_mutation",
                description="bad",
                input_model=EmptyInput,
                handler=handler,
                permission_level=PermissionLevel.WRITE,
                side_effects=ToolSideEffectKind.MUTATE_WORKSPACE,
                capabilities=(Capability.MUTATE_WORKSPACE,),
                operation=OperationKind.MUTATE_FILE,
                execution_backend=ToolExecutionBackendKind.DELEGATED_EDIT_RUNTIME,
                uses_edit_runtime=True,
            )
        )

    registry.register(
        ToolSpec(
            name="good_edit",
            description="good",
            input_model=EmptyInput,
            handler=handler,
            permission_level=PermissionLevel.WRITE,
            side_effects=ToolSideEffectKind.MUTATE_WORKSPACE,
            capabilities=(Capability.MUTATE_WORKSPACE,),
            operation=OperationKind.MUTATE_FILE,
            execution_backend=ToolExecutionBackendKind.DELEGATED_EDIT_RUNTIME,
            uses_edit_runtime=True,
            uses_mutation_runtime=True,
        )
    )

    assert registry.get("good_edit") is not None


def test_invalid_shell_tool_without_command_backend_is_rejected(tmp_path: Path) -> None:
    registry = ToolRegistry(tmp_path, include_default_tools=False)

    with pytest.raises(ValueError, match="command runtime"):
        registry.register(
            ToolSpec(
                name="bad_shell",
                description="bad",
                input_model=EmptyInput,
                handler=handler,
                permission_level=PermissionLevel.SHELL,
                side_effects=ToolSideEffectKind.EXECUTE_COMMAND,
                capabilities=(Capability.EXECUTE_COMMAND,),
                operation=OperationKind.EXECUTE_COMMAND,
            )
        )


def test_only_verification_backend_can_delegate_policy_constraints(tmp_path: Path) -> None:
    registry = ToolRegistry(tmp_path, include_default_tools=False)

    with pytest.raises(ValueError, match="verification runtime"):
        registry.register(
            ToolSpec(
                name="delegating_command",
                description="invalid delegate",
                input_model=EmptyInput,
                handler=handler,
                permission_level=PermissionLevel.SHELL,
                side_effects=ToolSideEffectKind.EXECUTE_COMMAND,
                capabilities=(Capability.EXECUTE_COMMAND,),
                operation=OperationKind.EXECUTE_COMMAND,
                execution_backend=ToolExecutionBackendKind.DELEGATED_COMMAND_RUNTIME,
                uses_command_runtime=True,
                delegates_policy_constraints=True,
            )
        )


def test_registry_freeze_prevents_late_registration(tmp_path: Path) -> None:
    registry = ToolRegistry(tmp_path, include_default_tools=False)

    registry.freeze()

    with pytest.raises(RuntimeError, match="frozen"):
        registry.register(
            ToolSpec(name="late", description="late", input_model=EmptyInput, handler=handler)
        )


def test_registry_indexes_capabilities_and_side_effects(tmp_path: Path) -> None:
    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="readish",
            description="read",
            input_model=EmptyInput,
            handler=handler,
            capabilities=(Capability.READ_WORKSPACE, Capability.LIST_DIRECTORY),
            side_effects=ToolSideEffectKind.READ_WORKSPACE,
        )
    )

    assert [spec.name for spec in registry.list_by_capability(Capability.LIST_DIRECTORY)] == [
        "readish"
    ]
    assert [spec.name for spec in registry.list_by_side_effect(ToolSideEffectKind.READ_WORKSPACE)] == [
        "readish"
    ]
    shapes = registry.list_policy_shapes()
    assert shapes[0]["tool_name"] == "readish"
    assert shapes[0]["capabilities"] == ["READ_WORKSPACE", "LIST_DIRECTORY"]


def test_dispatch_convenience_cannot_create_runtime(tmp_path: Path) -> None:
    registry = ToolRegistry(tmp_path, include_default_tools=False)

    with pytest.raises(RuntimeError, match="dispatch_for_tests"):
        registry.dispatch({"function": {"name": "missing", "arguments": "{}"}})


def test_openai_schema_includes_safe_metadata(tmp_path: Path) -> None:
    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="meta",
            version="1.2.3",
            description="meta",
            input_model=EmptyInput,
            handler=handler,
            capabilities=(Capability.READ_WORKSPACE,),
            execution_backend=ToolExecutionBackendKind.IN_PROCESS,
        )
    )

    tool = registry.to_openai_tools(strict=True)[0]["function"]

    assert tool["x-singularity-tool-version"] == "1.2.3"
    assert tool["x-singularity-capabilities"] == ["READ_WORKSPACE"]
    assert "policy" not in tool
