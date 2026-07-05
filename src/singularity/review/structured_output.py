from __future__ import annotations

"""Shared model-assisted review output boundary.

The ordered fallback path is Structured Outputs / JSON Schema, strict tool
calling with pinned tool choice, json_mode, then rule-only fallback. Schema
validation uses one bounded retry for parse/schema failures; transient provider
errors are handled by the model layer's bounded retry and exponential backoff
with jitter before this boundary records graceful degradation metadata.
"""

import hashlib
import json
from dataclasses import dataclass, field
from typing import Any

from pydantic import BaseModel, ValidationError

from singularity.model.models import (
    ContentBlock,
    ModelBudget,
    ModelMessage,
    ModelPreferences,
    ModelRole,
    ModelToolCall,
    ModelToolParseStatus,
    ModelToolSchema,
    ModelTurnRequest,
    ModelTurnStatus,
    ToolChoiceMode,
    ToolChoicePolicy,
)
from singularity.utils.attributes import nested_getattr

OUTPUT_MODE_STRUCTURED = "structured_output"
OUTPUT_MODE_TOOL = "forced_tool_call"
OUTPUT_MODE_JSON = "json_mode"
OUTPUT_MODE_RULE_ONLY = "rule_only"

RETRY_REASON_NONE = "none"
RETRY_REASON_JSON_PARSE_ERROR = "json_parse_error"
RETRY_REASON_SCHEMA_VALIDATION_ERROR = "schema_validation_error"
RETRY_REASON_TOOL_CALL_PARSE_ERROR = "tool_call_parse_error"
RETRY_REASON_PROVIDER_ERROR = "provider_error"
RETRY_REASON_BUSINESS_RULE_VALIDATION_FAILED = "business_rule_validation_failed"


@dataclass(frozen=True)
class ReviewOutputResult:
    status: str
    payload: dict[str, Any] = field(default_factory=dict)
    error: str | None = None
    metadata: dict[str, Any] = field(default_factory=dict)


@dataclass(frozen=True)
class BusinessRuleViolation(ValueError):
    message: str

    def __str__(self) -> str:
        return self.message


class JsonParseError(ValueError):
    pass


class ToolCallParseError(ValueError):
    pass


def call_review_output(
    *,
    model_runner: Any,
    request_base: dict[str, Any],
    prompt: str,
    output_model: type[BaseModel],
    schema_name: str,
    tool_name: str,
    tool_description: str,
    business_validator: Any | None = None,
    max_validation_retries: int = 1,
) -> ReviewOutputResult:
    if model_runner is None:
        return _fallback_result("model_runner_missing")
    modes = [
        OUTPUT_MODE_STRUCTURED,
        OUTPUT_MODE_TOOL,
        OUTPUT_MODE_JSON,
    ]
    unsupported: list[str] = []
    last_error = ""
    for mode in modes:
        if not _supports_mode(model_runner, mode):
            unsupported.append(mode)
            continue
        retry_count = 0
        retry_reason = RETRY_REASON_NONE
        while retry_count <= max_validation_retries:
            try:
                request = _build_request(
                    request_base=request_base,
                    prompt=prompt,
                    mode=mode,
                    output_model=output_model,
                    schema_name=schema_name,
                    tool_name=tool_name,
                    tool_description=tool_description,
                    retry_count=retry_count,
                )
                result = model_runner.run_turn(request)
            except Exception as exc:
                return _fallback_result(
                    str(exc),
                    output_mode=OUTPUT_MODE_RULE_ONLY,
                    retry_count=retry_count,
                    fallback_reason="provider_error",
                    retry_reason=RETRY_REASON_PROVIDER_ERROR,
                )
            if getattr(result, "status", None) != ModelTurnStatus.SUCCESS:
                return _fallback_result(
                    str(getattr(result, "error", None) or getattr(result, "status", None)),
                    output_mode=OUTPUT_MODE_RULE_ONLY,
                    retry_count=retry_count,
                    fallback_reason="provider_error",
                    retry_reason=RETRY_REASON_PROVIDER_ERROR,
                )
            try:
                payload = _payload_from_result(result, mode=mode, tool_name=tool_name)
                validated = output_model.model_validate(payload)
                parsed = validated.model_dump(mode="json")
                if business_validator is not None:
                    business_validator(parsed)
                return ReviewOutputResult(
                    status="ok",
                    payload=parsed,
                    metadata={
                        "output_mode": mode,
                        "schema_validation_passed": True,
                        "retry_count": retry_count,
                        "retry_reason": retry_reason,
                        "fallback_reason": "",
                        "schema_hash": _schema_hash(output_model),
                    },
                )
            except BusinessRuleViolation as exc:
                return _fallback_result(
                    str(exc),
                    output_mode=OUTPUT_MODE_RULE_ONLY,
                    retry_count=retry_count,
                    fallback_reason="business_rule_validation_failed",
                    retry_reason=RETRY_REASON_BUSINESS_RULE_VALIDATION_FAILED,
                )
            except ToolCallParseError as exc:
                last_error = str(exc)
                retry_reason = RETRY_REASON_TOOL_CALL_PARSE_ERROR
                if retry_count >= max_validation_retries:
                    return _fallback_result(
                        last_error,
                        output_mode=mode,
                        retry_count=retry_count,
                        fallback_reason="schema_validation_failed",
                        retry_reason=retry_reason,
                    )
                retry_count += 1
            except JsonParseError as exc:
                last_error = str(exc)
                retry_reason = RETRY_REASON_JSON_PARSE_ERROR
                if retry_count >= max_validation_retries:
                    return _fallback_result(
                        last_error,
                        output_mode=mode,
                        retry_count=retry_count,
                        fallback_reason="schema_validation_failed",
                        retry_reason=retry_reason,
                    )
                retry_count += 1
            except ValidationError as exc:
                last_error = str(exc)
                retry_reason = (
                    RETRY_REASON_TOOL_CALL_PARSE_ERROR
                    if mode == OUTPUT_MODE_TOOL
                    else RETRY_REASON_SCHEMA_VALIDATION_ERROR
                )
                if retry_count >= max_validation_retries:
                    return _fallback_result(
                        last_error,
                        output_mode=mode,
                        retry_count=retry_count,
                        fallback_reason="schema_validation_failed",
                        retry_reason=retry_reason,
                    )
                retry_count += 1
            except ValueError as exc:
                last_error = str(exc)
                retry_reason = (
                    RETRY_REASON_TOOL_CALL_PARSE_ERROR
                    if mode == OUTPUT_MODE_TOOL
                    else RETRY_REASON_JSON_PARSE_ERROR
                )
                if retry_count >= max_validation_retries:
                    return _fallback_result(
                        last_error,
                        output_mode=mode,
                        retry_count=retry_count,
                        fallback_reason="schema_validation_failed",
                        retry_reason=retry_reason,
                    )
                retry_count += 1
    reason = "unsupported_output_modes:" + ",".join(unsupported) if unsupported else last_error
    return _fallback_result(reason or "review output unavailable")


def _supports_mode(model_runner: Any, mode: str) -> bool:
    checker = getattr(model_runner, "supports_review_output_mode", None)
    if callable(checker):
        return bool(checker(mode))
    return True


def _build_request(
    *,
    request_base: dict[str, Any],
    prompt: str,
    mode: str,
    output_model: type[BaseModel],
    schema_name: str,
    tool_name: str,
    tool_description: str,
    retry_count: int,
) -> ModelTurnRequest:
    content = prompt
    if retry_count:
        content += "\n\nPrevious output failed schema validation. Return valid JSON Schema-conformant data only."
    schema = _strict_schema(output_model)
    preferences = ModelPreferences(max_output_tokens=int(request_base.get("max_output_tokens") or 1200))
    tools: list[ModelToolSchema] = []
    tool_choice = ToolChoicePolicy(mode=ToolChoiceMode.NONE, max_tool_calls=0)
    if mode == OUTPUT_MODE_STRUCTURED:
        preferences.structured_output_schema = {
            "name": schema_name,
            "strict": True,
            "schema": schema,
        }
    elif mode == OUTPUT_MODE_TOOL:
        content = (
            f"Call exactly one {tool_name} tool with a JSON object argument that conforms to the JSON Schema. "
            "Do not answer in natural language or include extra tool calls.\n\n"
            f"{content}"
        )
        tools = [
            ModelToolSchema(
                name=tool_name,
                description=tool_description,
                parameters_schema=schema,
                metadata={"strict": True},
            )
        ]
        tool_choice = ToolChoicePolicy(
            mode=ToolChoiceMode.SPECIFIC_TOOL,
            tool_name=tool_name,
            allowed_tool_names=[tool_name],
            max_tool_calls=1,
        )
    elif mode == OUTPUT_MODE_JSON:
        preferences.json_mode = True

    return ModelTurnRequest(
        request_id=str(request_base["request_id"]),
        run_id=str(request_base["run_id"]),
        session_id=str(request_base["session_id"]),
        task_id=str(request_base["task_id"]),
        phase_id=str(request_base["phase_id"]),
        action_id=str(request_base["action_id"]),
        purpose=request_base["purpose"],
        messages=[
            ModelMessage(
                role=ModelRole.USER,
                content=[ContentBlock.from_text(content)],
            )
        ],
        tools=tools,
        tool_choice=tool_choice,
        model_preferences=preferences,
        budget=ModelBudget(max_retries=1, max_output_tokens=preferences.max_output_tokens),
        context_metadata=dict(request_base.get("context_metadata") or {}),
    )


def _payload_from_result(result: Any, *, mode: str, tool_name: str) -> dict[str, Any]:
    if mode == OUTPUT_MODE_TOOL:
        calls = [
            call
            for call in getattr(result, "tool_calls", []) or []
            if isinstance(call, ModelToolCall) and call.tool_name == tool_name
        ]
        if not calls:
            raise ToolCallParseError("forced tool calling response did not include the required tool")
        call = calls[0]
        if call.parse_status != ModelToolParseStatus.VALID:
            try:
                raw_payload = json.loads(call.raw_arguments or "{}")
            except json.JSONDecodeError as exc:
                raise ToolCallParseError("forced tool calling arguments failed JSON/schema parsing") from exc
            if not isinstance(raw_payload, dict):
                raise ToolCallParseError("forced tool calling arguments were not a JSON object")
            return raw_payload
        return dict(call.arguments)
    text = nested_getattr(result, "assistant_message.text", default="") or ""
    return _load_json_object(text)


def _load_json_object(text: str) -> dict[str, Any]:
    stripped = text.strip()
    if not stripped:
        raise JsonParseError("empty review model response")
    try:
        payload = json.loads(stripped)
    except json.JSONDecodeError:
        decoder = json.JSONDecoder()
        for index, char in enumerate(stripped):
            if char not in "{[":
                continue
            try:
                payload, _ = decoder.raw_decode(stripped[index:])
                break
            except json.JSONDecodeError:
                continue
        else:
            raise JsonParseError("review model response did not contain JSON") from None
    if isinstance(payload, list):
        return {"findings": payload}
    if isinstance(payload, dict) and isinstance(payload.get("report"), dict):
        report = payload["report"]
        if isinstance(report.get("findings"), list):
            return {"findings": report["findings"]}
    if not isinstance(payload, dict):
        raise JsonParseError("review model response was not a JSON object")
    return payload


def _strict_schema(output_model: type[BaseModel]) -> dict[str, Any]:
    schema = output_model.model_json_schema()
    _forbid_additional_properties(schema)
    return schema


def _forbid_additional_properties(schema: Any) -> None:
    if isinstance(schema, dict):
        if schema.get("type") == "object" or "properties" in schema:
            schema["additionalProperties"] = False
        for value in schema.values():
            _forbid_additional_properties(value)
    elif isinstance(schema, list):
        for item in schema:
            _forbid_additional_properties(item)


def _schema_hash(output_model: type[BaseModel]) -> str:
    text = json.dumps(_strict_schema(output_model), ensure_ascii=False, sort_keys=True, default=str)
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def _fallback_result(
    error: str,
    *,
    output_mode: str = OUTPUT_MODE_RULE_ONLY,
    retry_count: int = 0,
    fallback_reason: str = "rule_only_fallback",
    retry_reason: str = RETRY_REASON_NONE,
) -> ReviewOutputResult:
    return ReviewOutputResult(
        status="fallback",
        error=error,
        metadata={
            "output_mode": output_mode,
            "schema_validation_passed": False,
            "retry_count": retry_count,
            "retry_reason": retry_reason,
            "fallback_reason": fallback_reason,
        },
    )
