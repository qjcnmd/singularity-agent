from __future__ import annotations

from pathlib import Path
from typing import Any

from miniharness.policy import Capability
from miniharness.tools.models import (
    PermissionLevel,
    ToolExecutionBackendKind,
    ToolSideEffectKind,
    ToolSpec,
)
from miniharness.tools.policy import ToolPolicy


class ToolRegistry:
    def __init__(self, project_root: Path, *, include_default_tools: bool = True) -> None:
        self.project_root = project_root.resolve()
        self._tools: dict[str, ToolSpec] = {}
        self._frozen = False
        if include_default_tools:
            from miniharness.tools.read_only import register_read_only_tools

            register_read_only_tools(self)

    def register(self, spec: ToolSpec) -> None:
        if self._frozen:
            raise RuntimeError("Tool registry is frozen.")
        if spec.name in self._tools:
            raise ValueError(f"Tool already registered: {spec.name}")
        self._validate_spec(spec)
        self._tools[spec.name] = spec

    def freeze(self) -> None:
        self._frozen = True

    def get(self, name: str) -> ToolSpec | None:
        return self._tools.get(name)

    def list(self) -> list[ToolSpec]:
        return list(self._tools.values())

    def to_openai_tools(self, *, strict: bool = False) -> list[dict[str, Any]]:
        tools: list[dict[str, Any]] = []
        for spec in self._tools.values():
            parameters = spec.input_model.model_json_schema()
            function: dict[str, Any] = {
                "name": spec.name,
                "description": spec.description,
                "parameters": self._parameters_schema(parameters, strict=strict),
                "x-miniharness-tool-version": spec.version,
                "x-miniharness-capabilities": [
                    capability.value for capability in spec.capabilities
                ],
            }
            if strict:
                function["strict"] = True
            tools.append({"type": "function", "function": function})
        return tools

    def openai_tools(self, *, strict: bool = False) -> list[dict[str, Any]]:
        return self.to_openai_tools(strict=strict)

    def schema_export(self, *, strict: bool = False) -> list[dict[str, Any]]:
        return self.to_openai_tools(strict=strict)

    def dispatch(self, tool_call: dict[str, Any]) -> dict[str, Any]:
        raise RuntimeError(
            "ToolRegistry.dispatch() cannot create a runtime. "
            "Use ToolRuntime.execute_tool_call() or dispatch_for_tests(..., runtime=...)."
        )

    def dispatch_for_tests(
        self,
        tool_call: dict[str, Any],
        *,
        runtime: Any | None = None,
    ) -> dict[str, Any]:
        if runtime is None:
            raise RuntimeError("dispatch_for_tests requires an explicit runtime.")
        return runtime.execute_tool_call(tool_call).model_dump(mode="json")

    def list_by_capability(self, capability: Capability) -> list[ToolSpec]:
        return [
            spec
            for spec in self._tools.values()
            if capability in spec.capabilities
        ]

    def list_by_side_effect(self, side_effect: ToolSideEffectKind) -> list[ToolSpec]:
        return [
            spec
            for spec in self._tools.values()
            if spec.side_effects == side_effect
        ]

    def list_policy_shapes(self) -> list[dict[str, Any]]:
        return [
            {
                "tool_name": spec.name,
                "version": spec.version,
                "operation": spec.operation.name if spec.operation else None,
                "capabilities": [capability.name for capability in spec.capabilities],
                "permission_level": spec.permission_level.value,
                "side_effects": spec.side_effects.value if spec.side_effects else None,
                "execution_backend": spec.execution_backend.value,
            }
            for spec in self._tools.values()
        ]

    @staticmethod
    def _parameters_schema(parameters: dict[str, Any], *, strict: bool) -> dict[str, Any]:
        parameters = dict(parameters)
        if strict:
            _forbid_additional_properties(parameters)
        return parameters

    @staticmethod
    def _validate_spec(spec: ToolSpec) -> None:
        if not spec.enabled:
            return
        if (
            spec.permission_level == PermissionLevel.WRITE
            and not spec.uses_mutation_runtime
            and spec.execution_backend
            not in {
                ToolExecutionBackendKind.DELEGATED_MUTATION_RUNTIME,
                ToolExecutionBackendKind.DELEGATED_EDIT_RUNTIME,
            }
        ):
            raise ValueError("Write tools must declare a mutation runtime backend.")
        if spec.execution_backend == ToolExecutionBackendKind.DELEGATED_EDIT_RUNTIME:
            if not spec.uses_edit_runtime:
                raise ValueError("Edit runtime tools must declare uses_edit_runtime=True.")
            if not spec.uses_mutation_runtime:
                raise ValueError("Edit runtime tools must delegate writes to mutation runtime.")
        if (
            spec.permission_level == PermissionLevel.SHELL
            and not spec.uses_command_runtime
            and spec.execution_backend
            not in {
                ToolExecutionBackendKind.DELEGATED_COMMAND_RUNTIME,
                ToolExecutionBackendKind.DELEGATED_VERIFICATION_RUNTIME,
            }
        ):
            raise ValueError("Shell tools must declare a command runtime backend.")
        if (
            spec.delegates_policy_constraints
            and spec.execution_backend != ToolExecutionBackendKind.DELEGATED_VERIFICATION_RUNTIME
        ):
            raise ValueError("Only verification runtime tools can delegate policy constraints.")


def _forbid_additional_properties(schema: Any) -> None:
    if isinstance(schema, dict):
        if schema.get("type") == "object" or "properties" in schema:
            schema["additionalProperties"] = False
        for value in schema.values():
            _forbid_additional_properties(value)
    elif isinstance(schema, list):
        for item in schema:
            _forbid_additional_properties(item)
