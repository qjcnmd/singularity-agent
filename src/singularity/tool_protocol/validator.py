from __future__ import annotations

import json
from typing import Any
from uuid import uuid4

from pydantic import ValidationError

from singularity.model.models import (
    ModelCapabilities,
    ModelToolParseStatus,
    ToolChoiceMode,
    ToolChoicePolicy,
)
from singularity.model.tools import coerce_json_string_fields
from singularity.tool_protocol.errors import ToolProtocolValidationError
from singularity.tool_protocol.models import (
    ToolCallBatch,
    ToolCallEnvelope,
    ToolCallFailureKind,
    ToolCallPhase,
    ToolProtocolValidationResult,
)
from singularity.tool_protocol.scheduler import ToolProtocolScheduler
from singularity.tools.registry import ToolRegistry


class ToolProtocolValidator:
    def __init__(self, registry: ToolRegistry, *, protocol_version: str = "1.0") -> None:
        self.registry = registry
        self.protocol_version = protocol_version
        self.scheduler = ToolProtocolScheduler(registry)

    def validate_assistant_message(
        self,
        *,
        run_id: str,
        session_id: str,
        task_id: str,
        phase_id: str,
        model_request_id: str,
        model_response_id: str,
        assistant_message: dict[str, Any],
        assistant_message_id: str | None = None,
        allowed_tool_names: list[str] | None = None,
        tool_choice: ToolChoicePolicy | None = None,
        provider_capabilities: ModelCapabilities | None = None,
        max_tool_calls: int | None = None,
    ) -> ToolProtocolValidationResult:
        if assistant_message.get("role") != "assistant":
            raise ToolProtocolValidationError("Tool protocol only accepts assistant messages.")

        raw_calls_value = assistant_message.get("tool_calls") or []
        if not isinstance(raw_calls_value, list):
            raise ToolProtocolValidationError("tool_calls_must_be_list")
        raw_calls = list(raw_calls_value)
        resolved_tool_choice = tool_choice or ToolChoicePolicy()
        resolved_max_tool_calls = (
            max_tool_calls
            if max_tool_calls is not None
            else resolved_tool_choice.max_tool_calls
        )
        errors: list[str] = []
        warnings: list[str] = []

        if resolved_tool_choice.mode == ToolChoiceMode.NONE and raw_calls:
            errors.append(ToolCallFailureKind.protocol_violation.value)
        if resolved_tool_choice.mode == ToolChoiceMode.REQUIRED and not raw_calls:
            errors.append(ToolCallFailureKind.protocol_violation.value)
        if resolved_max_tool_calls is not None and len(raw_calls) > resolved_max_tool_calls:
            errors.append("max_tool_calls_exceeded")

        seen: set[str] = set()
        envelopes: list[ToolCallEnvelope] = []
        registry_tool_names = [spec.name for spec in self.registry.list() if spec.enabled]
        allowed_names = list(allowed_tool_names) if allowed_tool_names is not None else registry_tool_names
        if resolved_tool_choice.mode == ToolChoiceMode.ALLOWED_TOOLS:
            allowed_names = list(resolved_tool_choice.allowed_tool_names or allowed_names)
        elif resolved_tool_choice.mode == ToolChoiceMode.SPECIFIC_TOOL:
            allowed_names = [str(resolved_tool_choice.tool_name or "")]
        supports_parallel = len(raw_calls) > 1
        if provider_capabilities is not None and not provider_capabilities.supports_parallel_tool_calls:
            supports_parallel = False
            if len(raw_calls) > 1:
                warnings.append("provider_parallel_unsupported_forced_sequential")
        message_id = assistant_message_id or str(assistant_message.get("id") or f"assistant_{uuid4().hex[:12]}")

        for raw_call in raw_calls:
            if not isinstance(raw_call, dict):
                raise ToolProtocolValidationError("tool_call_must_be_object")
            envelope = self._validate_tool_call(
                raw_call,
                run_id=run_id,
                session_id=session_id,
                task_id=task_id,
                phase_id=phase_id,
                model_request_id=model_request_id,
                model_response_id=model_response_id,
                assistant_message_id=message_id,
                allowed_tool_names=allowed_names,
            )
            if not envelope.tool_call_id:
                raise ToolProtocolValidationError(ToolCallFailureKind.missing_tool_call_id.value)
            if envelope.tool_call_id in seen:
                raise ToolProtocolValidationError(ToolCallFailureKind.duplicate_tool_call_id.value)
            seen.add(envelope.tool_call_id)
            if envelope.tool_name not in allowed_names:
                envelope.validation_errors.append(ToolCallFailureKind.disallowed_tool.value)
                envelope.phase = ToolCallPhase.REJECTED
            if envelope.validation_errors:
                errors.extend(envelope.validation_errors)
            envelopes.append(envelope)

        batch = ToolCallBatch(
            batch_id=f"tool_batch_{uuid4().hex[:12]}",
            run_id=run_id,
            session_id=session_id,
            task_id=task_id,
            phase_id=phase_id,
            model_request_id=model_request_id,
            model_response_id=model_response_id,
            assistant_message=assistant_message,
            tool_calls=envelopes,
            supports_parallel_execution=supports_parallel,
            max_tool_calls=resolved_max_tool_calls or 0,
        )
        return ToolProtocolValidationResult(
            valid=not errors,
            batch=batch,
            errors=errors,
            warnings=warnings,
            assistant_message_valid=not errors,
            protocol_version=self.protocol_version,
        )

    def schedule(self, batch: ToolCallBatch) -> Any:
        return self.scheduler.schedule(batch)

    def _validate_tool_call(
        self,
        raw_call: dict[str, Any],
        *,
        run_id: str,
        session_id: str,
        task_id: str,
        phase_id: str,
        model_request_id: str,
        model_response_id: str,
        assistant_message_id: str,
        allowed_tool_names: list[str],
    ) -> ToolCallEnvelope:
        function = raw_call.get("function") if isinstance(raw_call, dict) else None
        tool_name = str((function or {}).get("name") or "")
        raw_arguments_value = (function or {}).get("arguments") or {}
        if isinstance(raw_arguments_value, dict):
            raw_arguments = json.dumps(
                raw_arguments_value,
                ensure_ascii=False,
                sort_keys=True,
                separators=(",", ":"),
            )
        else:
            raw_arguments = str(raw_arguments_value)
        errors: list[str] = []
        parse_status = ModelToolParseStatus.VALID
        parsed_arguments: dict[str, Any] = {}

        try:
            parsed = json.loads(raw_arguments)
        except json.JSONDecodeError:
            parsed = {}
            parse_status = ModelToolParseStatus.INVALID_JSON
            errors.append(ToolCallFailureKind.invalid_json.value)
        if not isinstance(parsed, dict):
            parsed = {}
            parse_status = ModelToolParseStatus.SCHEMA_MISMATCH
            errors.append(ToolCallFailureKind.arguments_not_object.value)
        parsed_arguments = parsed

        spec = self.registry.get(tool_name)
        if spec is None:
            parse_status = ModelToolParseStatus.UNKNOWN_TOOL
            errors.append(ToolCallFailureKind.unknown_tool.value)
        elif not errors:
            try:
                parsed_arguments = coerce_json_string_fields(parsed_arguments, spec.input_model)
                validated = spec.input_model.model_validate(parsed_arguments)
                parsed_arguments = validated.model_dump(mode="json")
            except ValidationError:
                parse_status = ModelToolParseStatus.SCHEMA_MISMATCH
                errors.append(ToolCallFailureKind.schema_mismatch.value)

        return ToolCallEnvelope(
            protocol_version=self.protocol_version,
            run_id=run_id,
            session_id=session_id,
            task_id=task_id,
            phase_id=phase_id,
            model_request_id=model_request_id,
            model_response_id=model_response_id,
            assistant_message_id=assistant_message_id,
            tool_call_id=str(raw_call.get("id") or ""),
            tool_name=tool_name,
            raw_arguments=raw_arguments,
            parsed_arguments=parsed_arguments,
            normalized_arguments=dict(parsed_arguments),
            allowed_tool_names=allowed_tool_names,
            parse_status=parse_status,
            validation_errors=errors,
            metadata={"tool_version": spec.version if spec is not None else ""},
            phase=ToolCallPhase.REJECTED if errors else ToolCallPhase.VALIDATED,
        )
