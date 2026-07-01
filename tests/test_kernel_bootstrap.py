from __future__ import annotations

from pathlib import Path

from singularity.config import ProductionConfig
from singularity.context import ContextManager
from singularity.context.tokens import TokenCounter
from singularity.kernel.bootstrap import KernelBootstrap
from singularity.kernel.exceptions import KernelBootstrapError
from singularity.kernel.graph import AgentGraphBuilder
from singularity.kernel.models import KernelStatus
from singularity.planner import Planner
from singularity.session import SessionRunMode, SessionStatus, SessionStore
from singularity.tool_protocol.models import ToolCallBatch, ToolCallEnvelope, ToolCallPhase
from singularity.tool_protocol.state import ToolProtocolStateStore
from singularity.workspace_state import WorkspaceStateManager


def test_kernel_bootstrap_creates_ready_kernel_and_releases_lock_on_shutdown(
    tmp_path: Path,
    monkeypatch,
) -> None:
    monkeypatch.setenv("SINGULARITY_API_KEY", "test")
    monkeypatch.setenv("SINGULARITY_BASE_URL", "http://localhost/v1")
    monkeypatch.setenv("SINGULARITY_MODEL", "test-model")
    config = ProductionConfig.from_cli(project_root=tmp_path, dry_run=True)

    kernel = KernelBootstrap(project_root=tmp_path, config=config).boot("Build kernel")

    assert kernel.context.status == KernelStatus.READY
    assert kernel.context.workspace_lock_status == "acquired"
    assert (tmp_path / ".singularity" / "locks" / "workspace.lock").exists()

    kernel.shutdown()

    assert not (tmp_path / ".singularity" / "locks" / "workspace.lock").exists()


def test_kernel_bootstrap_records_effective_config_source_trace(
    tmp_path: Path,
    monkeypatch,
) -> None:
    monkeypatch.setenv("SINGULARITY_API_KEY", "test")
    monkeypatch.setenv("SINGULARITY_BASE_URL", "http://localhost/v1")
    monkeypatch.setenv("SINGULARITY_MODEL", "test-model")
    config_dir = tmp_path / ".singularity"
    config_dir.mkdir()
    (config_dir / "config.toml").write_text("max_turns = 5\n", encoding="utf-8")
    config = ProductionConfig.from_cli(project_root=tmp_path)

    kernel = KernelBootstrap(project_root=tmp_path, config=config).boot("Build kernel")

    config_events = [
        event
        for event in kernel.graph.trace.store.query_events()
        if event.component == "config" and event.summary == "Effective component config resolved."
    ]
    assert config_events
    assert config_events[-1].payload["values"]["max_turns"] == 5
    assert (
        config_events[-1].payload["sources"]["max_turns"]
        == "config:.singularity/config.toml"
    )

    kernel.shutdown()


def test_kernel_bootstrap_failure_releases_lock_and_returns_partial_final_report(
    tmp_path: Path,
    monkeypatch,
) -> None:
    monkeypatch.setenv("SINGULARITY_API_KEY", "test")
    monkeypatch.setenv("SINGULARITY_BASE_URL", "http://localhost/v1")
    monkeypatch.setenv("SINGULARITY_MODEL", "test-model")
    config = ProductionConfig.from_cli(project_root=tmp_path, dry_run=True)

    class FailingFactory(AgentGraphBuilder):
        def build(self, **kwargs):
            raise RuntimeError("graph failed")

    try:
        KernelBootstrap(
            project_root=tmp_path,
            config=config,
            component_factory=FailingFactory(),
        ).boot("Build kernel")
    except KernelBootstrapError as exc:
        report = exc.final_report
    else:
        raise AssertionError("KernelBootstrapError was not raised.")

    assert not (tmp_path / ".singularity" / "locks" / "workspace.lock").exists()
    assert report is not None
    assert report.shutdown_reason == "bootstrap_failed"
    assert report.cleanup_status == "completed"
    assert report.diagnostics_count == 1
    assert report.workspace_lock_status == "released"


def test_kernel_bootstrap_resume_uses_stable_session_and_new_run_identity(
    tmp_path: Path,
    monkeypatch,
) -> None:
    monkeypatch.setenv("SINGULARITY_API_KEY", "test")
    monkeypatch.setenv("SINGULARITY_BASE_URL", "http://localhost/v1")
    monkeypatch.setenv("SINGULARITY_MODEL", "test-model")
    store = SessionStore(tmp_path)
    store.create_session(
        session_id="session_resume",
        project_root=tmp_path,
        user_goal="Recover task",
        task_id="task_resume",
    )
    store.start_run(
        session_id="session_resume",
        run_id="run_previous",
        task_id="task_resume",
        mode=SessionRunMode.NEW,
        user_goal="Recover task",
        trace_run_dir=tmp_path / ".singularity" / "traces" / "runs" / "run_previous",
    )
    store.finish_run(run_id="run_previous", status=SessionStatus.INTERRUPTED)
    config = ProductionConfig.from_cli(
        project_root=tmp_path,
        dry_run=True,
        resume_session="session_resume",
        session_run_mode="resume",
        cli_overrides={"resume_session", "session_run_mode", "dry_run"},
    )

    kernel = KernelBootstrap(project_root=tmp_path, config=config).boot("Recover task")

    assert kernel.context.identity.session_id == "session_resume"
    assert kernel.context.identity.task_id == "task_resume"
    assert kernel.context.identity.run_id != "session_resume"
    assert kernel.context.identity.run_id != "run_previous"
    assert kernel.graph.trace.run_id == kernel.context.identity.run_id
    kernel.shutdown()


def test_kernel_bootstrap_resume_inspects_default_trace_tool_protocol_state(
    tmp_path: Path,
    monkeypatch,
) -> None:
    monkeypatch.setenv("SINGULARITY_API_KEY", "test")
    monkeypatch.setenv("SINGULARITY_BASE_URL", "http://localhost/v1")
    monkeypatch.setenv("SINGULARITY_MODEL", "test-model")
    previous_trace_dir = tmp_path / "work" / "traces" / "runs" / "run_previous"
    tool_store = ToolProtocolStateStore(previous_trace_dir / "tool_protocol.sqlite3")
    envelope = ToolCallEnvelope(
        protocol_version="1.0",
        run_id="run_previous",
        session_id="session_tool",
        task_id="task_tool",
        phase_id="phase_1",
        model_request_id="request_1",
        model_response_id="response_1",
        assistant_message_id="assistant_1",
        tool_call_id="call_pending",
        tool_name="read_file",
        raw_arguments='{"path":"README.md"}',
        parsed_arguments={"path": "README.md"},
        normalized_arguments={"path": "README.md"},
    )
    batch = tool_store.create_batch(
        ToolCallBatch(
            batch_id="batch_pending",
            run_id="run_previous",
            session_id="session_tool",
            task_id="task_tool",
            phase_id="phase_1",
            model_request_id="request_1",
            model_response_id="response_1",
            assistant_message={"id": "assistant_1", "role": "assistant", "content": None},
            tool_calls=[envelope],
        )
    )
    tool_store.upsert_record(batch.tool_calls[0], phase=ToolCallPhase.PROPOSED)
    tool_store.close()
    store = SessionStore(tmp_path)
    store.create_session(
        session_id="session_tool",
        project_root=tmp_path,
        user_goal="Recover pending tool",
        task_id="task_tool",
    )
    store.start_run(
        session_id="session_tool",
        run_id="run_previous",
        task_id="task_tool",
        mode=SessionRunMode.NEW,
        user_goal="Recover pending tool",
        trace_run_dir=previous_trace_dir,
    )
    store.finish_run(run_id="run_previous", status=SessionStatus.INTERRUPTED)
    store.close()
    config = ProductionConfig.from_cli(
        project_root=tmp_path,
        dry_run=True,
        resume_session="session_tool",
        session_run_mode="resume",
        cli_overrides={"resume_session", "session_run_mode", "dry_run"},
    )

    kernel = KernelBootstrap(project_root=tmp_path, config=config).boot("Recover pending tool")

    assert kernel.recovery_gate_decision is not None
    assert kernel.recovery_gate_decision.can_call_model is False
    assert "pending_tool_call" in kernel.recovery_gate_decision.blockers
    kernel.shutdown()


def test_kernel_bootstrap_resume_inspects_previous_context_recovery_state(
    tmp_path: Path,
    monkeypatch,
) -> None:
    monkeypatch.setenv("SINGULARITY_API_KEY", "test")
    monkeypatch.setenv("SINGULARITY_BASE_URL", "http://localhost/v1")
    monkeypatch.setenv("SINGULARITY_MODEL", "test-model")
    previous_trace_dir = tmp_path / "work" / "traces" / "runs" / "run_previous_context"
    context = ContextManager(
        system_prompt="system",
        user_goal="Recover context state",
        db_path=previous_trace_dir / "context.sqlite3",
        run_id="run_previous_context",
        session_id="session_context",
        task_id="task_context",
        token_counter=TokenCounter(model="gpt-4o-mini"),
    )
    context.add_assistant_message(
        {
            "role": "assistant",
            "content": None,
            "tool_calls": [
                {
                    "id": "call_pending_context",
                    "type": "function",
                    "function": {"name": "read_file", "arguments": "{}"},
                }
            ],
        }
    )
    context.add_command_observation(
        {
            "command_id": "cmd_running",
            "process_id": "proc_running",
            "command_preview": "python -m http.server",
            "exit_code": None,
            "status": "running",
            "stdout_preview": "",
            "stderr_preview": "",
            "output_ref": None,
            "resource_limits": {},
            "policy_decision_id": None,
        }
    )
    context.close()
    planner = Planner(tmp_path, session_id="session_context", task_id="task_context")
    planner.start_task("Recover context state")
    store = SessionStore(tmp_path)
    store.create_session(
        session_id="session_context",
        project_root=tmp_path,
        user_goal="Recover context state",
        task_id="task_context",
    )
    store.start_run(
        session_id="session_context",
        run_id="run_previous_context",
        task_id="task_context",
        mode=SessionRunMode.NEW,
        user_goal="Recover context state",
        trace_run_dir=previous_trace_dir,
    )
    store.finish_run(run_id="run_previous_context", status=SessionStatus.INTERRUPTED)
    store.close()
    config = ProductionConfig.from_cli(
        project_root=tmp_path,
        dry_run=True,
        resume_session="session_context",
        session_run_mode="resume",
        cli_overrides={"resume_session", "session_run_mode", "dry_run"},
    )

    kernel = KernelBootstrap(project_root=tmp_path, config=config).boot("Recover context state")

    assert kernel.recovery_gate_decision is not None
    assert kernel.recovery_gate_decision.can_call_model is False
    assert "pending_tool_call" in kernel.recovery_gate_decision.blockers
    assert "running_tool_call" in kernel.recovery_gate_decision.blockers
    kernel.shutdown()


def test_kernel_bootstrap_resume_uses_requested_workspace_session_for_gate(
    tmp_path: Path,
    monkeypatch,
) -> None:
    monkeypatch.setenv("SINGULARITY_API_KEY", "test")
    monkeypatch.setenv("SINGULARITY_BASE_URL", "http://localhost/v1")
    monkeypatch.setenv("SINGULARITY_MODEL", "test-model")
    (tmp_path / "target.txt").write_text("old\n", encoding="utf-8")
    first_workspace = WorkspaceStateManager(tmp_path)
    first_workspace.begin_session(task_id="task_first", session_id="session_first")
    target_workspace = WorkspaceStateManager(tmp_path)
    target_workspace.begin_session(task_id="task_target", session_id="session_target")
    (tmp_path / "target.txt").write_text("user edit\n", encoding="utf-8")
    target_workspace.record_external_changes()
    first_workspace.close()
    target_workspace.close()
    store = SessionStore(tmp_path)
    for session_id, task_id in {
        "session_first": "task_first",
        "session_target": "task_target",
    }.items():
        store.create_session(
            session_id=session_id,
            project_root=tmp_path,
            user_goal="Recover task",
            task_id=task_id,
        )
        store.start_run(
            session_id=session_id,
            run_id=f"run_{session_id}",
            task_id=task_id,
            mode=SessionRunMode.NEW,
            user_goal="Recover task",
            trace_run_dir=tmp_path / "work" / "traces" / "runs" / f"run_{session_id}",
        )
        store.finish_run(run_id=f"run_{session_id}", status=SessionStatus.INTERRUPTED)
    store.close()
    config = ProductionConfig.from_cli(
        project_root=tmp_path,
        dry_run=True,
        resume_session="session_target",
        session_run_mode="resume",
        cli_overrides={"resume_session", "session_run_mode", "dry_run"},
    )

    kernel = KernelBootstrap(project_root=tmp_path, config=config).boot("Recover task")

    assert kernel.recovery_gate_decision is not None
    assert "external_user_change" in kernel.recovery_gate_decision.blockers
    assert kernel.recovery_gate_decision.resume_context.workspace["external_changes"] == [
        "target.txt"
    ]
    kernel.shutdown()
