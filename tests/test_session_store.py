from __future__ import annotations

from pathlib import Path

from singularity.session import (
    SessionCheckpointKind,
    SessionRunMode,
    SessionState,
    SessionStatus,
    SessionStore,
)


def test_session_store_indexes_sessions_runs_checkpoints_and_timeline(tmp_path: Path) -> None:
    store = SessionStore(tmp_path)

    session = store.create_session(
        session_id="session_1",
        project_root=tmp_path,
        user_goal="Fix the bug",
        task_id="task_1",
    )
    run = store.start_run(
        session_id="session_1",
        run_id="run_1",
        task_id="task_1",
        mode=SessionRunMode.NEW,
        user_goal="Fix the bug",
        trace_run_dir=tmp_path / "work" / "traces" / "runs" / "run_1",
    )
    store.record_checkpoint(
        session_id="session_1",
        run_id="run_1",
        task_id="task_1",
        kind=SessionCheckpointKind.WORKSPACE,
        summary="workspace baseline captured",
        payload={"changed_files": []},
    )
    store.append_timeline_event(
        session_id="session_1",
        run_id="run_1",
        task_id="task_1",
        event_type="session.created",
        summary="Session created.",
        payload={"status": "active"},
    )
    store.finish_run(
        run_id="run_1",
        status=SessionStatus.INTERRUPTED,
        final_report_ref="work/traces/runs/run_1/final_report.json",
        summary={"last_status": "interrupted"},
    )

    listed = store.list_sessions()
    loaded = store.load_session("session_1")
    shown = store.show_session("session_1")

    assert session.session_id == "session_1"
    assert run.mode == SessionRunMode.NEW
    assert listed[0].session_id == "session_1"
    assert listed[0].status == SessionStatus.INTERRUPTED
    assert loaded is not None
    assert loaded.last_run_id == "run_1"
    assert shown.session.session_id == "session_1"
    assert shown.runs[0].run_id == "run_1"
    assert shown.checkpoints[0].kind == SessionCheckpointKind.WORKSPACE
    assert shown.timeline[0].event_type == "session.created"


def test_session_store_records_continue_and_resume_commands(tmp_path: Path) -> None:
    store = SessionStore(tmp_path)
    store.create_session(
        session_id="session_2",
        project_root=tmp_path,
        user_goal="Implement feature",
        task_id="task_2",
    )
    store.start_run(
        session_id="session_2",
        run_id="run_2",
        task_id="task_2",
        mode=SessionRunMode.RESUME,
        user_goal="Implement feature",
        trace_run_dir=tmp_path / "work" / "traces" / "runs" / "run_2",
    )
    store.finish_run(run_id="run_2", status=SessionStatus.INTERRUPTED)

    entry = store.list_sessions()[0]

    assert entry.continue_command == 'sg continue session_2 "<new instruction>"'
    assert entry.resume_command == "sg resume session_2"
    assert entry.show_command == "sg session show session_2"
    assert entry.state == SessionState.RECOVERABLE


def test_session_store_prepare_launch_creates_new_run_for_same_resumed_session(tmp_path: Path) -> None:
    store = SessionStore(tmp_path)
    store.create_session(
        session_id="session_3",
        project_root=tmp_path,
        user_goal="Recover task",
        task_id="task_3",
    )
    store.start_run(
        session_id="session_3",
        run_id="run_old",
        task_id="task_3",
        mode=SessionRunMode.NEW,
        user_goal="Recover task",
        trace_run_dir=tmp_path / "work" / "traces" / "runs" / "run_old",
    )
    store.finish_run(run_id="run_old", status=SessionStatus.INTERRUPTED)

    first = store.prepare_launch(
        mode=SessionRunMode.RESUME,
        requested_session_id="session_3",
        user_goal="Recover task",
        project_root=tmp_path,
    )
    second = store.prepare_launch(
        mode=SessionRunMode.RESUME,
        requested_session_id="session_3",
        user_goal="Recover task",
        project_root=tmp_path,
    )

    assert first.session_id == "session_3"
    assert first.task_id == "task_3"
    assert first.previous_run_id == "run_old"
    assert first.run_id != "session_3"
    assert first.run_id != second.run_id


def test_session_store_resume_rejects_non_recoverable_session(tmp_path: Path) -> None:
    store = SessionStore(tmp_path)
    store.create_session(
        session_id="session_done",
        project_root=tmp_path,
        user_goal="Done task",
        task_id="task_done",
    )
    store.start_run(
        session_id="session_done",
        run_id="run_done",
        task_id="task_done",
        mode=SessionRunMode.NEW,
        user_goal="Done task",
        trace_run_dir=tmp_path / "work" / "traces" / "runs" / "run_done",
    )
    store.finish_run(run_id="run_done", status=SessionStatus.COMPLETED)

    try:
        store.prepare_launch(
            mode=SessionRunMode.RESUME,
            requested_session_id="session_done",
            user_goal="Done task",
            project_root=tmp_path,
        )
    except ValueError as exc:
        assert "not recoverable" in str(exc)
    else:
        raise AssertionError("resume should reject a closed session")


def test_session_store_resume_allows_recovery_gate_review_and_blocked_states(tmp_path: Path) -> None:
    store = SessionStore(tmp_path)
    for session_id, status in {
        "session_active": SessionStatus.ACTIVE,
        "session_review": SessionStatus.NEEDS_REVIEW,
        "session_blocked": SessionStatus.BLOCKED,
    }.items():
        store.create_session(
            session_id=session_id,
            project_root=tmp_path,
            user_goal="Recover task",
            task_id=f"task_{session_id}",
        )
        store.start_run(
            session_id=session_id,
            run_id=f"run_{session_id}",
            task_id=f"task_{session_id}",
            mode=SessionRunMode.NEW,
            user_goal="Recover task",
            trace_run_dir=tmp_path / "work" / "traces" / "runs" / f"run_{session_id}",
        )
        store.finish_run(run_id=f"run_{session_id}", status=status)

        launch = store.prepare_launch(
            mode=SessionRunMode.RESUME,
            requested_session_id=session_id,
            user_goal="Recover task",
            project_root=tmp_path,
        )

        assert launch.session_id == session_id
        assert launch.previous_run_id == f"run_{session_id}"
        assert launch.previous_trace_run_dir.endswith(f"run_{session_id}")
