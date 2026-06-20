from __future__ import annotations

import json
import sys
from pathlib import Path
from typing import Any

import pytest
from typer.testing import CliRunner

from miniharness.cli import app
from miniharness.command import CommandRequest, CommandRuntime
from miniharness.context.manager import ContextManager
from miniharness.observability.models import TraceEventType
from miniharness.observability.runtime import TraceRuntime
from miniharness.planner import PlannerRuntime, TaskStatus
from miniharness.planner.finalizer import Finalizer
from miniharness.policy import (
    Capability,
    OperationKind,
    PolicyConfig,
    PolicyRequest,
    PolicyRuntime,
    PolicySubject,
    ResourceRef,
    RuntimeName,
)
from miniharness.policy.approval import ApprovalGate
from miniharness.policy.config import ApprovalMode
from miniharness.policy import SecurityMode
from miniharness.policy.exceptions import ApprovalRequired
from miniharness.policy.models import DecisionOutcome
from miniharness.tools import ToolPolicy, ToolRegistry, ToolRuntime
from miniharness.workspace import CreateFile, MutationRuntime
from miniharness.command import (
    CommandPolicyResult,
    CommandDecision,
    CommandRisk,
    ExecutionStatus,
    SemanticStatus,
)
from miniharness.sandbox import (
    SandboxRuntime,
    SandboxProfileName,
    SandboxRequest,
    default_sandbox_profile,
)
from miniharness.verification import VerificationRuntime
from tests.tool_runtime_helpers import make_test_policy_runtime


def _compat_policy_runtime(tmp_path: Path) -> PolicyRuntime:
    return PolicyRuntime(
        PolicyConfig(workspace_root=tmp_path, security_mode=SecurityMode.COMPAT)
    )


def _event_values(trace: TraceRuntime) -> list[str]:
    return [event.event_type.value for event in trace.store.query_events()]


def test_tool_runtime_dispatch_emits_structured_trace(tmp_path: Path) -> None:
    trace = TraceRuntime.create(tmp_path, run_id="run_tool", session_id="session_tool")
    runtime = ToolRuntime(
        registry=ToolRegistry(tmp_path),
        policy=ToolPolicy.read_only(),
        trace=trace,
        workspace_root=tmp_path,
        policy_runtime=make_test_policy_runtime(tmp_path),
    )

    result = runtime.execute_tool_call(
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


def test_policy_runtime_and_approval_gate_emit_trace(tmp_path: Path) -> None:
    trace = TraceRuntime.create(tmp_path, run_id="run_policy", session_id="session_policy")
    config = PolicyConfig(
        workspace_root=tmp_path,
        approval_mode=ApprovalMode.NON_INTERACTIVE,
        audit_log_path=tmp_path / "audit.jsonl",
    )
    policy = PolicyRuntime(config, trace=trace)
    request = PolicyRequest(
        session_id="session_policy",
        task_id="task_policy",
        phase_id="phase",
        action_id="action",
        runtime=RuntimeName.TOOL,
        operation=OperationKind.DELETE_FILE,
        capability=Capability.DELETE_FILE,
        subject=PolicySubject(subject_type="runtime", name="test"),
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
    trace = TraceRuntime.create(tmp_path, run_id="run_all", session_id="session_all")
    planner = PlannerRuntime(tmp_path, session_id="session_all", task_id="task_all", trace=trace)
    planner.start_task("Add a file and verify it")
    planner.state.status = TaskStatus.APPLYING_CHANGES
    planner.state.current_phase = "applying_changes"
    planner.plan.current_phase = "applying_changes"

    mutation = MutationRuntime(tmp_path, trace=trace, planner=planner)
    mutation_result = mutation.apply_operations(
        [CreateFile(path="app.py", content="print('ok')\n")],
        intent="add app",
        created_by="test",
        tool_call_id="call_mutation",
    )
    command = CommandRuntime(
        tmp_path,
        trace=trace,
        planner=planner,
        policy_runtime=_compat_policy_runtime(tmp_path),
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
    trace = TraceRuntime.create(tmp_path, run_id="run_cli", session_id="session_cli")
    trace.emit(
        TraceEventType.COMMAND_COMPLETED,
        runtime="command",
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


def test_sandbox_runtime_emits_unified_trace_when_trace_runtime_is_used(tmp_path: Path) -> None:
    trace = TraceRuntime.create(tmp_path, run_id="run_sandbox", session_id="session")
    runtime = SandboxRuntime(tmp_path, trace=trace)

    result = runtime.run(_sandbox_request(tmp_path))

    assert result.status.value == "success"
    values = _event_values(trace)
    assert TraceEventType.SANDBOX_REQUESTED.value in values
    assert TraceEventType.SANDBOX_PREPARED.value in values
    assert TraceEventType.SANDBOX_STARTED.value in values
    assert TraceEventType.SANDBOX_COMPLETED.value in values
    assert TraceEventType.SANDBOX_CLEANED.value in values


def test_verification_runtime_emits_check_evidence_and_repair_trace(tmp_path: Path) -> None:
    request = CommandRequest(argv=[sys.executable, "-m", "pytest"])
    fake = _FakeCommandRuntime(
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
    trace = TraceRuntime.create(tmp_path, run_id="run_verification", session_id="session")
    runtime = VerificationRuntime(tmp_path, command_runtime=fake, trace=trace)

    plan = runtime.plan_verification(changed_files=["src/app.py"], task_intent="code")
    runtime.run_plan(plan.id)

    values = _event_values(trace)
    assert TraceEventType.VERIFICATION_PLAN_CREATED.value in values
    assert TraceEventType.VERIFICATION_CHECK_STARTED.value in values
    assert TraceEventType.VERIFICATION_EVIDENCE_RECORDED.value in values
    assert TraceEventType.REPAIR_HINT_CREATED.value in values
    assert TraceEventType.VERIFICATION_FAILED.value in values


class _FakeCommandRuntime:
    def __init__(self, results: list[Any]) -> None:
        from miniharness.command import CommandPolicy

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
    from miniharness.command import CommandResult

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
    return SandboxRequest(
        sandbox_id="sandbox_runtime",
        session_id="session",
        task_id="task",
        action_id="action",
        command=[sys.executable, "-c", "print('runtime')"],
        cwd=tmp_path,
        workspace_root=tmp_path,
        profile=default_sandbox_profile(
            SandboxProfileName.ISOLATED_VERIFICATION,
            workspace_root=tmp_path,
        ),
    )
