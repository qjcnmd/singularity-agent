from pathlib import Path

from miniharness.cli import create_or_resume_planner, workspace_health_summary
from miniharness.planner import PlannerRuntime, TaskStatus
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


def test_create_or_resume_planner_marks_conflicted_workspace_needs_review(tmp_path: Path) -> None:
    planner = PlannerRuntime(tmp_path, session_id="session_1", task_id="task_1")
    planner.start_task("Resume task")
    planner.interrupt("pause")
    health = WorkspaceHealthReport(
        status=WorkspaceHealthStatus.CONFLICTED,
        external_changes=["README.md"],
    )

    resumed = create_or_resume_planner(
        workspace_root=tmp_path,
        session_id="session_1",
        task_id="task_2",
        user_goal="Resume task",
        trace=None,
        workspace_health=health,
    )

    assert resumed.state.status == TaskStatus.NEEDS_REVIEW
    assert resumed.evidence.external_changes == ["README.md"]


def test_create_or_resume_planner_starts_new_task_without_resume(tmp_path: Path) -> None:
    planner = create_or_resume_planner(
        workspace_root=tmp_path,
        session_id=None,
        task_id="task_1",
        user_goal="New task",
        trace=None,
        workspace_health=WorkspaceHealthReport(status=WorkspaceHealthStatus.CLEAN),
    )

    assert planner.state.task_id == "task_1"
    assert planner.state.status == TaskStatus.UNDERSTANDING_TASK
