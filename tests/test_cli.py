from pathlib import Path

from typer.testing import CliRunner

from miniharness.cli import app
from miniharness.cli import create_or_resume_planner, workspace_health_summary
from miniharness.kernel import CancellationError
from miniharness.kernel.finalization import FinalReport
from miniharness.kernel.models import RunStatus
from miniharness.planner import PlannerRuntime, TaskStatus
from miniharness.workspace_state import WorkspaceHealthReport, WorkspaceHealthStatus


runner = CliRunner()


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


def test_cli_runs_through_kernel_bootstrap(monkeypatch, tmp_path: Path) -> None:
    calls: list[tuple[str, object]] = []

    class FakeWorkspaceState:
        baseline = None

        def get_workspace_health(self) -> WorkspaceHealthReport:
            return WorkspaceHealthReport(status=WorkspaceHealthStatus.CLEAN)

    class FakeTrace:
        def record(self, event: str, data: dict) -> None:
            calls.append((event, data))

        class Store:
            run_dir = tmp_path / "traces" / "run_1"

        store = Store()

    class FakeGraph:
        trace = FakeTrace()
        workspace_state = FakeWorkspaceState()

    class FakeResult:
        final_answer = "done"
        status = RunStatus.COMPLETED
        final_report = FinalReport(
            run_id="run_1",
            session_id="session_1",
            task_id="task_1",
            kernel_status="finalized",
            shutdown_reason="normal",
            diagnostics_count=0,
            cleanup_status="completed",
            recovered_previous_run=False,
            uncertain_transactions=[],
            workspace_lock_status="released",
            runtime_health_summary={"planner": "ok"},
            shutdown_summary={"cleanup_status": "completed"},
            recovery_summary={"recovered": False},
            lifecycle_summary={"events": 3},
        )

    class FakeKernel:
        graph = FakeGraph()
        recovery_report = None

        class Context:
            class Identity:
                run_id = "run_1"

            identity = Identity()

        context = Context()

        def run_task(self, goal: str) -> FakeResult:
            calls.append(("run_task", {"goal": goal}))
            return FakeResult()

    class FakeBootstrap:
        def __init__(self, **kwargs) -> None:
            calls.append(("bootstrap_init", kwargs))

        def boot(self, goal: str) -> FakeKernel:
            calls.append(("boot", {"goal": goal}))
            return FakeKernel()

    monkeypatch.chdir(tmp_path)
    monkeypatch.setattr("miniharness.cli.KernelBootstrap", FakeBootstrap)

    result = runner.invoke(app, ["main", "hello", "--dry-run"])

    assert result.exit_code == 0
    assert ("boot", {"goal": "hello"}) in calls
    assert ("run_task", {"goal": "hello"}) in calls
    assert "final report" in result.output


def test_cli_converts_kernel_cancellation_to_exit(monkeypatch, tmp_path: Path) -> None:
    final_report = FinalReport(
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        kernel_status="finalized",
        shutdown_reason="keyboard_interrupt",
        diagnostics_count=0,
        cleanup_status="completed",
        recovered_previous_run=False,
        uncertain_transactions=[],
        workspace_lock_status="released",
        shutdown_summary={"reason": "keyboard_interrupt", "cleanup_status": "completed"},
    )

    class FakeKernel:
        class FakeWorkspaceState:
            baseline = None

            def get_workspace_health(self) -> WorkspaceHealthReport:
                return WorkspaceHealthReport(status=WorkspaceHealthStatus.CLEAN)

        class FakeTrace:
            class Store:
                run_dir = tmp_path / "traces" / "run_1"

            store = Store()

            def record(self, event: str, data: dict) -> None:
                pass

        class FakeGraph:
            pass

        graph = FakeGraph()
        graph.trace = FakeTrace()
        graph.workspace_state = FakeWorkspaceState()

        class Context:
            class Identity:
                run_id = "run_1"

            identity = Identity()

        context = Context()
        recovery_report = None

        def run_task(self, goal: str):
            raise CancellationError("Ctrl+C", code="keyboard_interrupt")

        def final_report(self):
            return final_report

    class FakeBootstrap:
        def __init__(self, **kwargs) -> None:
            pass

        def boot(self, goal: str):
            return FakeKernel()

    monkeypatch.chdir(tmp_path)
    monkeypatch.setattr("miniharness.cli.KernelBootstrap", FakeBootstrap)

    result = runner.invoke(app, ["main", "hello", "--dry-run"])

    assert result.exit_code == 1
    assert "cancelled" in result.output
    assert "final report" in result.output
    assert "keyboard_interrupt" in result.output
