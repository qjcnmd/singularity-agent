from __future__ import annotations

from dataclasses import dataclass

from singularity.error_codes import (
    TOOL_BLOCKING_ERROR_CODES,
    TOOL_REPLAN_ERROR_CODES,
    TOOL_RETRYABLE_ERROR_CODES,
    ErrorCode,
)
from singularity.execution_outcome import ExecutionOutcomeStatus

EXECUTION_OUTCOME_TERMINAL_MAP: dict[ExecutionOutcomeStatus, bool] = {
    ExecutionOutcomeStatus.SUCCESS: False,
    ExecutionOutcomeStatus.RETRYABLE: False,
    ExecutionOutcomeStatus.REPLAN_REQUIRED: False,
    ExecutionOutcomeStatus.APPROVAL_REQUIRED: False,
    ExecutionOutcomeStatus.USER_INPUT_REQUIRED: False,
    ExecutionOutcomeStatus.BLOCKED: True,
    ExecutionOutcomeStatus.FATAL: True,
}

PROTOCOL_NEXT_ACTION_LIFECYCLE_MAP: dict[str, str] = {
    "pending_approval": "waiting_approval",
    "resume_pending_approval": "waiting_approval",
    "ask_user": "waiting_user",
    "request_user_input": "waiting_user",
    "await_tool_result": "running",
    "execute_pending_tool": "running",
    "append_tool_message": "running",
    "request_model": "running",
    "continue": "running",
    "finalize": "reporting",
}

PROTOCOL_TERMINAL_RETRY_STATUSES: frozenset[str] = frozenset(
    {"failed", "invalid_assistant"}
)


@dataclass(frozen=True)
class ProtocolOutcomeMapping:
    status: ExecutionOutcomeStatus
    source: str
    error_code: str
    next_action: str
    retry_allowed: bool


def execution_outcome_is_terminal(status: ExecutionOutcomeStatus | str) -> bool:
    resolved = (
        status
        if isinstance(status, ExecutionOutcomeStatus)
        else ExecutionOutcomeStatus(str(status))
    )
    return EXECUTION_OUTCOME_TERMINAL_MAP[resolved]


def lifecycle_status_for_protocol_next_action(
    next_action: str,
    *,
    pending_approval_count: int,
    current_status: str,
) -> str:
    if pending_approval_count:
        return "waiting_approval"
    return PROTOCOL_NEXT_ACTION_LIFECYCLE_MAP.get(next_action, current_status)


def protocol_error_code_to_outcome(error_code: str) -> ProtocolOutcomeMapping | None:
    if error_code == ErrorCode.APPROVAL_REQUIRED.value:
        return ProtocolOutcomeMapping(
            status=ExecutionOutcomeStatus.APPROVAL_REQUIRED,
            source="protocol",
            error_code=ErrorCode.APPROVAL_REQUIRED.value,
            next_action="wait_for_approval",
            retry_allowed=False,
        )
    if error_code == ErrorCode.POLICY_ASK_USER_REQUIRED.value:
        return ProtocolOutcomeMapping(
            status=ExecutionOutcomeStatus.USER_INPUT_REQUIRED,
            source="tool",
            error_code=ErrorCode.POLICY_ASK_USER_REQUIRED.value,
            next_action="ask_user",
            retry_allowed=False,
        )
    if error_code in TOOL_BLOCKING_ERROR_CODES:
        return ProtocolOutcomeMapping(
            status=ExecutionOutcomeStatus.BLOCKED,
            source="tool",
            error_code=error_code,
            next_action="blocked",
            retry_allowed=False,
        )
    if error_code in TOOL_REPLAN_ERROR_CODES:
        return ProtocolOutcomeMapping(
            status=ExecutionOutcomeStatus.REPLAN_REQUIRED,
            source="tool",
            error_code=error_code,
            next_action="replan",
            retry_allowed=True,
        )
    if error_code in TOOL_RETRYABLE_ERROR_CODES:
        return ProtocolOutcomeMapping(
            status=ExecutionOutcomeStatus.RETRYABLE,
            source="protocol"
            if "json" in error_code or "schema" in error_code
            else "tool",
            error_code=error_code,
            next_action="retry",
            retry_allowed=True,
        )
    return None
