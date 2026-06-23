from __future__ import annotations

import hashlib
import json
from typing import Any

from pydantic import ValidationError

from singularity.model.models import (
    ModelToolCall,
    ModelToolParseStatus,
    ModelToolSchema,
)
from singularity.tools.models import PermissionLevel
from singularity.tools.registry import ToolRegistry


class ModelToolRenderer:
    def __init__(self, registry: ToolRegistry) -> None:
        self.registry = registry

    def render(self, *, allowed_tool_names: list[str] | None = None, strict: bool = False) -> list[ModelToolSchema]:
        allowed = (
            {spec.name for spec in self.registry.list()}
            if allowed_tool_names is None
            else set(allowed_tool_names)
        )
        schemas: list[ModelToolSchema] = []
        for spec in sorted(self.registry.list(), key=lambda item: item.name):
            if spec.name not in allowed:
                continue
            schemas.append(
                ModelToolSchema(
                    name=spec.name,
                    description=spec.description,
                    parameters_schema=self.registry._parameters_schema(
                        spec.input_model.model_json_schema(), strict=strict
                    ),
                    capability_tags=[_capability_for_permission(spec.permission_level)],
                    risk_tags=list(spec.risk_tags),
                    metadata={
                        "version": spec.version,
                        "permission_level": spec.permission_level.value,
                        "cacheable": spec.cacheable,
                        "idempotent": spec.idempotent,
                        "strict": strict,
                    },
                )
            )
        return schemas

    def to_provider_tools(self, tools: list[ModelToolSchema], *, strict: bool = False) -> list[dict[str, Any]]:
        provider_tools: list[dict[str, Any]] = []
        for tool in tools:
            function: dict[str, Any] = {
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.parameters_schema,
            }
            if strict:
                function["strict"] = True
            provider_tools.append({"type": "function", "function": function})
        return provider_tools

    @staticmethod
    def schema_hash(tools: list[ModelToolSchema]) -> str:
        payload = [tool.to_dict() for tool in tools]
        text = json.dumps(payload, ensure_ascii=False, sort_keys=True, default=str)
        return hashlib.sha256(text.encode("utf-8")).hexdigest()


class ToolCallNormalizer:
    def __init__(self, registry: ToolRegistry) -> None:
        self.registry = registry

    def normalize(
        self,
        tool_call: dict[str, Any],
        *,
        allowed_tool_names: list[str] | None = None,
        seen_ids: set[str] | None = None,
    ) -> ModelToolCall:
        errors: list[str] = []
        function = tool_call.get("function") or {}
        tool_call_id = str(tool_call.get("id") or "")
        tool_name = str(function.get("name") or "")
        raw_arguments_value = function.get("arguments", "{}")
        allowed = (
            {spec.name for spec in self.registry.list()}
            if allowed_tool_names is None
            else set(allowed_tool_names)
        )
        if not tool_call_id:
            errors.append("missing_tool_call_id")
            tool_call_id = "<missing>"
        if seen_ids is not None:
            if tool_call_id in seen_ids:
                errors.append("duplicate_tool_call_id")
            seen_ids.add(tool_call_id)
        if not tool_name:
            errors.append("missing_tool_name")
        if tool_name not in allowed or self.registry.get(tool_name) is None:
            return ModelToolCall(
                tool_call_id=tool_call_id,
                tool_name=tool_name or "<unknown>",
                arguments={},
                raw_arguments=self._raw_arguments(raw_arguments_value),
                parse_status=ModelToolParseStatus.UNKNOWN_TOOL,
                validation_errors=errors or ["unknown_tool"],
                provider_metadata={"raw_tool_call": tool_call},
            )
        raw_arguments = self._raw_arguments(raw_arguments_value)
        try:
            parsed = json.loads(raw_arguments)
        except json.JSONDecodeError as exc:
            return ModelToolCall(
                tool_call_id=tool_call_id,
                tool_name=tool_name,
                arguments={},
                raw_arguments=raw_arguments,
                parse_status=ModelToolParseStatus.INVALID_JSON,
                validation_errors=[*errors, str(exc)],
                provider_metadata={"raw_tool_call": tool_call},
            )
        if not isinstance(parsed, dict):
            return ModelToolCall(
                tool_call_id=tool_call_id,
                tool_name=tool_name,
                arguments={},
                raw_arguments=raw_arguments,
                parse_status=ModelToolParseStatus.SCHEMA_MISMATCH,
                validation_errors=[*errors, "arguments_not_object"],
                provider_metadata={"raw_tool_call": tool_call},
            )
        spec = self.registry.get(tool_name)
        try:
            assert spec is not None
            validated = spec.input_model.model_validate(parsed)
        except (ValidationError, AssertionError) as exc:
            details = exc.errors() if isinstance(exc, ValidationError) else [str(exc)]
            return ModelToolCall(
                tool_call_id=tool_call_id,
                tool_name=tool_name,
                arguments=parsed,
                raw_arguments=raw_arguments,
                parse_status=ModelToolParseStatus.SCHEMA_MISMATCH,
                validation_errors=[*errors, json.dumps(details, ensure_ascii=False, default=str)],
                provider_metadata={"raw_tool_call": tool_call},
            )
        normalized_args = validated.model_dump(mode="json")
        return ModelToolCall(
            tool_call_id=tool_call_id,
            tool_name=tool_name,
            arguments=normalized_args,
            raw_arguments=raw_arguments,
            parse_status=ModelToolParseStatus.VALID if not errors else ModelToolParseStatus.SCHEMA_MISMATCH,
            validation_errors=errors,
            provider_metadata={"raw_tool_call": tool_call},
        )

    @staticmethod
    def _raw_arguments(value: Any) -> str:
        if isinstance(value, str):
            return value
        if isinstance(value, dict):
            return json.dumps(value, ensure_ascii=False, sort_keys=True)
        return json.dumps(value, ensure_ascii=False, sort_keys=True, default=str)


def _capability_for_permission(permission: PermissionLevel) -> str:
    mapping = {
        PermissionLevel.READ_ONLY: "read_workspace",
        PermissionLevel.WRITE: "mutate_workspace",
        PermissionLevel.SHELL: "execute_command",
        PermissionLevel.GIT: "git",
    }
    return mapping.get(permission, permission.value)
