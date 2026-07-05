from __future__ import annotations

from singularity.agent_loop_completion import CompletionGate
from singularity.execution_outcome import ExecutionOutcome, ExecutionOutcomeStatus


def test_completion_gate_repair_phase_blocked_outcome_requires_assessment_reason(tmp_path) -> None:
    class _State:
        current_phase = "repairing_failures"

    class _Planner:
        workspace_root = tmp_path
        state = _State()

    outcome = CompletionGate.repair_phase_completion_blocked_outcome(
        _Planner(),
        assessment={
            "unmet": ["verification_contract_satisfaction"],
        },
    )

    assert isinstance(outcome, ExecutionOutcome)
    assert outcome.status == ExecutionOutcomeStatus.BLOCKED
    assert "repair contract is unsatisfied" in outcome.reason
