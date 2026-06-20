import json
from pathlib import Path

from typer.testing import CliRunner

from miniharness.cli import app
from miniharness.cli import create_or_resume_planner, workspace_health_summary
from miniharness.evaluation import (
    BenchmarkTask,
    ExpectedOutcome,
    ExpectedOutcomeKind,
    GoldenTaskStore,
    WorkspaceSnapshot,
    WorkspaceSnapshotKind,
)
from miniharness.observability import TraceEventType, TraceRuntime
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


def test_cli_eval_task_validate_and_list_filter_tags(tmp_path: Path) -> None:
    task_set = tmp_path / "golden.json"
    task = BenchmarkTask(
        task_id="task.cli",
        version="v1",
        title="CLI task",
        input_prompt="Run the evaluation CLI.",
        workspace_snapshot=WorkspaceSnapshot(
            kind=WorkspaceSnapshotKind.GIT_REF,
            git_ref="HEAD",
        ),
        expected_outcomes=[
            ExpectedOutcome(kind=ExpectedOutcomeKind.HEURISTIC, weight=1.0)
        ],
        tags=["easy", "tool-heavy"],
    )
    task_set.write_text(GoldenTaskStore.to_json_document([task]), encoding="utf-8")

    validate = runner.invoke(app, ["eval", "task", "validate", str(task_set), "--json"])
    listed = runner.invoke(
        app,
        ["benchmark", "task", "list", str(task_set), "--tag", "tool-heavy", "--json"],
    )

    assert validate.exit_code == 0
    assert '"task_count": 1' in validate.output
    assert listed.exit_code == 0
    assert "task.cli" in listed.output


def _cli_eval_task(task_id: str = "task.cli.suite") -> BenchmarkTask:
    return BenchmarkTask(
        task_id=task_id,
        version="v1",
        title="CLI evaluation task",
        input_prompt="Run the evaluation CLI.",
        workspace_snapshot=WorkspaceSnapshot(
            kind=WorkspaceSnapshotKind.GIT_REF,
            git_ref="HEAD",
        ),
        expected_outcomes=[
            ExpectedOutcome(
                kind=ExpectedOutcomeKind.HEURISTIC,
                weight=1.0,
                heuristic="custom",
                metadata={"score": 0.7},
            )
        ],
        tags=["easy", "tool-heavy"],
    )


def _write_cli_task_set(path: Path) -> None:
    path.write_text(GoldenTaskStore.to_json_document([_cli_eval_task()]), encoding="utf-8")


def _write_cli_trace(root: Path) -> Path:
    trace = TraceRuntime.create(root, run_id="run_cli", session_id="session_cli")
    trace.emit(
        TraceEventType.MODEL_RESPONSE_RECEIVED,
        runtime="model",
        summary="Model response received.",
        payload={"usage": {"input_tokens": 10, "output_tokens": 5, "cost_estimate": 0.01}},
    )
    trace.emit(
        TraceEventType.VERIFICATION_CHECK_COMPLETED,
        runtime="verification",
        summary="check passed",
        payload={"status": "passed"},
    )
    return trace.store.run_dir


def test_cli_eval_suite_run_writes_report_without_miniharness_state(tmp_path: Path, monkeypatch) -> None:
    monkeypatch.chdir(tmp_path)
    task_set = tmp_path / "golden.json"
    output_dir = tmp_path / "evals"
    _write_cli_task_set(task_set)

    result = runner.invoke(
        app,
        [
            "eval",
            "suite",
            "run",
            str(task_set),
            "--output-dir",
            str(output_dir),
            "--run-id",
            "suite_cli",
            "--json",
        ],
    )

    assert result.exit_code == 0
    assert (output_dir / "suite_cli" / "report.json").exists()
    assert (output_dir / "suite_cli" / "report.md").exists()
    assert not (tmp_path / ".miniharness").exists()
    payload = json.loads((output_dir / "suite_cli" / "report.json").read_text(encoding="utf-8"))
    assert payload["run_id"] == "suite_cli"
    assert payload["profile_reports"][0]["task_results"][0]["runtime_overrides"]["model"] == "default"


def test_cli_eval_trace_replay_outputs_deterministic_hash(tmp_path: Path, monkeypatch) -> None:
    monkeypatch.chdir(tmp_path)
    trace_run_dir = _write_cli_trace(tmp_path)

    result = runner.invoke(app, ["eval", "trace", "replay", str(trace_run_dir), "--json"])

    assert result.exit_code == 0
    payload = json.loads(result.output)
    assert payload["deterministic"] is True
    assert payload["trace_input_digest"]
    assert payload["result_hash"]


def test_cli_eval_ab_and_regression_run_persist_reports(tmp_path: Path, monkeypatch) -> None:
    monkeypatch.chdir(tmp_path)
    task_set = tmp_path / "golden.json"
    output_dir = tmp_path / "evals"
    trace_run_dir = _write_cli_trace(tmp_path)
    _write_cli_task_set(task_set)
    baseline = json.dumps(
        {
            "name": "baseline",
            "model": "gpt-a",
            "prompt_profile": "default",
            "memory_enabled": True,
            "allowed_tools": [],
            "tool_policy": "read_write",
        }
    )
    candidate = json.dumps(
        {
            "name": "candidate",
            "model": "gpt-b",
            "prompt_profile": "compact",
            "memory_enabled": False,
            "allowed_tools": ["read_file"],
            "tool_policy": "read_only",
        }
    )

    ab_result = runner.invoke(
        app,
        [
            "eval",
            "ab",
            "run",
            str(task_set),
            "--baseline-profile-json",
            baseline,
            "--candidate-profile-json",
            candidate,
            "--trace-run-dir",
            str(trace_run_dir),
            "--output-dir",
            str(output_dir),
            "--run-id",
            "ab_cli",
            "--json",
        ],
    )
    regression_result = runner.invoke(
        app,
        [
            "eval",
            "regression",
            "run",
            str(task_set),
            "--baseline-profile-json",
            baseline,
            "--candidate-profile-json",
            candidate,
            "--trace-run-dir",
            str(trace_run_dir),
            "--output-dir",
            str(output_dir),
            "--run-id",
            "reg_cli",
            "--json",
        ],
    )

    assert ab_result.exit_code == 0
    assert regression_result.exit_code == 0
    assert (output_dir / "ab_cli" / "report.json").exists()
    assert (output_dir / "reg_cli" / "report.json").exists()
    assert (output_dir / "reg_cli" / "regression.json").exists()
    regression_payload = json.loads((output_dir / "reg_cli" / "regression.json").read_text(encoding="utf-8"))
    assert regression_payload["baseline_profile"] == "baseline"
    assert regression_payload["candidate_profile"] == "candidate"


def test_cli_eval_report_show_reads_json_and_markdown(tmp_path: Path, monkeypatch) -> None:
    monkeypatch.chdir(tmp_path)
    task_set = tmp_path / "golden.json"
    output_dir = tmp_path / "evals"
    _write_cli_task_set(task_set)
    run = runner.invoke(
        app,
        [
            "eval",
            "suite",
            "run",
            str(task_set),
            "--output-dir",
            str(output_dir),
            "--run-id",
            "show_cli",
        ],
    )
    assert run.exit_code == 0

    shown_json = runner.invoke(
        app,
        ["eval", "report", "show", str(output_dir / "show_cli" / "report.json"), "--json"],
    )
    shown_md = runner.invoke(
        app,
        ["eval", "report", "show", str(output_dir / "show_cli" / "report.md")],
    )

    assert shown_json.exit_code == 0
    assert '"run_id": "show_cli"' in shown_json.output
    assert shown_md.exit_code == 0
    assert "# Evaluation Report `show_cli`" in shown_md.output
