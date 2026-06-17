from miniharness.cli import workspace_health_summary
from miniharness.workspace_state import WorkspaceHealthReport, WorkspaceHealthStatus


def test_workspace_health_summary_lists_state_categories() -> None:
    health = WorkspaceHealthReport(
        status=WorkspaceHealthStatus.CONFLICTED,
        agent_changes=["app.py"],
        command_side_effects=["generated.txt"],
        external_changes=["README.md"],
        rollback_available=True,
        rollback_conflicts=["app.py"],
        recommended_next_action="re-read changed files before continuing",
    )

    summary = workspace_health_summary(health)

    assert "status: conflicted" in summary
    assert "agent_changes: app.py" in summary
    assert "command_side_effects: generated.txt" in summary
    assert "external_changes: README.md" in summary
    assert "rollback_available: true" in summary
    assert "rollback_conflicts: app.py" in summary
    assert "recommended_next_action: re-read changed files before continuing" in summary
