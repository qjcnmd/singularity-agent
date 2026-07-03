from __future__ import annotations

from singularity.error_codes import ERROR_CODE_VALUES, ErrorCode
from singularity.error_mapping import (
    PROTOCOL_VALIDATION_ERROR_TOKENS,
    TOOL_PROTOCOL_FAILURE_KIND_ERROR_CODE_MAP,
    TOOL_PROTOCOL_VALIDATION_ERROR_KIND_PRIORITY,
    tool_protocol_failure_error_code,
    tool_protocol_validation_error_code,
    tool_protocol_validation_error_kind,
)
from singularity.tool_protocol.engine import (
    _error_code_from_validation,
    _error_kind_from_validation,
)
from singularity.tool_protocol.models import ToolCallFailureKind


def test_tool_protocol_failure_kind_mapping_is_exhaustive_and_canonical() -> None:
    assert set(TOOL_PROTOCOL_FAILURE_KIND_ERROR_CODE_MAP) == set(ToolCallFailureKind)
    assert {code.value for code in TOOL_PROTOCOL_FAILURE_KIND_ERROR_CODE_MAP.values()} <= ERROR_CODE_VALUES

    assert tool_protocol_failure_error_code(ToolCallFailureKind.missing_tool_call_id) == ErrorCode.PROTOCOL_VIOLATION
    assert tool_protocol_failure_error_code(ToolCallFailureKind.duplicate_tool_call_id) == ErrorCode.PROTOCOL_VIOLATION
    assert tool_protocol_failure_error_code(ToolCallFailureKind.unknown_tool) == ErrorCode.UNKNOWN_TOOL
    assert tool_protocol_failure_error_code(ToolCallFailureKind.disallowed_tool) == ErrorCode.DISALLOWED_TOOL


def test_tool_protocol_validation_error_priority_is_explicit() -> None:
    assert len(TOOL_PROTOCOL_VALIDATION_ERROR_KIND_PRIORITY) == len(set(TOOL_PROTOCOL_VALIDATION_ERROR_KIND_PRIORITY))
    assert set(TOOL_PROTOCOL_VALIDATION_ERROR_KIND_PRIORITY) <= set(ToolCallFailureKind)

    errors = ["schema_mismatch", "unknown_tool", "invalid_json"]

    assert tool_protocol_validation_error_kind(errors) == ToolCallFailureKind.unknown_tool
    assert tool_protocol_validation_error_code(errors) == ErrorCode.UNKNOWN_TOOL.value
    assert _error_kind_from_validation(errors) == ToolCallFailureKind.unknown_tool
    assert _error_code_from_validation(errors) == ErrorCode.UNKNOWN_TOOL.value


def test_tool_protocol_validation_tokens_are_mapped_or_internal_only() -> None:
    expected_internal_only = {
        "max_tool_calls_exceeded",
        "tool_calls_must_be_list",
        "tool_call_must_be_object",
    }

    assert {kind.value for kind in TOOL_PROTOCOL_VALIDATION_ERROR_KIND_PRIORITY} | expected_internal_only == set(
        PROTOCOL_VALIDATION_ERROR_TOKENS
    )
    assert tool_protocol_validation_error_kind(["max_tool_calls_exceeded"]) == ToolCallFailureKind.protocol_violation
    assert tool_protocol_validation_error_code(["max_tool_calls_exceeded"]) == ErrorCode.PROTOCOL_VIOLATION.value
    assert tool_protocol_validation_error_code([]) is None
