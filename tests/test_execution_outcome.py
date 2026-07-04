from __future__ import annotations

from singularity.execution_outcome import ExecutionOutcome, ExecutionOutcomeStatus


def test_execution_outcome_to_dict_serializes_status_and_retry_flag() -> None:
    outcome = ExecutionOutcome(
        status=ExecutionOutcomeStatus.REPLAN_REQUIRED,
        source="protocol",
        reason="Need a different command.",
        error_code="process_not_found",
        next_action="retry",
        retry_allowed=True,
    )

    assert outcome.to_dict() == {
        "status": "replan_required",
        "source": "protocol",
        "reason": "Need a different command.",
        "error_code": "process_not_found",
        "missing_evidence": [],
        "next_action": "retry",
        "observation_summary": "",
        "retry_allowed": True,
        "metadata": {},
    }
