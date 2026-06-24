from __future__ import annotations

from pathlib import Path
from typing import Any

from singularity.policy import Capability
from singularity.tools.models import (
    PermissionLevel,
    RegisteredToolRecord,
    ToolExecutionBackendKind,
    ToolOrigin,
    ToolOriginKind,
    ToolSideEffectKind,
    ToolSpec,
)


class ToolRegistry:
    def __init__(self, project_root: Path, *, include_default_tools: bool = True) -> None:
        self.project_root = project_root.resolve()
        self._tools: dict[str, ToolSpec] = {}
        self._records: dict[str, RegisteredToolRecord] = {}
        self._frozen = False
        if include_default_tools:
            from singularity.tools.read_only import register_read_only_tools

            register_read_only_tools(self)

    def register(
        self,
        spec: ToolSpec,
        *,
        origin: ToolOrigin | None = None,
        admitted: bool = True,
        admission_reason: str = "registered",
        diagnostics: tuple[str, ...] = (),
        metadata: dict[str, Any] | None = None,
    ) -> None:
        if self._frozen:
            raise RuntimeError("Tool registry is frozen.")
        if spec.name in self._tools:
            raise ValueError(f"Tool already registered: {spec.name}")
        self._validate_spec(spec)
        self._tools[spec.name] = spec
        self._records[spec.name] = RegisteredToolRecord(
            spec=spec,
            origin=origin or ToolOrigin(),
            admitted=admitted,
            admission_reason=admission_reason,
            diagnostics=diagnostics,
            metadata=dict(metadata or {}),
        )

    def freeze(self) -> None:
        self._frozen = True

    def get(self, name: str) -> ToolSpec | None:
        return self._tools.get(name)

    def get_record(self, name: str) -> RegisteredToolRecord | None:
        return self._records.get(name)

    def list(self) -> list[ToolSpec]:
        return list(self._tools.values())

    def list_records(
        self,
        *,
        origin: ToolOriginKind | str | None = None,
        include_disabled: bool = True,
        admitted_only: bool = False,
    ) -> list[RegisteredToolRecord]:
        records = list(self._records.values())
        if origin is not None:
            origin_kind = origin if isinstance(origin, ToolOriginKind) else ToolOriginKind(str(origin))
            records = [record for record in records if record.origin.kind == origin_kind]
        if not include_disabled:
            records = [record for record in records if record.spec.enabled]
        if admitted_only:
            records = [record for record in records if record.admitted]
        return records

    def list_model_visible(self) -> list[ToolSpec]:
        return [
            record.spec
            for record in self.list_records(include_disabled=False, admitted_only=True)
        ]

    def to_openai_tools(self, *, strict: bool = False) -> list[dict[str, Any]]:
        tools: list[dict[str, Any]] = []
        for spec in self.list_model_visible():
            parameters = spec.input_model.model_json_schema()
            function: dict[str, Any] = {
                "name": spec.name,
                "description": spec.description,
                "parameters": self._parameters_schema(parameters, strict=strict),
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
            "ToolRegistry.dispatch() cannot create an executor. "
            "Use ToolExecutor.execute_tool_call() or dispatch_for_tests(..., executor=...)."
        )

    def dispatch_for_tests(
        self,
        tool_call: dict[str, Any],
        *,
        executor: Any | None = None,
    ) -> dict[str, Any]:
        if executor is None:
            raise RuntimeError("dispatch_for_tests requires an explicit executor.")
        return executor.execute_tool_call(tool_call).model_dump(mode="json")

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
            and not spec.uses_mutation_manager
            and spec.execution_backend
            not in {
                ToolExecutionBackendKind.DELEGATED_MUTATION_MANAGER,
                ToolExecutionBackendKind.DELEGATED_EDIT_EXECUTOR,
            }
        ):
            raise ValueError("Write tools must declare a mutation manager backend.")
        if spec.execution_backend == ToolExecutionBackendKind.DELEGATED_EDIT_EXECUTOR:
            if not spec.uses_edit_executor:
                raise ValueError("EditExecutor tools must declare uses_edit_executor=True.")
            if not spec.uses_mutation_manager:
                raise ValueError("EditExecutor tools must delegate writes to mutation manager.")
        if (
            spec.permission_level == PermissionLevel.SHELL
            and not spec.uses_command_executor
            and spec.execution_backend
            not in {
                ToolExecutionBackendKind.DELEGATED_COMMAND_EXECUTOR,
                ToolExecutionBackendKind.DELEGATED_VERIFICATION_RUNNER,
            }
        ):
            raise ValueError("Shell tools must declare a command executor backend.")
        if (
            spec.delegates_policy_constraints
            and spec.execution_backend != ToolExecutionBackendKind.DELEGATED_VERIFICATION_RUNNER
        ):
            raise ValueError("Only verification runner tools can delegate policy constraints.")


def _forbid_additional_properties(schema: Any) -> None:
    if isinstance(schema, dict):
        if schema.get("type") == "object" or "properties" in schema:
            schema["additionalProperties"] = False
        for value in schema.values():
            _forbid_additional_properties(value)
    elif isinstance(schema, list):
        for item in schema:
            _forbid_additional_properties(item)
