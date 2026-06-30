from __future__ import annotations

import json
import sys
from pathlib import Path
from typing import Any

import pytest
from typer.testing import CliRunner

from singularity.cli import app
from singularity.command import (
    CommandDecision,
    CommandExecutor,
    CommandPolicyResult,
    CommandRequest,
    CommandRisk,
    ExecutionStatus,
    SemanticStatus,
)
from singularity.context.manager import ContextManager
from singularity.observability.models import TraceEventType
from singularity.observability.recorder import TraceRecorder
from singularity.planner import Planner, TaskStatus
from singularity.planner.finalizer import Finalizer
from singularity.policy import (
    Capability,
    OperationKind,
    PolicyComponent,
    PolicyConfig,
    PolicyEngine,
    PolicyRequest,
    PolicySubject,
    ResourceRef,
)
from singularity.policy.approval import ApprovalGate
from singularity.policy.exceptions import ApprovalRequired
from singularity.policy.models import DecisionOutcome
from singularity.policy.permissions import ApprovalPolicy, PermissionProfile, PermissionProfileName
from singularity.sandbox import (
    SandboxManager,
    SandboxNetworkMode,
    SandboxProfileName,
    SandboxRequest,
    default_sandbox_profile,
)
from singularity.tools import ToolExecutor, ToolPolicy, ToolRegistry
from singularity.verification import VerificationRunner
from singularity.workspace import CreateFile, WorkspaceMutationManager
from tests.tool_executor_helpers import make_test_policy_engine


def _unrestricted_policy_engine(tmp_path: Path) -> PolicyEngine:
    return PolicyEngine(
        PolicyConfig(
            workspace_root=tmp_path,
            permission_profile=PermissionProfile.default_for_workspace(
                tmp_path,
                profile=PermissionProfileName.DANGER_FULL_ACCESS,
            ),
        )
    )


def _event_values(trace: TraceRecorder) -> list[str]:
    return [event.event_type.value for event in trace.store.query_events()]


def test_tool_executor_dispatch_emits_structured_trace(tmp_path: Path) -> None:
    trace = TraceRecorder.create(tmp_path, run_id="run_tool", session_id="session_tool")
    component = ToolExecutor(
        registry=ToolRegistry(tmp_path),
        policy=ToolPolicy.read_only(),
        trace=trace,
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )

    result = component.execute_tool_call(
        {
            "id": "call_list",
            "type": "function",
            "function": {"name": "list_files", "arguments": json.dumps({"path": "."})},
        }
    )

    assert result.ok is True
    values = _event_values(trace)
    assert TraceEventType.TOOL_VALIDATION_STARTED.value in values
    assert TraceEventType.TOOL_DISPATCH_STARTED.value in values
    assert TraceEventType.TOOL_DISPATCH_COMPLETED.value in values
    assert values.count(TraceEventType.TOOL_DISPATCH_COMPLETED.value) == 1


def test_policy_engine_and_approval_gate_emit_trace(tmp_path: Path) -> None:
    trace = TraceRecorder.create(tmp_path, run_id="run_policy", session_id="session_policy")
    config = PolicyConfig(
        workspace_root=tmp_path,
        permission_profile=PermissionProfile.default_for_workspace(
            tmp_path,
            approval_policy=ApprovalPolicy.NEVER,
        ),
        audit_log_path=tmp_path / "audit.jsonl",
    )
    policy = PolicyEngine(config, trace=trace)
    request = PolicyRequest(
        session_id="session_policy",
        task_id="task_policy",
        phase_id="phase",
        action_id="action",
        component=PolicyComponent.TOOL,
        operation=OperationKind.DELETE_FILE,
        capability=Capability.DELETE_FILE,
        subject=PolicySubject(subject_type="component", name="test"),
        resource=ResourceRef(
            resource_type="file",
            identifier=".env",
            workspace_relative=True,
        ),
        reason="delete .env",
        proposed_by_model=True,
        workspace_root=str(tmp_path),
    )

    decision = policy.enforce(request)

    assert decision.outcome == DecisionOutcome.DENY
    with pytest.raises(ApprovalRequired):
        ApprovalGate(config, trace=trace).resolve(
            request,
            decision.model_copy_with(outcome=DecisionOutcome.REQUIRE_REVIEW),
        )
    values = _event_values(trace)
    assert TraceEventType.POLICY_REQUESTED.value in values
    assert TraceEventType.POLICY_BLOCKED.value in values
    assert TraceEventType.APPROVAL_REQUESTED.value in values
    assert TraceEventType.APPROVAL_DENIED.value in values


def test_command_mutation_planner_context_and_final_report_trace(tmp_path: Path) -> None:
    trace = TraceRecorder.create(tmp_path, run_id="run_all", session_id="session_all")
    planner = Planner(tmp_path, session_id="session_all", task_id="task_all", trace=trace)
    planner.start_task("Add a file and verify it")
    planner.state.status = TaskStatus.APPLYING_CHANGES
    planner.state.current_phase = "applying_changes"
    planner.plan.current_phase = "applying_changes"

    mutation = WorkspaceMutationManager(tmp_path, trace=trace, planner=planner)
    mutation_result = mutation.apply_operations(
        [CreateFile(path="app.py", content="print('ok')\n")],
        intent="add app",
        created_by="test",
        tool_call_id="call_mutation",
    )
    command = CommandExecutor(
        tmp_path,
        trace=trace,
        planner=planner,
        policy_engine=_unrestricted_policy_engine(tmp_path),
    )
    command_result = command.run(
        CommandRequest(
            argv=[sys.executable, "-c", "print('ok')"],
            cwd=".",
            command_id="cmd_ok",
        )
    )

    context = ContextManager(
        system_prompt="system",
        user_goal="goal",
        run_id=trace.run_id,
        trace=trace,
    )
    context.add_trace_summary(trace.context_summary(task_id="task_all"))
    messages = context.messages()
    report = Finalizer().build(
        state=planner.state,
        evidence=planner.evidence,
        trace_summary=trace.final_report_summary(task_id="task_all"),
    )

    assert mutation_result.ok is True
    assert command_result.exit_code == 0
    values = _event_values(trace)
    assert TraceEventType.TASK_STARTED.value in values
    assert TraceEventType.MUTATION_TRANSACTION_STARTED.value in values
    assert TraceEventType.MUTATION_APPLIED.value in values
    assert TraceEventType.COMMAND_STARTED.value in values
    assert TraceEventType.COMMAND_COMPLETED.value in values
    assert TraceEventType.CONTEXT_RENDERED_FOR_MODEL.value in values
    assert messages[-1]["content"].startswith("[trace]")
    assert report.execution_trace_summary["commands_executed"] >= 1
    assert "sk-" not in str(report.to_dict())


def test_trace_cli_show_and_timeline(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.chdir(tmp_path)
    trace = TraceRecorder.create(tmp_path, run_id="run_cli", session_id="session_cli")
    trace.emit(
        TraceEventType.COMMAND_COMPLETED,
        component="command",
        summary="command done",
        ids={"task_id": "task_cli", "command_id": "cmd_cli"},
    )
    runner = CliRunner()

    show = runner.invoke(app, ["trace", "show", "run_cli"])
    timeline = runner.invoke(app, ["trace", "timeline", "run_cli"])

    assert show.exit_code == 0
    assert "run_cli" in show.output
    assert "total_events" in show.output
    assert timeline.exit_code == 0
    assert "command.completed" in timeline.output


def test_trace_artifacts_cli_shows_handle_not_absolute_path(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.chdir(tmp_path)
    trace = TraceRecorder.create(tmp_path, run_id="run_artifacts", session_id="session")
    artifact = trace.write_artifact(
        kind="report",
        text="artifact body",
        summary="report",
    )
    runner = CliRunner()

    result = runner.invoke(app, ["trace", "artifacts", "run_artifacts"])

    assert result.exit_code == 0
    assert artifact.artifact_id in result.output
    assert "handle=artifacts/" in result.output
    assert str(tmp_path) not in result.output


def test_sandbox_manager_emits_unified_trace_when_trace_recorder_is_used(tmp_path: Path) -> None:
    trace = TraceRecorder.create(tmp_path, run_id="run_sandbox", session_id="session")
    component = SandboxManager(tmp_path, backends=[], trace=trace)

    result = component.run(_sandbox_request(tmp_path))

    assert result.status.value == "backend_unavailable"
    values = _event_values(trace)
    assert TraceEventType.SANDBOX_REQUESTED.value in values
    assert TraceEventType.SANDBOX_COMPLETED.value in values


def test_verification_runner_emits_check_evidence_and_repair_trace(tmp_path: Path) -> None:
    request = CommandRequest(argv=[sys.executable, "-m", "pytest"])
    fake = _FakeCommandExecutor(
        [
            _command_result(
                request,
                command_id="cmd_syntax",
                exit_code=0,
                semantic_status=SemanticStatus.SUCCEEDED,
                output="syntax ok",
                error_code=None,
            ),
            _command_result(
                request,
                command_id="cmd_fail",
                exit_code=1,
                semantic_status=SemanticStatus.TESTS_FAILED,
                output="FAILED tests/test_app.py::test_bad - AssertionError: nope",
                error_code="semantic_failure",
            ),
            _command_result(
                request,
                command_id="cmd_fail_2",
                exit_code=1,
                semantic_status=SemanticStatus.TESTS_FAILED,
                output="FAILED tests/test_app.py::test_bad - AssertionError: nope",
                error_code="semantic_failure",
            ),
        ]
    )
    (tmp_path / "package.json").write_text(
        json.dumps({"scripts": {"test": "python -m pytest"}}),
        encoding="utf-8",
    )
    trace = TraceRecorder.create(tmp_path, run_id="run_verification", session_id="session")
    component = VerificationRunner(tmp_path, command_executor=fake, trace=trace)

    plan = component.plan_verification(changed_files=["src/app.py"], task_intent="code")
    component.run_plan(plan.id)

    values = _event_values(trace)
    assert TraceEventType.VERIFICATION_PLAN_CREATED.value in values
    assert TraceEventType.VERIFICATION_CHECK_STARTED.value in values
    assert TraceEventType.VERIFICATION_EVIDENCE_RECORDED.value in values
    assert TraceEventType.REPAIR_HINT_CREATED.value in values
    assert TraceEventType.VERIFICATION_FAILED.value in values


class _FakeCommandExecutor:
    def __init__(self, results: list[Any]) -> None:
        from singularity.command import CommandPolicy

        self.policy = CommandPolicy()
        self.results = results

    def run(self, request: CommandRequest, *, transaction_id: str | None = None) -> Any:
        return self.results.pop(0)


def _command_result(
    request: CommandRequest,
    *,
    command_id: str,
    exit_code: int | None,
    semantic_status: SemanticStatus,
    output: str,
    error_code: str | None,
) -> Any:
    from singularity.command import CommandResult

    return CommandResult(
        command_id=command_id,
        execution_status=ExecutionStatus.COMPLETED,
        semantic_status=semantic_status,
        exit_code=exit_code,
        signal=None,
        duration_ms=12,
        timed_out=False,
        idle_timed_out=False,
        stdout_preview=output,
        stderr_preview="",
        combined_output_preview=output,
        output_truncated=False,
        output_digest="digest",
        artifact_path=None,
        changed_files=[],
        policy_decision=CommandPolicyResult(
            decision=CommandDecision.ALLOW,
            reasons=["allowed"],
            risk_tags=[CommandRisk.PROJECT_VERIFICATION],
        ),
        risk_tags=[CommandRisk.PROJECT_VERIFICATION],
        error_code=error_code,
        isolation_report={},
        started_at="2026-01-01T00:00:00+00:00",
        ended_at="2026-01-01T00:00:00+00:00",
    )


def _sandbox_request(tmp_path: Path) -> SandboxRequest:
    profile = default_sandbox_profile(
        SandboxProfileName.ISOLATED_VERIFICATION,
        workspace_root=tmp_path,
    )
    profile.network.mode = SandboxNetworkMode.ALLOWED
    return SandboxRequest(
        sandbox_id="sandbox_manager",
        session_id="session",
        task_id="task",
        action_id="action",
        command=[sys.executable, "-c", "print('component')"],
        cwd=tmp_path,
        workspace_root=tmp_path,
        profile=profile,
    )
