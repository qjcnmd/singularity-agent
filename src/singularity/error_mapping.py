from __future__ import annotations

from singularity.error_codes import ErrorCode
from singularity.error_kinds import ToolCallFailureKind

TOOL_PROTOCOL_FAILURE_KIND_ERROR_CODE_MAP: dict[ToolCallFailureKind, ErrorCode] = {
    ToolCallFailureKind.missing_tool_call_id: ErrorCode.PROTOCOL_VIOLATION,
    ToolCallFailureKind.duplicate_tool_call_id: ErrorCode.PROTOCOL_VIOLATION,
    ToolCallFailureKind.unknown_tool: ErrorCode.UNKNOWN_TOOL,
    ToolCallFailureKind.disallowed_tool: ErrorCode.DISALLOWED_TOOL,
    ToolCallFailureKind.invalid_json: ErrorCode.INVALID_JSON,
    ToolCallFailureKind.arguments_not_object: ErrorCode.ARGUMENTS_NOT_OBJECT,
    ToolCallFailureKind.schema_mismatch: ErrorCode.SCHEMA_MISMATCH,
    ToolCallFailureKind.protocol_violation: ErrorCode.PROTOCOL_VIOLATION,
    ToolCallFailureKind.policy_denied: ErrorCode.POLICY_DENIED,
    ToolCallFailureKind.approval_required: ErrorCode.APPROVAL_REQUIRED,
    ToolCallFailureKind.approval_denied: ErrorCode.APPROVAL_DENIED,
    ToolCallFailureKind.sandbox_required: ErrorCode.SANDBOX_REQUIRED,
    ToolCallFailureKind.tool_executor_failed: ErrorCode.TOOL_FAILURE,
    ToolCallFailureKind.result_binding_failed: ErrorCode.PROTOCOL_VIOLATION,
    ToolCallFailureKind.replay_detected: ErrorCode.PROTOCOL_VIOLATION,
    ToolCallFailureKind.conflicting_replay: ErrorCode.PROTOCOL_VIOLATION,
    ToolCallFailureKind.context_append_failed: ErrorCode.PROTOCOL_VIOLATION,
}

TOOL_PROTOCOL_VALIDATION_ERROR_KIND_PRIORITY: tuple[ToolCallFailureKind, ...] = (
    ToolCallFailureKind.conflicting_replay,
    ToolCallFailureKind.unknown_tool,
    ToolCallFailureKind.missing_tool_call_id,
    ToolCallFailureKind.duplicate_tool_call_id,
    ToolCallFailureKind.invalid_json,
    ToolCallFailureKind.arguments_not_object,
    ToolCallFailureKind.schema_mismatch,
    ToolCallFailureKind.approval_required,
    ToolCallFailureKind.approval_denied,
    ToolCallFailureKind.sandbox_required,
    ToolCallFailureKind.disallowed_tool,
    ToolCallFailureKind.protocol_violation,
)

PROTOCOL_VALIDATION_ERROR_TOKENS: frozenset[str] = frozenset(
    {kind.value for kind in TOOL_PROTOCOL_VALIDATION_ERROR_KIND_PRIORITY}
    | {
        "max_tool_calls_exceeded",
        "tool_calls_must_be_list",
        "tool_call_must_be_object",
    }
)


def tool_protocol_failure_error_code(kind: ToolCallFailureKind | str) -> ErrorCode:
    resolved = kind if isinstance(kind, ToolCallFailureKind) else ToolCallFailureKind(str(kind))
    return TOOL_PROTOCOL_FAILURE_KIND_ERROR_CODE_MAP[resolved]


def tool_protocol_validation_error_kind(errors: list[str]) -> ToolCallFailureKind:
    for kind in TOOL_PROTOCOL_VALIDATION_ERROR_KIND_PRIORITY:
        if kind.value in errors:
            return kind
    return ToolCallFailureKind.protocol_violation


def tool_protocol_validation_error_code(errors: list[str]) -> str | None:
    if not errors:
        return None
    return tool_protocol_failure_error_code(
        tool_protocol_validation_error_kind(errors)
    ).value
