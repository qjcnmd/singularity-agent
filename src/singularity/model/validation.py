from __future__ import annotations

from singularity.model.models import (
    ModelCapabilities,
    ModelMessage,
    ModelRole,
    ModelToolCall,
    ModelToolParseStatus,
    ModelValidationResult,
    ToolChoiceMode,
    ToolChoicePolicy,
)
from singularity.tools.registry import ToolRegistry


class ModelResponseValidator:
    def __init__(self, registry: ToolRegistry) -> None:
        self.registry = registry

    def validate(
        self,
        *,
        assistant_message: ModelMessage | None,
        tool_calls: list[ModelToolCall],
        tool_choice: ToolChoicePolicy,
        allowed_tool_names: list[str],
        capabilities: ModelCapabilities | None = None,
    ) -> ModelValidationResult:
        errors: list[str] = []
        warnings: list[str] = []
        allowed = set(allowed_tool_names)
        request_tool_names = set(allowed_tool_names) - {
            spec.name for spec in self.registry.list_model_visible()
        }
        if assistant_message is None:
            errors.append("missing_assistant_message")
        elif assistant_message.role != ModelRole.ASSISTANT:
            errors.append("non_assistant_response")
        if assistant_message is not None and not assistant_message.text.strip() and not tool_calls:
            errors.append("empty_response")
        if tool_choice.mode == ToolChoiceMode.NONE and tool_calls:
            errors.append("tool_choice_none")
        if tool_choice.mode == ToolChoiceMode.REQUIRED and not tool_calls:
            errors.append("tool_choice_required")
        if len(tool_calls) > tool_choice.max_tool_calls:
            errors.append("max_tool_calls_exceeded")
        if capabilities is not None:
            if tool_calls and not capabilities.supports_tools:
                errors.append("provider_does_not_support_tools")
            if len(tool_calls) > 1 and not capabilities.supports_parallel_tool_calls:
                errors.append("provider_does_not_support_parallel_tool_calls")
        seen: set[str] = set()
        for call in tool_calls:
            if call.tool_call_id in seen:
                errors.append("duplicate_tool_call_id")
            seen.add(call.tool_call_id)
            if call.tool_name not in allowed:
                errors.append("unknown_tool")
            if self.registry.get(call.tool_name) is None and call.tool_name not in request_tool_names:
                errors.append("unknown_tool")
            if call.parse_status == ModelToolParseStatus.INVALID_JSON:
                errors.append("invalid_json")
            elif call.parse_status == ModelToolParseStatus.SCHEMA_MISMATCH:
                errors.append("schema_mismatch")
            elif call.parse_status == ModelToolParseStatus.UNKNOWN_TOOL:
                errors.append("unknown_tool")
            if call.validation_errors:
                warnings.extend(call.validation_errors)
        return ModelValidationResult(valid=not errors, errors=sorted(set(errors)), warnings=warnings)

