from __future__ import annotations

import pytest

from singularity.error_codes import (
    TOOL_BLOCKING_ERROR_CODES,
    TOOL_REPLAN_ERROR_CODES,
    TOOL_RETRYABLE_ERROR_CODES,
    ErrorCode,
)
from singularity.execution_outcome import ExecutionOutcomeStatus
from singularity.status_mapping import (
    EXECUTION_OUTCOME_TERMINAL_MAP,
    PROTOCOL_NEXT_ACTION_LIFECYCLE_MAP,
    PROTOCOL_TERMINAL_RETRY_STATUSES,
    execution_outcome_is_terminal,
    lifecycle_status_for_protocol_next_action,
    protocol_error_code_to_outcome,
)


def test_execution_outcome_terminal_mapping_is_exhaustive() -> None:
    assert set(EXECUTION_OUTCOME_TERMINAL_MAP) == set(ExecutionOutcomeStatus)

    assert execution_outcome_is_terminal(ExecutionOutcomeStatus.SUCCESS) is False
    assert execution_outcome_is_terminal(ExecutionOutcomeStatus.RETRYABLE) is False
    assert execution_outcome_is_terminal(ExecutionOutcomeStatus.REPLAN_REQUIRED) is False
    assert execution_outcome_is_terminal(ExecutionOutcomeStatus.APPROVAL_REQUIRED) is False
    assert execution_outcome_is_terminal(ExecutionOutcomeStatus.USER_INPUT_REQUIRED) is False
    assert execution_outcome_is_terminal(ExecutionOutcomeStatus.BLOCKED) is True
    assert execution_outcome_is_terminal(ExecutionOutcomeStatus.FATAL) is True


@pytest.mark.parametrize(
    ("next_action", "expected"),
    [
        ("pending_approval", "waiting_approval"),
        ("resume_pending_approval", "waiting_approval"),
        ("ask_user", "waiting_user"),
        ("request_user_input", "waiting_user"),
        ("await_tool_result", "running"),
        ("execute_pending_tool", "running"),
        ("append_tool_message", "running"),
        ("request_model", "running"),
        ("continue", "running"),
        ("finalize", "reporting"),
    ],
)
def test_protocol_next_action_lifecycle_mapping(next_action: str, expected: str) -> None:
    assert PROTOCOL_NEXT_ACTION_LIFECYCLE_MAP[next_action] == expected
    assert (
        lifecycle_status_for_protocol_next_action(
            next_action,
            pending_approval_count=0,
            current_status="running",
        )
        == expected
    )


def test_pending_approval_count_overrides_protocol_next_action() -> None:
    assert (
        lifecycle_status_for_protocol_next_action(
            "continue",
            pending_approval_count=1,
            current_status="running",
        )
        == "waiting_approval"
    )


def test_unknown_protocol_next_action_preserves_current_lifecycle_status() -> None:
    assert (
        lifecycle_status_for_protocol_next_action(
            "new_future_action",
            pending_approval_count=0,
            current_status="repairing",
        )
        == "repairing"
    )


@pytest.mark.parametrize("error_code", sorted(TOOL_BLOCKING_ERROR_CODES))
def test_blocking_error_codes_map_to_blocked(error_code: str) -> None:
    mapping = protocol_error_code_to_outcome(error_code)

    assert mapping is not None
    assert mapping.status == ExecutionOutcomeStatus.BLOCKED
    assert mapping.next_action == "blocked"
    assert mapping.retry_allowed is False


@pytest.mark.parametrize("error_code", sorted(TOOL_REPLAN_ERROR_CODES))
def test_replan_error_codes_map_to_replan(error_code: str) -> None:
    mapping = protocol_error_code_to_outcome(error_code)

    assert mapping is not None
    assert mapping.status == ExecutionOutcomeStatus.REPLAN_REQUIRED
    assert mapping.next_action == "replan"
    assert mapping.retry_allowed is True


@pytest.mark.parametrize("error_code", sorted(TOOL_RETRYABLE_ERROR_CODES))
def test_retryable_error_codes_map_to_retryable(error_code: str) -> None:
    mapping = protocol_error_code_to_outcome(error_code)

    assert mapping is not None
    assert mapping.status == ExecutionOutcomeStatus.RETRYABLE
    assert mapping.next_action == "retry"
    assert mapping.retry_allowed is True


def test_waiting_error_codes_have_explicit_outcome_mapping() -> None:
    approval = protocol_error_code_to_outcome(ErrorCode.APPROVAL_REQUIRED.value)
    ask_user = protocol_error_code_to_outcome(ErrorCode.POLICY_ASK_USER_REQUIRED.value)

    assert approval is not None
    assert approval.status == ExecutionOutcomeStatus.APPROVAL_REQUIRED
    assert approval.next_action == "wait_for_approval"
    assert ask_user is not None
    assert ask_user.status == ExecutionOutcomeStatus.USER_INPUT_REQUIRED
    assert ask_user.next_action == "ask_user"


def test_protocol_terminal_retry_statuses_are_explicit() -> None:
    assert {"failed", "invalid_assistant"} == PROTOCOL_TERMINAL_RETRY_STATUSES
    assert protocol_error_code_to_outcome("not_a_registered_error_code") is None
