from __future__ import annotations

from pathlib import Path

from singularity.context.store import ObservationStore
from singularity.kernel.recovery import RecoveryReport
from singularity.observability import TraceRecorder
from singularity.planner import Planner
from singularity.session import (
    RecoveryGateDecision,
    RecoveryGateStatus,
    SessionHistoryReader,
    SessionResumeContext,
    SessionRunMode,
    SessionStatus,
    SessionStore,
)
from singularity.session.recovery import SessionRecoveryGate
from singularity.workspace_state import WorkspaceHealthReport, WorkspaceHealthStatus


def test_recovery_gate_allows_clean_continue_context() -> None:
    decision = SessionRecoveryGate().evaluate(
        session_id="session_1",
        mode="continue",
        workspace_health=WorkspaceHealthReport(status=WorkspaceHealthStatus.CLEAN),
        crash_recovery=RecoveryReport(recovered=False),
        tool_protocol_report={"next_action": "request_model"},
        context_recovery={"recommended_next_action": "request_model"},
        planner_state={"status": "completed", "current_phase": "finalizing"},
    )

    assert decision.status == RecoveryGateStatus.READY_TO_CONTINUE
    assert decision.can_call_model is True
    assert decision.blockers == []
    assert decision.resume_context.planner["status"] == "completed"


def test_recovery_gate_allows_new_session_before_planner_state_exists() -> None:
    decision = SessionRecoveryGate().evaluate(
        session_id="session_new",
        mode="new",
        workspace_health=WorkspaceHealthReport(status=WorkspaceHealthStatus.CLEAN),
        crash_recovery=RecoveryReport(recovered=False),
        tool_protocol_report={"next_action": "request_model"},
        context_recovery={},
        planner_state={"status": "missing", "blockers": ["planner_state_missing"]},
    )

    assert decision.status == RecoveryGateStatus.READY_TO_CONTINUE
    assert decision.can_call_model is True
    assert "planner_state_missing" not in decision.blockers


def test_recovery_gate_blocks_external_changes_before_model_call() -> None:
    decision = SessionRecoveryGate().evaluate(
        session_id="session_1",
        mode="continue",
        workspace_health=WorkspaceHealthReport(
            status=WorkspaceHealthStatus.CONFLICTED,
            external_changes=["README.md"],
        ),
        crash_recovery=RecoveryReport(recovered=False),
        tool_protocol_report={"next_action": "request_model"},
        context_recovery={"recommended_next_action": "request_model"},
        planner_state={"status": "interrupted"},
    )

    assert decision.status == RecoveryGateStatus.NEEDS_REVIEW
    assert decision.can_call_model is False
    assert "external_user_change" in decision.blockers
    assert "README.md" in decision.resume_context.workspace["external_changes"]


def test_recovery_gate_blocks_unfinished_mutation_leftover_sandbox_and_pending_tool() -> None:
    decision = SessionRecoveryGate().evaluate(
        session_id="session_1",
        mode="resume",
        workspace_health=WorkspaceHealthReport(status=WorkspaceHealthStatus.CLEAN),
        crash_recovery=RecoveryReport(
            recovered=True,
            unfinished_mutations=["tx_1"],
            leftover_sandboxes=[r"C:\repo\work\sandboxes\sandbox_1"],
        ),
        tool_protocol_report={
            "next_action": "await_tool_result",
            "running_call_ids": ["call_1"],
        },
        context_recovery={"recommended_next_action": "request_model"},
        planner_state={"status": "interrupted"},
    )

    assert decision.status == RecoveryGateStatus.BLOCKED
    assert decision.can_call_model is False
    assert decision.blockers == [
        "unfinished_mutation",
        "leftover_sandbox",
        "running_tool_call",
    ]


def test_recovery_gate_requires_review_for_stale_lock_before_model_call() -> None:
    decision = SessionRecoveryGate().evaluate(
        session_id="session_1",
        mode="resume",
        workspace_health=WorkspaceHealthReport(status=WorkspaceHealthStatus.CLEAN),
        crash_recovery=RecoveryReport(recovered=True, stale_lock_detected=True),
        tool_protocol_report={"next_action": "request_model"},
        context_recovery={"recommended_next_action": "request_model"},
        planner_state={"status": "interrupted"},
    )

    assert decision.status == RecoveryGateStatus.NEEDS_REVIEW
    assert decision.can_call_model is False
    assert "stale_lock_detected" in decision.blockers


def test_recovery_gate_blocks_pending_approval_and_pending_tool() -> None:
    approval = SessionRecoveryGate().evaluate(
        session_id="session_1",
        mode="resume",
        workspace_health=WorkspaceHealthReport(status=WorkspaceHealthStatus.CLEAN),
        crash_recovery=RecoveryReport(recovered=False),
        tool_protocol_report={
            "next_action": "resume_pending_approval",
            "pending_approval_call_ids": ["call_approval"],
        },
        context_recovery={},
        planner_state={"status": "interrupted"},
    )
    pending_tool = SessionRecoveryGate().evaluate(
        session_id="session_1",
        mode="resume",
        workspace_health=WorkspaceHealthReport(status=WorkspaceHealthStatus.CLEAN),
        crash_recovery=RecoveryReport(recovered=False),
        tool_protocol_report={
            "next_action": "execute_pending_tool",
            "pending_call_ids": ["call_pending"],
        },
        context_recovery={},
        planner_state={"status": "interrupted"},
    )

    assert approval.can_call_model is False
    assert approval.blockers == ["pending_approval"]
    assert pending_tool.can_call_model is False
    assert pending_tool.blockers == ["pending_tool_call"]


def test_recovery_gate_blocks_pending_context_tool_and_active_process() -> None:
    decision = SessionRecoveryGate().evaluate(
        session_id="session_1",
        mode="resume",
        workspace_health=WorkspaceHealthReport(status=WorkspaceHealthStatus.CLEAN),
        crash_recovery=RecoveryReport(recovered=False),
        tool_protocol_report={"next_action": "request_model"},
        context_recovery={
            "recommended_next_action": "resume_process_observation",
            "pending_tool_calls": [{"id": "call_pending", "function": {"name": "read_file"}}],
            "active_process_sessions": ["proc_1"],
        },
        planner_state={"status": "interrupted"},
    )

    assert decision.can_call_model is False
    assert decision.status == RecoveryGateStatus.BLOCKED
    assert decision.blockers == ["pending_tool_call", "running_tool_call"]


def test_recovery_gate_needs_review_when_context_recovery_inspection_failed() -> None:
    decision = SessionRecoveryGate().evaluate(
        session_id="session_1",
        mode="resume",
        workspace_health=WorkspaceHealthReport(status=WorkspaceHealthStatus.CLEAN),
        crash_recovery=RecoveryReport(recovered=False),
        tool_protocol_report={"next_action": "request_model"},
        context_recovery={
            "recommended_next_action": "needs_review",
            "context_recovery_failed": True,
            "recovery_warnings": ["context recovery inspect failed: DatabaseError"],
        },
        planner_state={"status": "interrupted"},
    )

    assert decision.can_call_model is False
    assert decision.status == RecoveryGateStatus.NEEDS_REVIEW
    assert decision.blockers == ["context_recovery_failed"]


def test_resume_context_filters_sensitive_execution_payloads() -> None:
    context = SessionResumeContext.from_sources(
        session_id="session_1",
        user_goal="continue work",
        dialogue=[
            {"role": "user", "content": "previous instruction"},
            {"role": "assistant", "content": "I changed app.py"},
            {"role": "tool", "content": "raw secret output", "tool_call_id": "call_1"},
        ],
        planner={"status": "recovering", "current_phase": "running_verification"},
        workspace={"agent_changes": ["app.py"], "external_changes": []},
        verification={"last_status": "failed", "stdout": "very long raw output"},
        tool_protocol={"next_action": "request_model", "raw_args": {"token": "secret"}},
        failures={"summary": "pytest failed"},
    )

    payload = context.to_model_context()

    assert payload["session_id"] == "session_1"
    assert payload["dialogue_summary"] == [
        {"role": "user", "content": "previous instruction"},
        {"role": "assistant", "content": "I changed app.py"},
    ]
    assert "planner" not in payload
    assert "workspace" not in payload
    assert "verification" not in payload
    assert "tool_protocol" not in payload
    assert "failures" not in payload
    assert "stdout" not in payload["verification_summary"]
    assert "raw_args" not in payload["tool_protocol_summary"]
    assert "raw secret output" not in str(payload)


def test_resume_context_removes_env_assignment_shapes_from_safe_projection() -> None:
    context = SessionResumeContext.from_sources(
        session_id="session_1",
        user_goal="Recover after OPENAI_API_KEY=sk-secret-value",
        current_instruction="provider status SINGULARITY_API_KEY=present(redacted)",
        dialogue=[
            {
                "role": "assistant",
                "content": "provider status SINGULARITY_API_KEY=present(redacted)",
            }
        ],
        verification={
            "provider_env_status": "SINGULARITY_API_KEY=present(redacted); SINGULARITY_MODEL=present",
            "checks": [{"name": "smoke", "stdout": "OPENAI_API_KEY=sk-secret-value"}],
        },
        failures={
            "summary": "Model turn failed after OPENAI_API_KEY=sk-secret-value appeared in raw diagnostics."
        },
    )

    payload = context.to_model_context()
    dumped = str(payload)

    assert "SINGULARITY_API_KEY=" not in dumped
    assert "OPENAI_API_KEY=" not in dumped
    assert "sk-secret-value" not in dumped
    assert "stdout" not in dumped
    assert payload["verification_summary"]["provider_env_status"]["SINGULARITY_API_KEY"] == "present_redacted"
    assert payload["verification_summary"]["provider_env_status"]["SINGULARITY_MODEL"] == "present"
    assert payload["user_goal"] == "Recover after OPENAI_API_KEY <redacted>"
    assert payload["current_instruction"] == "provider status SINGULARITY_API_KEY <redacted>"


def test_recovery_gate_decision_round_trips_for_trace_and_report() -> None:
    decision = RecoveryGateDecision(
        session_id="session_1",
        mode="resume",
        status=RecoveryGateStatus.BLOCKED,
        can_call_model=False,
        blockers=["rollback_conflict"],
        warnings=["review workspace"],
        next_action="run sg session show session_1 --timeline",
        resume_context=SessionResumeContext(session_id="session_1"),
    )

    payload = decision.to_dict()

    assert payload["status"] == "blocked"
    assert payload["can_call_model"] is False
    assert payload["resume_context"]["session_id"] == "session_1"


def test_session_history_resume_context_preserves_stable_goal_for_continue(tmp_path: Path) -> None:
    planner = Planner(tmp_path, session_id="session_continue", task_id="task_continue")
    planner.start_task("Implement the original feature")
    planner.continue_with_instruction("Also add the CLI output")
    store = SessionStore(tmp_path)
    store.create_session(
        session_id="session_continue",
        project_root=tmp_path,
        user_goal="Implement the original feature",
        task_id="task_continue",
    )
    store.start_run(
        session_id="session_continue",
        run_id="run_initial",
        task_id="task_continue",
        mode=SessionRunMode.NEW,
        user_goal="Implement the original feature",
        trace_run_dir=tmp_path / "work" / "traces" / "runs" / "run_initial",
    )
    store.finish_run(run_id="run_initial", status=SessionStatus.INTERRUPTED)
    store.start_run(
        session_id="session_continue",
        run_id="run_continue",
        task_id="task_continue",
        mode=SessionRunMode.CONTINUE,
        user_goal="Also add the CLI output",
        trace_run_dir=tmp_path / "work" / "traces" / "runs" / "run_continue",
    )
    store.close()

    context = SessionHistoryReader(tmp_path).build_resume_context(
        session_id="session_continue",
        user_goal="Also add JSON output",
        workspace_health=WorkspaceHealthReport(status=WorkspaceHealthStatus.CLEAN),
        current_run_id="run_continue",
        task_id="task_continue",
    )

    assert "Implement the original feature" in context.user_goal
    assert "Also add the CLI output" in context.user_goal
    assert context.current_instruction == "Also add JSON output"


def test_session_history_resume_context_reads_previous_context_and_trace_summary(
    tmp_path: Path,
) -> None:
    previous_trace_dir = tmp_path / "work" / "traces" / "runs" / "run_previous"
    context_store = ObservationStore(previous_trace_dir / "context.sqlite3")
    context_store.append_message(
        run_id="run_previous",
        message={"role": "user", "content": "read the config"},
    )
    context_store.append_message(
        run_id="run_previous",
        message={"role": "assistant", "content": "I inspected config.py"},
    )
    context_store.append_message(
        run_id="run_previous",
        message={"role": "tool", "content": "raw secret output"},
    )
    context_store.close()
    trace = TraceRecorder.create(
        tmp_path,
        run_id="run_previous",
        session_id="session_history",
    )
    trace.record(
        "planner",
        {
            "decision": "tool_result",
            "run_id": "run_previous",
            "session_id": "session_history",
            "task_id": "task_history",
            "error_code": "tool_failed",
            "reason": "Tool failed during verification.",
        },
    )
    store = SessionStore(tmp_path)
    store.create_session(
        session_id="session_history",
        project_root=tmp_path,
        user_goal="Recover history",
        task_id="task_history",
    )
    store.start_run(
        session_id="session_history",
        run_id="run_previous",
        task_id="task_history",
        mode=SessionRunMode.NEW,
        user_goal="Recover history",
        trace_run_dir=previous_trace_dir,
    )
    store.finish_run(run_id="run_previous", status=SessionStatus.INTERRUPTED)
    store.start_run(
        session_id="session_history",
        run_id="run_current",
        task_id="task_history",
        mode=SessionRunMode.RESUME,
        user_goal="Recover history",
        trace_run_dir=tmp_path / "work" / "traces" / "runs" / "run_current",
    )
    store.close()

    context = SessionHistoryReader(tmp_path).build_resume_context(
        session_id="session_history",
        user_goal="Recover history",
        workspace_health=WorkspaceHealthReport(status=WorkspaceHealthStatus.CLEAN),
        current_run_id="run_current",
        task_id="task_history",
    )

    assert context.dialogue_summary == [
        {"role": "user", "content": "read the config"},
        {"role": "assistant", "content": "I inspected config.py"},
    ]
    assert context.verification["previous_trace"]["failed_actions"] == 1
    assert "raw secret output" not in str(context.to_model_context())
