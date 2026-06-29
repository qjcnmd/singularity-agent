import json
import queue
import sys
import time
from pathlib import Path

import pytest

import singularity.sandbox as sandbox
from singularity.command import (
    CommandDecision,
    CommandExecutor,
    CommandPlan,
    CommandPolicy,
    CommandPurpose,
    CommandRequest,
    CommandRisk,
    ExecutionStatus,
    FilesystemMode,
    NetworkMode,
    ResourceLimits,
    SemanticStatus,
)
from singularity.command.backend import RunningProcess, _reader_thread
from singularity.context import ContextManager
from singularity.jsonl_trace import JsonlTraceRecorder
from singularity.policy import DecisionOutcome, PolicyConfig, PolicyEngine
from singularity.policy.permissions import PermissionProfile, PermissionProfileName
from singularity.sandbox import SandboxManager
from singularity.tools import ToolExecutor, ToolPolicy, ToolRegistry
from singularity.tools.command import register_command_tools
from singularity.tools.models import ToolResult
from tests.test_tool_executor_policy_approval import SequencedPolicyEngine
from tests.tool_executor_helpers import default_policy_engine


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


def unrestricted_command_executor(tmp_path: Path, **kwargs) -> CommandExecutor:
    return CommandExecutor(
        tmp_path,
        policy_engine=PolicyEngine(
            PolicyConfig(
                workspace_root=tmp_path,
                permission_profile=PermissionProfile.default_for_workspace(
                    tmp_path,
                    profile=PermissionProfileName.DANGER_FULL_ACCESS,
                ),
            )
        ),
        **kwargs,
    )


def test_command_reader_reads_output_in_chunks() -> None:
    class Pipe:
        def __init__(self, data: bytes) -> None:
            self.data = data
            self.read_sizes: list[int] = []

        def read(self, size: int) -> bytes:
            self.read_sizes.append(size)
            if not self.data:
                return b""
            chunk = self.data[:size]
            self.data = self.data[size:]
            return chunk

        def read1(self, size: int) -> bytes:
            return self.read(size)

    pipe = Pipe(b"x" * 9000)
    output_queue: queue.Queue[tuple[str, bytes]] = queue.Queue()

    thread = _reader_thread("stdout", pipe, output_queue)
    thread.join(timeout=2)

    assert not thread.is_alive()
    assert max(pipe.read_sizes) > 1
    chunks: list[bytes] = []
    while not output_queue.empty():
        stream, chunk = output_queue.get_nowait()
        assert stream == "stdout"
        chunks.append(chunk)
    assert b"".join(chunks) == b"x" * 9000


def test_strict_mode_blocks_inline_interpreter_readonly_command(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # Force the sandbox backend list to be empty so this executor-level test
    # deterministically exercises the no-local-fallback path regardless of
    # whether the host has a configured (available) Windows sandbox.
    monkeypatch.setattr("singularity.sandbox.manager.default_sandbox_backends", lambda: [])
    component = CommandExecutor(tmp_path)

    result = component.run(
        CommandRequest(
            argv=[
                sys.executable,
                "-c",
                "from pathlib import Path; Path('x').write_text('bad', encoding='utf-8')",
            ],
            cwd=".",
            purpose=CommandPurpose.READ_ONLY_COMMAND,
            filesystem_mode=FilesystemMode.READ_WRITE_WORKSPACE,
        )
    )

    assert result.execution_status == ExecutionStatus.BACKEND_ERROR
    assert result.error_code == "sandbox_unavailable"
    assert result.backend != "local_process"
    assert not (tmp_path / "x").exists()


def test_workspace_write_command_requires_sandbox_instead_of_local_process(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # Deterministic no-local-fallback check: empty backend list means the
    # sandbox is unavailable regardless of host configuration.
    monkeypatch.setattr("singularity.sandbox.manager.default_sandbox_backends", lambda: [])
    component = CommandExecutor(
        tmp_path,
        policy_engine=PolicyEngine(
            PolicyConfig(workspace_root=tmp_path)
        ),
    )

    result = component.run(
        CommandRequest(
            argv=[sys.executable, "-c", "print('strict')"],
            cwd=".",
            purpose=CommandPurpose.READ_ONLY_COMMAND,
        )
    )

    assert result.backend != "local_process"
    assert result.error_code in {"sandbox_unavailable", None}
    assert result.isolation_report["filesystem_isolation"] != "workspace_cwd_advisory"


def test_workspace_write_low_risk_verification_runs_through_windows_sandbox(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    monkeypatch.setattr(
        "singularity.sandbox.windows._windows_state_dir_path",
        lambda: tmp_path / "state" / "windows-sandbox",
    )
    class FakeRunner:
        def __init__(self) -> None:
            self.calls = []

        def run(self, prepared):
            self.calls.append(prepared)
            now = "2026-01-01T00:00:00+00:00"
            return sandbox.WindowsRunnerResult(
                exit_code=0,
                stdout="pytest passed\n",
                stderr="",
                timed_out=False,
                started_at=now,
                ended_at=now,
                duration_ms=3,
                network_denied_verified=True,
                metadata={
                    "restricted_token": True,
                    "low_integrity": True,
                    "private_desktop": True,
                    "job_object": True,
                },
            )

    runner = FakeRunner()
    backend = sandbox.WindowsSandboxBackend(
        runner=runner,
        acl_applier=lambda _path, _account: None,
        doctor_provider=sandbox.WindowsSandboxDoctorReport.ready_for_tests,
    )
    profile = PermissionProfile.default_for_workspace(
        tmp_path,
        profile=PermissionProfileName.WORKSPACE_WRITE,
    )
    component = CommandExecutor(
        tmp_path,
        policy_engine=PolicyEngine(
            PolicyConfig(workspace_root=tmp_path, permission_profile=profile)
        ),
        sandbox_manager=SandboxManager(
            tmp_path,
            backends=[backend],
            permission_profile=profile,
        ),
    )

    result = component.run(
        CommandRequest(
            argv=[sys.executable, "-m", "pytest", "-q"],
            cwd=".",
            purpose=CommandPurpose.PROJECT_VERIFICATION,
        )
    )

    assert result.execution_status == ExecutionStatus.COMPLETED
    assert result.backend == "windows"
    assert result.error_code is None
    assert result.stdout_preview == "pytest passed\n"
    assert result.isolation_report["backend"] == "windows"
    sandbox_report = result.isolation_report["sandbox"]
    assert len(runner.calls) == 1
    prepared = runner.calls[0]
    assert prepared.backend_name == "windows"
    assert prepared.request.command == [sys.executable, "-m", "pytest", "-q"]
    assert prepared.baseline["runner_spec"]
    assert prepared.baseline["runner_result"]
    assert sandbox_report["backend_is_local_process"] is False
    assert sandbox_report["network_denied_verified"] is True
    assert sandbox_report["execution_backend"] == "account_restricted_token"
    assert result.metadata["network_denied_verified"] is True


def test_danger_full_access_allows_inline_interpreter_execution(tmp_path: Path) -> None:
    component = unrestricted_command_executor(tmp_path)

    result = component.run(
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


def test_danger_full_access_still_requires_review_for_workspace_write(tmp_path: Path) -> None:
    component = unrestricted_command_executor(tmp_path)

    result = component.run(
        CommandRequest(
            argv=[
                sys.executable,
                "-c",
                "from pathlib import Path; Path('x').write_text('bad', encoding='utf-8')",
            ],
            cwd=".",
            purpose=CommandPurpose.READ_ONLY_COMMAND,
            filesystem_mode=FilesystemMode.READ_WRITE_WORKSPACE,
        )
    )

    assert result.execution_status == ExecutionStatus.REVIEW_REQUIRED
    assert result.error_code == "review_required"
    assert not (tmp_path / "x").exists()


def test_command_executor_can_build_command_plan(tmp_path: Path) -> None:
    component = unrestricted_command_executor(tmp_path)
    request = CommandRequest(
        argv=[sys.executable, "-c", "print('plan')"],
        cwd=".",
        purpose=CommandPurpose.READ_ONLY_COMMAND,
    )

    plan = component.plan(request)

    assert isinstance(plan, CommandPlan)
    assert plan.request.command_id == request.command_id
    assert plan.cwd == "."
    assert plan.policy_decision.decision == CommandDecision.ALLOW
    assert plan.backend == "local_process"


def test_shell_string_is_marked_high_risk_and_requires_review(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # Deterministic no-local-fallback check: empty backend list means the
    # sandbox is unavailable regardless of host configuration.
    monkeypatch.setattr("singularity.sandbox.manager.default_sandbox_backends", lambda: [])
    component = CommandExecutor(tmp_path)

    result = component.run(
        CommandRequest(
            shell="echo shell",
            cwd=".",
            purpose=CommandPurpose.UNKNOWN,
        )
    )

    assert result.execution_status == ExecutionStatus.BACKEND_ERROR
    assert result.error_code == "sandbox_unavailable"
    assert result.backend != "local_process"
    assert CommandRisk.UNKNOWN in result.risk_tags


def test_cwd_outside_workspace_is_rejected(tmp_path: Path) -> None:
    component = CommandExecutor(tmp_path)

    result = component.run(
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
    component = unrestricted_command_executor(tmp_path)
    code = (
        "import os; "
        "print(os.getenv('OPENAI_API_KEY', 'missing')); "
        "print(os.getenv('READ_REPLICA_DSN', 'missing')); "
        "print(os.getenv('APP_CONN_STR', 'missing')); "
        "print(os.getenv('VISIBLE_VALUE', 'missing')); "
        "print('DSN=postgres://user:pass@localhost/db'); "
        "print('CONNECTION_STRING=Server=.;Password=pw')"
    )

    result = component.run(
        CommandRequest(
            argv=[sys.executable, "-c", code],
            cwd=".",
            purpose=CommandPurpose.READ_ONLY_COMMAND,
            env_request={
                "OPENAI_API_KEY": "sk-test-secret",
                "DATABASE_URL": "postgres://user:pass@localhost/main",
                "READ_REPLICA_DSN": "postgres://user:pass@localhost/replica",
                "APP_CONN_STR": "Server=.;Password=secret",
                "VISIBLE_VALUE": "visible-ok",
            },
        )
    )

    assert result.execution_status == ExecutionStatus.COMPLETED
    assert result.env_denied == [
        "APP_CONN_STR",
        "DATABASE_URL",
        "OPENAI_API_KEY",
        "READ_REPLICA_DSN",
    ]
    assert "sk-test-secret" not in result.stdout_preview
    assert "postgres://user:pass" not in result.stdout_preview
    assert "Password=pw" not in result.stdout_preview
    assert "DSN=[REDACTED]" in result.stdout_preview
    assert "CONNECTION_STRING=[REDACTED]" in result.stdout_preview
    assert "missing" in result.stdout_preview
    assert "visible-ok" in result.stdout_preview


def test_stdout_stderr_are_collected_separately(tmp_path: Path) -> None:
    component = unrestricted_command_executor(tmp_path)
    code = (
        "import sys, time; "
        "print('out', flush=True); "
        "time.sleep(0.05); "
        "print('err', file=sys.stderr, flush=True)"
    )

    result = component.run(
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
    component = unrestricted_command_executor(tmp_path)

    result = component.run(
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
    observation = result.to_observation()["command_result"]
    assert observation["artifact_ref"] == result.artifact_path
    assert "artifact_path" not in observation
    assert str(tmp_path) not in str(observation)
    assert result.error_code == "output_limit_exceeded"


def test_timeout_marks_result_and_does_not_become_internal_error(tmp_path: Path) -> None:
    component = unrestricted_command_executor(tmp_path)

    result = component.run(
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
    component = unrestricted_command_executor(tmp_path)

    result = component.run(
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
    component = unrestricted_command_executor(tmp_path)

    result = component.run(
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
    component = unrestricted_command_executor(tmp_path)

    result = component.run(
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

    assert decision.decision == CommandDecision.REQUIRE_REVIEW
    assert CommandRisk.DESTRUCTIVE in decision.risk_tags
    assert decision.error_code == "review_required"


def test_long_running_process_can_start_read_stop_and_list(tmp_path: Path) -> None:
    component = unrestricted_command_executor(tmp_path)
    secret = "process-secret-value"
    request = CommandRequest(
        argv=[
            sys.executable,
            "-c",
            "import sys, time; print('ready'); sys.stdout.flush(); time.sleep(20)",
            "--token",
            secret,
        ],
        cwd=".",
        purpose=CommandPurpose.LONG_RUNNING,
        risk_acceptance_reason="test owns this long-running process",
    )

    session = component.start_process(request)
    try:
        output = ""
        deadline = time.time() + 3
        while time.time() < deadline and "ready" not in output:
            time.sleep(0.05)
            output = component.read_process_output(session.process_id).combined_output

        listed = component.list_processes()
        session_payload = session.to_dict()
        listed_payload = [item.to_dict() for item in listed]

        assert session.process_id
        assert session.status == "running"
        assert "ready" in output
        assert any(item.process_id == session.process_id for item in listed)
        assert secret not in json.dumps(session_payload)
        assert secret not in json.dumps(listed_payload)
        assert session_payload["argv"][-1] == "<redacted>"
    finally:
        stopped = component.stop_process(session.process_id)

    assert stopped.status == "stopped"
    assert stopped.exit_code is not None


def test_start_process_tracks_files_written_immediately_after_spawn(tmp_path: Path) -> None:
    class ImmediateWriteBackend:
        name = "immediate_write"

        def start(self, *, request, cwd, env, collector, owner_transaction=None):
            _ = request, env, owner_transaction
            (cwd / "generated.txt").write_text("new", encoding="utf-8")
            return RunningProcess(
                process_id="process_1",
                process=None,
                request=request,
                cwd=cwd,
                collector=collector,
                reader_threads=[],
                output_queue=queue.Queue(),
                started_at_monotonic=time.perf_counter(),
            )

    component = unrestricted_command_executor(tmp_path, backend=ImmediateWriteBackend())
    request = CommandRequest(
        argv=[sys.executable, "-c", "pass"],
        cwd=".",
        purpose=CommandPurpose.LONG_RUNNING,
        filesystem_mode=FilesystemMode.READ_WRITE_WORKSPACE,
        risk_acceptance_reason="test writes a known generated file",
    )

    session = component.start_process(request)
    stopped = component.stop_process(session.process_id)

    assert stopped.changed_files == ["generated.txt"]


def test_workspace_side_effects_are_tracked(tmp_path: Path) -> None:
    trace = JsonlTraceRecorder.create(tmp_path)
    component = unrestricted_command_executor(tmp_path, trace=trace)

    result = component.run(
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
    component = unrestricted_command_executor(tmp_path)
    result = component.run(
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
    trace = JsonlTraceRecorder.create(tmp_path)
    component = unrestricted_command_executor(tmp_path, trace=trace)

    result = component.run(
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
    assert audit["command_preview"]
    assert audit["command_hash"]
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


def test_command_executor_does_not_silently_support_legacy_backend_execute_signature(
    tmp_path: Path,
) -> None:
    class LegacyExecuteBackend:
        name = "legacy_execute"

        def execute(self, *, request, cwd, env, collector):
            _ = request, cwd, env, collector
            raise AssertionError("legacy execute signature should not be called")

    component = unrestricted_command_executor(tmp_path, backend=LegacyExecuteBackend())

    with pytest.raises(TypeError, match="cancellation_token"):
        component.run(
            CommandRequest(
                argv=[sys.executable, "-c", "print('legacy')"],
                cwd=".",
                purpose=CommandPurpose.READ_ONLY_COMMAND,
            )
        )


def test_command_trace_redacts_sensitive_argv_and_url_query(tmp_path: Path) -> None:
    trace = JsonlTraceRecorder.create(tmp_path)
    component = unrestricted_command_executor(tmp_path, trace=trace)
    secret = "plain-secret-value"
    query_secret = "query-secret-value"

    result = component.run(
        CommandRequest(
            argv=[
                sys.executable,
                "-c",
                "pass",
                "--token",
                secret,
                f"https://example.test/callback?api_key={query_secret}",
            ],
            cwd=".",
            purpose=CommandPurpose.READ_ONLY_COMMAND,
        ),
        tool_call_id="call_secret_args",
    )

    assert result.exit_code == 0
    trace_text = trace.path.read_text(encoding="utf-8")
    assert secret not in trace_text
    assert query_secret not in trace_text
    events = [json.loads(line) for line in trace_text.splitlines()]
    audit = [event for event in events if event["event"] == "command"][-1]["data"]
    assert audit["argv"][4] == "<redacted>"
    assert "api_key=<redacted>" in audit["argv"][5]
    assert audit["command_hash"]


def test_command_result_records_safe_capability_summary(tmp_path: Path) -> None:
    component = unrestricted_command_executor(tmp_path)

    result = component.run(
        CommandRequest(
            argv=[sys.executable, "-c", "print('ok')"],
            cwd=".",
        )
    )

    assert result.metadata["command_capabilities"]["backend"] == "local_process"
    assert "available_backends" in result.metadata["sandbox_availability"]
    assert str(tmp_path) not in json.dumps(result.metadata, sort_keys=True)


def test_run_command_tool_is_registered_and_uses_command_executor(tmp_path: Path) -> None:
    registry = ToolRegistry(tmp_path, include_default_tools=False)
    register_command_tools(registry, unrestricted_command_executor(tmp_path))
    component = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.coding_agent(),
        trace=None,
        workspace_root=tmp_path,
        policy_engine=default_policy_engine(tmp_path),
    )

    result = component.execute_tool_call(
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


def test_start_process_rejects_verification_like_command(tmp_path: Path) -> None:
    registry = ToolRegistry(tmp_path, include_default_tools=False)
    register_command_tools(registry, unrestricted_command_executor(tmp_path))
    component = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.coding_agent(),
        trace=None,
        workspace_root=tmp_path,
        policy_engine=SequencedPolicyEngine([DecisionOutcome.ALLOW]),  # type: ignore[arg-type]
    )

    result = component.execute_tool_call(
        tool_call(
            "start_process",
            {
                "argv": [sys.executable, "-m", "pytest"],
                "cwd": ".",
                "purpose": "PROJECT_VERIFICATION",
                "risk_acceptance_reason": "long process owner",
            },
        )
    )

    assert result.ok is False
    assert result.error_code == "verification_runner_required"


def test_command_executor_start_process_rejects_verification_like_command(tmp_path: Path) -> None:
    component = unrestricted_command_executor(tmp_path)

    session = component.start_process(
        CommandRequest(
            argv=[sys.executable, "-m", "pytest"],
            cwd=".",
            purpose=CommandPurpose.PROJECT_VERIFICATION,
            risk_acceptance_reason="long process owner",
        )
    )

    assert session.status == "policy_denied"
    assert session.error_code == "verification_runner_required"
    assert session.pid is None
