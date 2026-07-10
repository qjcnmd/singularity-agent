from __future__ import annotations

import json
from pathlib import Path

from typer.testing import CliRunner

from singularity.kernel.models import RunStatus
from singularity.oracle.cli import _session_status_from_run_status, app
from singularity.session import (
    RecoveryGateDecision,
    RecoveryGateStatus,
    SessionResumeContext,
    SessionRunMode,
    SessionStatus,
    SessionStore,
)

runner = CliRunner()


def test_session_cli_lists_and_shows_sessions(tmp_path: Path, monkeypatch) -> None:
    store = SessionStore(tmp_path)
    store.create_session(
        session_id="session_cli",
        project_root=tmp_path,
        user_goal="Fix CLI",
        task_id="task_cli",
    )
    store.start_run(
        session_id="session_cli",
        run_id="run_cli",
        task_id="task_cli",
        mode=SessionRunMode.NEW,
        user_goal="Fix CLI",
        trace_run_dir=tmp_path / "work" / "traces" / "runs" / "run_cli",
    )
    store.finish_run(run_id="run_cli", status=SessionStatus.INTERRUPTED)
    store.append_timeline_event(
        session_id="session_cli",
        run_id="run_cli",
        task_id="task_cli",
        event_type="session.recovery_blocked",
        summary="Recovery blocked for review.",
    )
    monkeypatch.chdir(tmp_path)

    listed = runner.invoke(app, ["session", "list", "--json"])
    shown = runner.invoke(app, ["session", "show", "session_cli", "--timeline", "--json"])

    assert listed.exit_code == 0, listed.output
    list_payload = json.loads(listed.output)
    assert list_payload[0]["session_id"] == "session_cli"
    assert list_payload[0]["resume_command"] == "sg resume session_cli"
    assert shown.exit_code == 0, shown.output
    show_payload = json.loads(shown.output)
    assert show_payload["session"]["session_id"] == "session_cli"
    assert show_payload["history_summary"]["planner"]["status"] == "missing"
    assert show_payload["history_summary"]["failures"]["last_run_id"] == "run_cli"
    assert show_payload["timeline"][0]["event_type"] == "session.recovery_blocked"


def test_continue_and_resume_cli_use_session_launch_path(tmp_path: Path, monkeypatch) -> None:
    calls: list[tuple[str, object]] = []
    store = SessionStore(tmp_path)
    store.create_session(
        session_id="session_continue",
        project_root=tmp_path,
        user_goal="Initial task",
        task_id="task_continue",
    )
    store.start_run(
        session_id="session_continue",
        run_id="run_previous",
        task_id="task_continue",
        mode=SessionRunMode.NEW,
        user_goal="Initial task",
        trace_run_dir=tmp_path / "work" / "traces" / "runs" / "run_previous",
    )
    store.finish_run(run_id="run_previous", status=SessionStatus.INTERRUPTED)

    class FakeResult:
        final_answer = "done"
        status = type("Status", (), {"value": "completed"})()
        final_report = type(
            "Report",
            (),
            {
                "run_id": "run_new",
                "session_id": "session_continue",
                "task_id": "task_continue",
                "to_dict": lambda self: {"run_id": "run_new", "status": "completed"},
            },
        )()

    class FakeKernel:
        class Context:
            class Identity:
                run_id = "run_new"
                session_id = "session_continue"
                task_id = "task_continue"

            identity = Identity()

        context = Context()
        recovery_gate_decision = None

        class Graph:
            class Trace:
                class Store:
                    run_dir = tmp_path / "work" / "traces" / "runs" / "run_new"

                store = Store()

            trace = Trace()

        graph = Graph()

        def run_task(self, goal: str) -> FakeResult:
            calls.append(("run_task", goal))
            return FakeResult()

    class FakeBootstrap:
        def __init__(self, **kwargs) -> None:
            calls.append(("bootstrap_init", kwargs))

        def boot(self, goal: str) -> FakeKernel:
            calls.append(("boot", goal))
            return FakeKernel()

    monkeypatch.chdir(tmp_path)
    monkeypatch.setattr("singularity.oracle.cli.KernelBootstrap", FakeBootstrap)

    continued = runner.invoke(app, ["continue", "session_continue", "Do the next step"])
    resumed = runner.invoke(app, ["resume", "session_continue"])

    assert continued.exit_code == 0, continued.output
    assert resumed.exit_code == 0, resumed.output
    configs = [
        payload["config"]
        for event, payload in calls
        if event == "bootstrap_init"
    ]
    assert configs[0].resume_session == "session_continue"
    assert configs[0].session_run_mode == "continue"
    assert configs[1].session_run_mode == "resume"
    assert ("run_task", "Do the next step") in calls
    assert ("run_task", "Initial task") in calls


def test_cli_marks_recovery_gate_blocked_run_as_needs_review(tmp_path: Path, monkeypatch) -> None:
    store = SessionStore(tmp_path)
    store.create_session(
        session_id="session_gate",
        project_root=tmp_path,
        user_goal="Recover task",
        task_id="task_gate",
    )
    store.start_run(
        session_id="session_gate",
        run_id="run_previous",
        task_id="task_gate",
        mode=SessionRunMode.NEW,
        user_goal="Recover task",
        trace_run_dir=tmp_path / "work" / "traces" / "runs" / "run_previous",
    )
    store.finish_run(run_id="run_previous", status=SessionStatus.INTERRUPTED)

    decision = RecoveryGateDecision(
        session_id="session_gate",
        mode="resume",
        status=RecoveryGateStatus.NEEDS_REVIEW,
        can_call_model=False,
        blockers=["external_user_change"],
        warnings=[],
        next_action="run sg session show session_gate --timeline",
        resume_context=SessionResumeContext(session_id="session_gate"),
    )

    class FakeResult:
        final_answer = "needs review"
        status = RunStatus.BLOCKED
        final_report = type("Report", (), {"to_dict": lambda self: {"status": "blocked"}})()

    class FakeKernel:
        class Context:
            class Identity:
                run_id = "run_gate"
                session_id = "session_gate"
                task_id = "task_gate"

            identity = Identity()

        context = Context()
        recovery_gate_decision = decision

        class Graph:
            class Trace:
                class Store:
                    run_dir = tmp_path / "work" / "traces" / "runs" / "run_gate"

                store = Store()

            trace = Trace()

            class WorkspaceState:
                baseline = None

                def get_workspace_health(self):
                    from singularity.workspace_state import WorkspaceHealthReport, WorkspaceHealthStatus

                    return WorkspaceHealthReport(status=WorkspaceHealthStatus.CLEAN)

            workspace_state = WorkspaceState()

        graph = Graph()

        def run_task(self, goal: str) -> FakeResult:
            return FakeResult()

    class FakeBootstrap:
        def __init__(self, **kwargs) -> None:
            pass

        def boot(self, goal: str) -> FakeKernel:
            store = SessionStore(tmp_path)
            try:
                store.start_run(
                    session_id="session_gate",
                    run_id="run_gate",
                    task_id="task_gate",
                    mode=SessionRunMode.RESUME,
                    user_goal=goal,
                    trace_run_dir=tmp_path / "work" / "traces" / "runs" / "run_gate",
                )
            finally:
                store.close()
            return FakeKernel()

    monkeypatch.chdir(tmp_path)
    monkeypatch.setattr("singularity.oracle.cli.KernelBootstrap", FakeBootstrap)

    result = runner.invoke(app, ["resume", "session_gate"])
    launch = store.prepare_launch(
        mode=SessionRunMode.RESUME,
        requested_session_id="session_gate",
        user_goal="Recover task",
        project_root=tmp_path,
    )

    assert result.exit_code == 0, result.output
    assert store.load_session("session_gate").status == SessionStatus.NEEDS_REVIEW
    assert launch.session_id == "session_gate"


def test_cli_keeps_agent_blocked_status_when_recovery_gate_allowed_model() -> None:
    decision = RecoveryGateDecision(
        session_id="session_gate",
        mode="resume",
        status=RecoveryGateStatus.READY_TO_RESUME,
        can_call_model=True,
        blockers=[],
        warnings=[],
        next_action="continue",
        resume_context=SessionResumeContext(session_id="session_gate"),
    )

    status = _session_status_from_run_status(
        RunStatus.BLOCKED,
        recovery_gate_decision=decision,
    )

    assert status == SessionStatus.BLOCKED


def test_cli_records_workspace_checkpoint_and_conflict_events(
    tmp_path: Path,
    monkeypatch,
) -> None:
    trace_events: list[tuple[str, dict]] = []

    class FakeResult:
        final_answer = "done"
        status = RunStatus.COMPLETED
        final_report = type("Report", (), {"to_dict": lambda self: {"status": "completed"}})()

    class FakeKernel:
        class Context:
            class Identity:
                run_id = "run_workspace"
                session_id = "session_workspace"
                task_id = "task_workspace"

            identity = Identity()

        context = Context()
        recovery_gate_decision = None

        class Graph:
            class Trace:
                class Store:
                    run_dir = tmp_path / "work" / "traces" / "runs" / "run_workspace"

                store = Store()

                def record(self, event: str, payload: dict) -> None:
                    trace_events.append((event, payload))

            trace = Trace()

            class WorkspaceState:
                baseline = type(
                    "Baseline",
                    (),
                    {"baseline_id": "baseline_1", "snapshots": {"app.py": object()}},
                )()

                def get_workspace_health(self):
                    from singularity.workspace_state import WorkspaceHealthReport, WorkspaceHealthStatus

                    return WorkspaceHealthReport(
                        status=WorkspaceHealthStatus.CONFLICTED,
                        external_changes=["README.md"],
                        rollback_conflicts=["app.py"],
                    )

            workspace_state = WorkspaceState()

        graph = Graph()

        def run_task(self, goal: str) -> FakeResult:
            return FakeResult()

    class FakeBootstrap:
        def __init__(self, **kwargs) -> None:
            pass

        def boot(self, goal: str) -> FakeKernel:
            store = SessionStore(tmp_path)
            try:
                store.create_session(
                    session_id="session_workspace",
                    project_root=tmp_path,
                    user_goal=goal,
                    task_id="task_workspace",
                )
                store.start_run(
                    session_id="session_workspace",
                    run_id="run_workspace",
                    task_id="task_workspace",
                    mode=SessionRunMode.NEW,
                    user_goal=goal,
                    trace_run_dir=tmp_path / "work" / "traces" / "runs" / "run_workspace",
                )
            finally:
                store.close()
            return FakeKernel()

    monkeypatch.chdir(tmp_path)
    monkeypatch.setattr("singularity.oracle.cli.KernelBootstrap", FakeBootstrap)

    result = runner.invoke(app, ["run", "Inspect workspace"])
    detail = SessionStore(tmp_path).show_session("session_workspace")

    assert result.exit_code == 0, result.output
    assert ("workspace.checkpoint_created", {
        "run_id": "run_workspace",
        "session_id": "session_workspace",
        "task_id": "task_workspace",
        "baseline_id": "baseline_1",
        "snapshot_count": 1,
    }) in trace_events
    assert any(event == "workspace.conflict_detected" for event, _payload in trace_events)
    assert any(
        event.event_type == "workspace.conflict_detected"
        for event in detail.timeline
    )
