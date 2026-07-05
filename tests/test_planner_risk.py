from __future__ import annotations

from singularity.planner.models import RiskDecisionKind
from singularity.planner.risk import LARGE_CHANGED_FILE_SCOPE_THRESHOLD, RiskEscalator


def test_large_changed_file_scope_threshold_is_not_triggered_at_limit() -> None:
    changed_files = [f"src/file_{index}.py" for index in range(LARGE_CHANGED_FILE_SCOPE_THRESHOLD)]

    result = RiskEscalator().evaluate_action(
        tool_name="workspace_edit_file",
        arguments={},
        changed_files=changed_files,
    )

    assert result.decision == RiskDecisionKind.CONTINUE
    assert "large changed-file scope" not in result.reasons


def test_large_changed_file_scope_threshold_requires_review_above_limit() -> None:
    changed_files = [f"src/file_{index}.py" for index in range(LARGE_CHANGED_FILE_SCOPE_THRESHOLD + 1)]

    result = RiskEscalator().evaluate_action(
        tool_name="workspace_edit_file",
        arguments={},
        changed_files=changed_files,
    )

    assert result.decision == RiskDecisionKind.REQUIRE_REVIEW
    assert "large changed-file scope" in result.reasons
