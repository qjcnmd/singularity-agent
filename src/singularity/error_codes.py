from __future__ import annotations

from enum import StrEnum


class ErrorCode(StrEnum):
    APPROVAL_REQUIRED = "approval_required"
    APPROVAL_DENIED = "approval_denied"
    PERMISSION_DENIED = "permission_denied"
    POLICY_BLOCKED = "policy_blocked"
    POLICY_DENIED = "policy_denied"
    POLICY_ASK_USER_REQUIRED = "policy_ask_user_required"
    POLICY_ESCALATION_REQUIRED = "policy_escalation_required"
    PROTECTED_PATH_DENIED = "protected_path_denied"
    REVIEW_REQUIRED = "review_required"
    ACTION_NOT_ALLOWED = "action_not_allowed"
    RISK_ESCALATED = "risk_escalated"
    SANDBOX_REQUIRED = "sandbox_required"
    SANDBOX_CAPABILITY_FAILED = "sandbox_capability_failed"
    SANDBOX_UNAVAILABLE = "sandbox_unavailable"
    SANDBOX_VIOLATION = "sandbox_violation"
    CWD_DENIED = "cwd_denied"
    COMMAND_NOT_FOUND = "command_not_found"
    SPAWN_FAILED = "spawn_failed"
    PERMISSION_ERROR = "permission_error"
    TIMEOUT = "timeout"
    IDLE_TIMEOUT = "idle_timeout"
    EXIT_NONZERO = "exit_nonzero"
    OUTPUT_LIMIT_EXCEEDED = "output_limit_exceeded"
    SEMANTIC_FAILURE = "semantic_failure"
    VERIFICATION_FAILED = "verification_failed"
    BLOCKED_BY_VERIFICATION = "blocked_by_verification"
    VERIFICATION_RUNNER_REQUIRED = "verification_runner_required"
    SNAPSHOT_MISMATCH = "snapshot_mismatch"
    EXTERNAL_CHANGE_DETECTED = "external_change_detected"
    FILE_CHANGED = "file_changed"
    ROLLBACK_CONFLICT = "rollback_conflict"
    BAD_ARGUMENTS_JSON = "bad_arguments_json"
    INVALID_JSON = "invalid_json"
    ARGUMENTS_NOT_OBJECT = "arguments_not_object"
    VALIDATION_ERROR = "validation_error"
    SCHEMA_MISMATCH = "schema_mismatch"
    UNKNOWN_TOOL = "unknown_tool"
    TOOL_NOT_FOUND = "tool_not_found"
    DISALLOWED_TOOL = "disallowed_tool"
    PROTOCOL_VIOLATION = "protocol_violation"
    INTERNAL_ERROR = "internal_error"
    PROTOCOL_FAIL_SAFE = "protocol_fail_safe"
    TOOL_FAILURE = "tool_failure"
    PROCESS_NOT_FOUND = "process_not_found"
    COMPLETION_REJECTED = "completion_rejected"
    FINAL_REVIEW_REJECTED = "final_review_rejected"
    MAX_TURNS_EXCEEDED = "max_turns_exceeded"
    MODEL_RUNNER_FAILED = "model_runner_failed"
    REPAIR_BUDGET_EXCEEDED = "repair_budget_exceeded"


ERROR_CODE_VALUES = frozenset(code.value for code in ErrorCode)

FAILURE_ANALYSIS_EXCLUDED_ERROR_CODES = frozenset(
    {
        ErrorCode.APPROVAL_REQUIRED.value,
        ErrorCode.APPROVAL_DENIED.value,
        ErrorCode.PERMISSION_DENIED.value,
        ErrorCode.POLICY_BLOCKED.value,
        ErrorCode.POLICY_DENIED.value,
        ErrorCode.POLICY_ASK_USER_REQUIRED.value,
        ErrorCode.ACTION_NOT_ALLOWED.value,
        ErrorCode.PROTECTED_PATH_DENIED.value,
        ErrorCode.RISK_ESCALATED.value,
        ErrorCode.SANDBOX_REQUIRED.value,
        ErrorCode.SANDBOX_CAPABILITY_FAILED.value,
        ErrorCode.SANDBOX_VIOLATION.value,
        ErrorCode.POLICY_ESCALATION_REQUIRED.value,
    }
)

TOOL_BLOCKING_ERROR_CODES = frozenset(
    {
        ErrorCode.POLICY_BLOCKED.value,
        ErrorCode.POLICY_DENIED.value,
        ErrorCode.PROTECTED_PATH_DENIED.value,
        ErrorCode.REVIEW_REQUIRED.value,
        ErrorCode.APPROVAL_DENIED.value,
        ErrorCode.ACTION_NOT_ALLOWED.value,
        ErrorCode.RISK_ESCALATED.value,
        ErrorCode.SANDBOX_REQUIRED.value,
        ErrorCode.SANDBOX_UNAVAILABLE.value,
        ErrorCode.SANDBOX_VIOLATION.value,
        ErrorCode.CWD_DENIED.value,
        ErrorCode.POLICY_ESCALATION_REQUIRED.value,
    }
)

TOOL_REPLAN_ERROR_CODES = frozenset(
    {
        ErrorCode.SNAPSHOT_MISMATCH.value,
        ErrorCode.EXTERNAL_CHANGE_DETECTED.value,
        ErrorCode.FILE_CHANGED.value,
        ErrorCode.ROLLBACK_CONFLICT.value,
        ErrorCode.SEMANTIC_FAILURE.value,
        ErrorCode.VERIFICATION_FAILED.value,
        ErrorCode.BLOCKED_BY_VERIFICATION.value,
        ErrorCode.COMMAND_NOT_FOUND.value,
        ErrorCode.PROCESS_NOT_FOUND.value,
        ErrorCode.TIMEOUT.value,
    }
)

TOOL_RETRYABLE_ERROR_CODES = frozenset(
    {
        ErrorCode.BAD_ARGUMENTS_JSON.value,
        ErrorCode.INVALID_JSON.value,
        ErrorCode.ARGUMENTS_NOT_OBJECT.value,
        ErrorCode.VALIDATION_ERROR.value,
        ErrorCode.SCHEMA_MISMATCH.value,
        ErrorCode.UNKNOWN_TOOL.value,
        ErrorCode.TOOL_NOT_FOUND.value,
        ErrorCode.DISALLOWED_TOOL.value,
        ErrorCode.PROTOCOL_VIOLATION.value,
        ErrorCode.INTERNAL_ERROR.value,
    }
)
