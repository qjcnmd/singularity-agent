import json
import sys
import time
from pathlib import Path

from miniharness.command import (
    CommandDecision,
    CommandPlan,
    CommandPolicy,
    CommandPurpose,
    CommandRequest,
    CommandRisk,
    CommandRuntime,
    ExecutionStatus,
    FilesystemMode,
    NetworkMode,
    ResourceLimits,
    SemanticStatus,
)
from miniharness.context import ContextManager
from miniharness.tools import ToolPolicy, ToolRegistry, ToolRuntime
from miniharness.tools.command import register_command_tools
from miniharness.tools.models import ToolResult
from miniharness.trace import TraceWriter


class SimpleTokenCounter:
    def count_text(self, text: str) -> int:
        return len(text.split())

    def count_messages(self, messages: list[dict]) -> int:
        return sum(self.count_text(str(message.get("content") or "")) for message in messages)


def tool_call(name: str, arguments: dict, *, tool_call_id: str = "call_command") -> dict:
    return {
        "id": tool_call_id,
        "type": "function",
        "function": {"name": name, "arguments": json.dumps(arguments)},
    }


def test_argv_command_executes_and_returns_structured_result(tmp_path: Path) -> None:
    runtime = CommandRuntime(tmp_path)

    result = runtime.run(
        CommandRequest(
            argv=[sys.executable, "-c", "print('hello command')"],
            cwd=".",
            purpose=CommandPurpose.READ_ONLY_COMMAND,
        )
    )

    assert result.command_id
    assert result.execution_status == ExecutionStatus.COMPLETED
    assert result.semantic_status == SemanticStatus.SUCCEEDED
    assert result.exit_code == 0
    assert result.stdout_preview.strip() == "hello command"
    assert result.stderr_preview == ""
    assert result.policy_decision.decision == CommandDecision.ALLOW
    assert result.output_digest


def test_command_runtime_can_build_command_plan(tmp_path: Path) -> None:
    runtime = CommandRuntime(tmp_path)
    request = CommandRequest(
        argv=[sys.executable, "-c", "print('plan')"],
        cwd=".",
        purpose=CommandPurpose.READ_ONLY_COMMAND,
    )

    plan = runtime.plan(request)

    assert isinstance(plan, CommandPlan)
    assert plan.request.command_id == request.command_id
    assert plan.cwd == "."
    assert plan.policy_decision.decision == CommandDecision.ALLOW
    assert plan.backend == "local_process"


def test_shell_string_is_marked_high_risk_and_requires_review(tmp_path: Path) -> None:
    runtime = CommandRuntime(tmp_path)

    result = runtime.run(
        CommandRequest(
            shell="echo shell",
            cwd=".",
            purpose=CommandPurpose.UNKNOWN,
        )
    )

    assert result.execution_status == ExecutionStatus.REVIEW_REQUIRED
    assert result.error_code == "review_required"
    assert result.policy_decision.decision == CommandDecision.REQUIRE_REVIEW
    assert CommandRisk.UNKNOWN in result.risk_tags


def test_cwd_outside_workspace_is_rejected(tmp_path: Path) -> None:
    runtime = CommandRuntime(tmp_path)

    result = runtime.run(
        CommandRequest(
            argv=[sys.executable, "-c", "print('nope')"],
            cwd="../outside",
            purpose=CommandPurpose.READ_ONLY_COMMAND,
        )
    )

    assert result.execution_status == ExecutionStatus.POLICY_DENIED
    assert result.error_code == "cwd_outside_workspace"
    assert result.exit_code is None


def test_env_secret_is_not_passed_and_output_is_redacted(tmp_path: Path) -> None:
    runtime = CommandRuntime(tmp_path)
    code = (
        "import os; "
        "print(os.getenv('OPENAI_API_KEY', 'missing')); "
        "print(os.getenv('VISIBLE_VALUE', 'missing'))"
    )

    result = runtime.run(
        CommandRequest(
            argv=[sys.executable, "-c", code],
            cwd=".",
            purpose=CommandPurpose.READ_ONLY_COMMAND,
            env_request={
                "OPENAI_API_KEY": "sk-test-secret",
                "VISIBLE_VALUE": "visible-ok",
            },
        )
    )

    assert result.execution_status == ExecutionStatus.COMPLETED
    assert result.env_denied == ["OPENAI_API_KEY"]
    assert "sk-test-secret" not in result.stdout_preview
    assert "missing" in result.stdout_preview
    assert "visible-ok" in result.stdout_preview


def test_stdout_stderr_are_collected_separately(tmp_path: Path) -> None:
    runtime = CommandRuntime(tmp_path)
    code = (
        "import sys, time; "
        "print('out', flush=True); "
        "time.sleep(0.05); "
        "print('err', file=sys.stderr, flush=True)"
    )

    result = runtime.run(
        CommandRequest(
            argv=[sys.executable, "-c", code],
            cwd=".",
            purpose=CommandPurpose.READ_ONLY_COMMAND,
        )
    )

    assert result.stdout_preview.strip() == "out"
    assert result.stderr_preview.strip() == "err"
    assert result.combined_output_preview.index("out") < result.combined_output_preview.index("err")


def test_large_output_is_truncated_and_saved_as_artifact(tmp_path: Path) -> None:
    runtime = CommandRuntime(tmp_path)

    result = runtime.run(
        CommandRequest(
            argv=[sys.executable, "-c", "import sys; sys.stdout.write('A' * 200)"],
            cwd=".",
            purpose=CommandPurpose.READ_ONLY_COMMAND,
            resource_limits=ResourceLimits(
                max_stdout_bytes=40,
                max_combined_output_bytes=80,
            ),
        )
    )

    assert result.output_truncated is True
    assert len(result.stdout_preview) <= 40
    assert result.artifact_path is not None
    assert (tmp_path / result.artifact_path).exists()
    assert result.error_code == "output_limit_exceeded"


def test_timeout_marks_result_and_does_not_become_internal_error(tmp_path: Path) -> None:
    runtime = CommandRuntime(tmp_path)

    result = runtime.run(
        CommandRequest(
            argv=[sys.executable, "-c", "import time; time.sleep(5)"],
            cwd=".",
            purpose=CommandPurpose.READ_ONLY_COMMAND,
            timeout_seconds=0.2,
        )
    )

    assert result.execution_status == ExecutionStatus.TIMED_OUT
    assert result.timed_out is True
    assert result.error_code == "timeout"
    assert result.killed_reason == "timeout"


def test_idle_timeout_marks_result(tmp_path: Path) -> None:
    runtime = CommandRuntime(tmp_path)

    result = runtime.run(
        CommandRequest(
            argv=[sys.executable, "-c", "import time; time.sleep(5)"],
            cwd=".",
            purpose=CommandPurpose.READ_ONLY_COMMAND,
            timeout_seconds=5,
            idle_timeout_seconds=0.2,
        )
    )

    assert result.execution_status == ExecutionStatus.IDLE_TIMED_OUT
    assert result.idle_timed_out is True
    assert result.error_code == "idle_timeout"
    assert result.killed_reason == "idle_timeout"


def test_nonzero_exit_is_not_an_internal_error(tmp_path: Path) -> None:
    runtime = CommandRuntime(tmp_path)

    result = runtime.run(
        CommandRequest(
            argv=[sys.executable, "-c", "import sys; sys.exit(7)"],
            cwd=".",
            purpose=CommandPurpose.READ_ONLY_COMMAND,
        )
    )

    assert result.execution_status == ExecutionStatus.COMPLETED
    assert result.exit_code == 7
    assert result.semantic_status == SemanticStatus.EXIT_NONZERO
    assert result.error_code == "exit_nonzero"


def test_project_verification_nonzero_is_semantic_test_failure(tmp_path: Path) -> None:
    runtime = CommandRuntime(tmp_path)

    result = runtime.run(
        CommandRequest(
            argv=[sys.executable, "-c", "import sys; sys.exit(1)"],
            cwd=".",
            purpose=CommandPurpose.PROJECT_VERIFICATION,
        )
    )

    assert result.execution_status == ExecutionStatus.COMPLETED
    assert result.semantic_status == SemanticStatus.TESTS_FAILED
    assert result.error_code == "semantic_failure"


def test_command_policy_classifies_pytest_and_package_manager_commands(tmp_path: Path) -> None:
    policy = CommandPolicy()

    pytest_decision = policy.evaluate(
        CommandRequest(
            argv=["pytest", "tests"],
            cwd=".",
            purpose=CommandPurpose.PROJECT_VERIFICATION,
        ),
        workspace_root=tmp_path,
    )
    package_decision = policy.evaluate(
        CommandRequest(
            argv=["npm", "install"],
            cwd=".",
            purpose=CommandPurpose.PACKAGE_MANAGER,
            network_mode=NetworkMode.ALLOW_PACKAGE_REGISTRIES,
            filesystem_mode=FilesystemMode.READ_WRITE_WORKSPACE,
        ),
        workspace_root=tmp_path,
    )

    assert CommandRisk.EXECUTES_PROJECT_CODE in pytest_decision.risk_tags
    assert CommandRisk.PACKAGE_MANAGER in package_decision.risk_tags
    assert CommandRisk.NETWORK in package_decision.risk_tags
    assert CommandRisk.WRITE_WORKSPACE in package_decision.risk_tags
    assert package_decision.decision == CommandDecision.REQUIRE_REVIEW


def test_destructive_command_is_denied(tmp_path: Path) -> None:
    decision = CommandPolicy().evaluate(
        CommandRequest(
            argv=["rm", "-rf", "."],
            cwd=".",
            purpose=CommandPurpose.DESTRUCTIVE,
        ),
        workspace_root=tmp_path,
    )

    assert decision.decision == CommandDecision.DENY
    assert CommandRisk.DESTRUCTIVE in decision.risk_tags
    assert decision.error_code == "policy_denied"


def test_long_running_process_can_start_read_stop_and_list(tmp_path: Path) -> None:
    runtime = CommandRuntime(tmp_path)
    request = CommandRequest(
        argv=[
            sys.executable,
            "-c",
            "import sys, time; print('ready'); sys.stdout.flush(); time.sleep(20)",
        ],
        cwd=".",
        purpose=CommandPurpose.LONG_RUNNING,
        risk_acceptance_reason="test owns this long-running process",
    )

    session = runtime.start_process(request)
    try:
        output = ""
        deadline = time.time() + 3
        while time.time() < deadline and "ready" not in output:
            time.sleep(0.05)
            output = runtime.read_process_output(session.process_id).combined_output

        listed = runtime.list_processes()

        assert session.process_id
        assert session.status == "running"
        assert "ready" in output
        assert any(item.process_id == session.process_id for item in listed)
    finally:
        stopped = runtime.stop_process(session.process_id)

    assert stopped.status == "stopped"
    assert stopped.exit_code is not None


def test_workspace_side_effects_are_tracked(tmp_path: Path) -> None:
    trace = TraceWriter.create(tmp_path)
    runtime = CommandRuntime(tmp_path, trace=trace)

    result = runtime.run(
        CommandRequest(
            argv=[
                sys.executable,
                "-c",
                "from pathlib import Path; Path('generated.txt').write_text('new', encoding='utf-8')",
            ],
            cwd=".",
            purpose=CommandPurpose.WRITE_WORKSPACE,
            filesystem_mode=FilesystemMode.READ_WRITE_WORKSPACE,
            risk_acceptance_reason="test writes a known generated file",
        )
    )

    assert result.changed_files == ["generated.txt"]
    events = [json.loads(line) for line in trace.path.read_text(encoding="utf-8").splitlines()]
    command_events = [event for event in events if event["event"] == "command"]
    assert command_events[-1]["data"]["changed_files"] == ["generated.txt"]


def test_command_result_observation_is_compact_for_context_manager(tmp_path: Path) -> None:
    runtime = CommandRuntime(tmp_path)
    result = runtime.run(
        CommandRequest(
            argv=[sys.executable, "-c", "print('X' * 5000)"],
            cwd=".",
            purpose=CommandPurpose.READ_ONLY_COMMAND,
            resource_limits=ResourceLimits(max_stdout_bytes=200),
        )
    )
    context = ContextManager(
        system_prompt="system",
        user_goal="run command",
        token_counter=SimpleTokenCounter(),
    )

    observation = context.add_tool_result(
        tool_call=tool_call("run_command", {"argv": [sys.executable]}),
        result=ToolResult.success(content=result.to_observation()).model_dump(mode="json"),
    )

    assert observation.ok is True
    assert observation.raw_result["content"]["command_result"]["command_id"] == result.command_id
    assert "X" * 1000 not in observation.preview
    assert "command_result" in observation.preview


def test_command_trace_records_full_audit_event(tmp_path: Path) -> None:
    trace = TraceWriter.create(tmp_path)
    runtime = CommandRuntime(tmp_path, trace=trace)

    result = runtime.run(
        CommandRequest(
            argv=[sys.executable, "-c", "print('audit')"],
            cwd=".",
            purpose=CommandPurpose.READ_ONLY_COMMAND,
        ),
        tool_call_id="call_audit",
        transaction_id="tx_audit",
    )

    events = [json.loads(line) for line in trace.path.read_text(encoding="utf-8").splitlines()]
    command_events = [event for event in events if event["event"] == "command"]
    audit = command_events[-1]["data"]
    assert audit["command_id"] == result.command_id
    assert audit["tool_call_id"] == "call_audit"
    assert audit["transaction_id"] == "tx_audit"
    assert audit["argv"] == [sys.executable, "-c", "print('audit')"]
    assert audit["backend"] == "local_process"
    assert audit["policy_decision"] == "allow"
    assert audit["network_mode"] == "DISABLED"
    assert audit["filesystem_mode"] == "READ_ONLY_WORKSPACE"
    assert audit["resource_limits"]["timeout_seconds"] == 30.0
    assert audit["exit_code"] == 0
    assert audit["output_digest"]
    assert audit["semantic_status"] == "succeeded"
    assert audit["isolation_report"]["network_isolation_enforced"] is False
    assert "git_before" in audit
    assert "git_after" in audit


def test_run_command_tool_is_registered_and_uses_command_runtime(tmp_path: Path) -> None:
    registry = ToolRegistry(tmp_path, include_default_tools=False)
    register_command_tools(registry, CommandRuntime(tmp_path))
    runtime = ToolRuntime(
        registry=registry,
        policy=ToolPolicy.coding_agent(),
        trace=None,
        workspace_root=tmp_path,
    )

    result = runtime.execute_tool_call(
        tool_call(
            "run_command",
            {
                "argv": [sys.executable, "-c", "print('tool command')"],
                "cwd": ".",
                "purpose": "READ_ONLY_COMMAND",
            },
        )
    )

    assert result.ok is True
    assert result.content["command_result"]["status"] == "completed"
    assert "tool command" in result.content["command_result"]["key_output"]
