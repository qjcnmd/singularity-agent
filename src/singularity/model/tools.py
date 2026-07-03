from __future__ import annotations

import hashlib
import json
from types import UnionType
from typing import Any, Union, get_args, get_origin

from pydantic import ValidationError

from singularity.model.models import (
    ModelToolCall,
    ModelToolParseStatus,
    ModelToolSchema,
)
from singularity.model.openai_format import model_tool_to_openai
from singularity.tools.models import PermissionLevel
from singularity.tools.registry import ToolRegistry


class ModelToolRenderer:
    def __init__(self, registry: ToolRegistry) -> None:
        self.registry = registry

    def render(self, *, allowed_tool_names: list[str] | None = None, strict: bool = False) -> list[ModelToolSchema]:
        visible = self.registry.list_model_visible()
        visible_names = {spec.name for spec in visible}
        allowed = visible_names if allowed_tool_names is None else set(allowed_tool_names) & visible_names
        schemas: list[ModelToolSchema] = []
        for spec in sorted(visible, key=lambda item: item.name):
            if spec.name not in allowed:
                continue
            record = self.registry.get_record(spec.name)
            metadata = {
                "version": spec.version,
                "permission_level": spec.permission_level.value,
                "cacheable": spec.cacheable,
                "idempotent": spec.idempotent,
                "strict": strict,
            }
            if record is not None:
                metadata["origin"] = record.origin.kind.value
                if record.origin.plugin_id:
                    metadata["plugin_id"] = record.origin.plugin_id
            schemas.append(
                ModelToolSchema(
                    name=spec.name,
                    description=spec.description,
                    parameters_schema=self.registry._parameters_schema(
                        spec.input_model.model_json_schema(), strict=strict
                    ),
                    capability_tags=[_capability_for_permission(spec.permission_level)],
                    risk_tags=list(spec.risk_tags),
                    metadata=metadata,
                )
            )
        return schemas

    def to_provider_tools(self, tools: list[ModelToolSchema], *, strict: bool = False) -> list[dict[str, Any]]:
        return [model_tool_to_openai(tool, strict=strict) for tool in tools]

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
        visible = {spec.name for spec in self.registry.list_model_visible()}
        allowed = visible if allowed_tool_names is None else set(allowed_tool_names) & visible
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
            parsed = coerce_json_string_fields(parsed, spec.input_model)
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


def coerce_json_string_fields(payload: dict[str, Any], input_model: Any) -> dict[str, Any]:
    fields = getattr(input_model, "model_fields", {})
    if not fields:
        return payload
    coerced = dict(payload)
    for name, field in fields.items():
        value = coerced.get(name)
        if not isinstance(value, str) or not _expects_json_container(field.annotation):
            continue
        text = value.strip()
        if not text or text[0] not in "[{":
            continue
        try:
            coerced[name] = json.loads(text)
        except json.JSONDecodeError:
            continue
    return coerced


def _expects_json_container(annotation: Any) -> bool:
    origin = get_origin(annotation)
    if origin in {list, dict} or annotation in {list, dict}:
        return True
    if origin in {Union, UnionType}:
        return any(_expects_json_container(arg) for arg in get_args(annotation))
    return False
