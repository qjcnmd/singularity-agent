import json
import sys
from pathlib import Path

from singularity.command import (
    CommandDecision,
    CommandPolicy,
    CommandPolicyResult,
    CommandRequest,
    CommandRisk,
    ExecutionStatus,
    SemanticStatus,
)
from singularity.command.models import CommandResult
from singularity.context import ContextManager
from singularity.tools import ToolPolicy, ToolRegistry, ToolRuntime
from singularity.tools.command import register_command_tools
from singularity.tools.verification import register_verification_tools
from singularity.trace import TraceWriter
from singularity.verification import (
    CheckKind,
    CheckStatus,
    CommandDiscovery,
    CompletionAssessor,
    CompletionStatus,
    FailureParserRegistry,
    FailureType,
    ImpactAnalyzer,
    ProjectDetector,
    ProjectLanguage,
    RepairBudget,
    RepairLoopController,
    RepairLoopState,
    VerificationRuntime,
    WorkspaceKind,
)
from singularity.review import ReviewRuntime
from singularity.verification.models import DiscoveredCommand, VerificationCheck, VerificationPlan
from tests.tool_runtime_helpers import runtime_default_policy_runtime


def tool_call(name: str, arguments: dict, *, tool_call_id: str = "call_verify") -> dict:
    return {
        "id": tool_call_id,
        "type": "function",
        "function": {"name": name, "arguments": json.dumps(arguments)},
    }


class FakeCommandRuntime:
    def __init__(self, results: list[CommandResult]) -> None:
        self.policy = CommandPolicy()
        self.results = results
        self.calls: list[CommandRequest] = []

    def run(self, request: CommandRequest, *, transaction_id: str | None = None) -> CommandResult:
        self.calls.append(request)
        result = self.results.pop(0)
        return result


def command_result(
    request: CommandRequest,
    *,
    command_id: str,
    exit_code: int,
    semantic_status: SemanticStatus,
    output: str = "",
    error_code: str | None = None,
    execution_status: ExecutionStatus = ExecutionStatus.COMPLETED,
    timed_out: bool = False,
) -> CommandResult:
    return CommandResult(
        command_id=command_id,
        execution_status=execution_status,
        semantic_status=semantic_status,
        exit_code=exit_code,
        signal=None,
        duration_ms=12,
        timed_out=timed_out,
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
    )


def test_detects_node_typescript_package_scripts(tmp_path: Path) -> None:
    (tmp_path / "package.json").write_text(
        json.dumps(
            {
                "scripts": {
                    "test": "vitest run",
                    "lint": "eslint .",
                    "typecheck": "tsc --noEmit",
                    "build": "tsc -p tsconfig.json",
                },
                "devDependencies": {"typescript": "^5.0.0", "vitest": "^2.0.0"},
            }
        ),
        encoding="utf-8",
    )
    (tmp_path / "pnpm-lock.yaml").write_text("lockfileVersion: '9.0'\n", encoding="utf-8")
    (tmp_path / "tsconfig.json").write_text("{}", encoding="utf-8")

    profile = ProjectDetector(tmp_path).detect()

    assert profile.language == ProjectLanguage.TYPESCRIPT
    assert profile.package_manager == "pnpm"
    assert profile.workspace_kind == WorkspaceKind.SINGLE_PROJECT
    assert {command.kind for command in profile.available_commands} >= {
        CheckKind.UNIT_TEST,
        CheckKind.LINT,
        CheckKind.TYPECHECK,
        CheckKind.BUILD,
    }


def test_detects_python_pytest_from_pyproject(tmp_path: Path) -> None:
    (tmp_path / "pyproject.toml").write_text(
        """
[project]
name = "sample"
dependencies = []

[project.optional-dependencies]
dev = ["pytest>=8", "ruff>=0.5"]

[tool.pytest.ini_options]
testpaths = ["tests"]
""",
        encoding="utf-8",
    )
    (tmp_path / "tests").mkdir()

    commands = CommandDiscovery(tmp_path).discover()
    profile = ProjectDetector(tmp_path).detect()

    assert profile.language == ProjectLanguage.PYTHON
    assert "pytest" in profile.test_frameworks
    assert any(command.kind == CheckKind.UNIT_TEST for command in commands)
    assert any(command.kind == CheckKind.LINT for command in commands)


def test_verification_command_serialization_redacts_sensitive_arguments() -> None:
    request = CommandRequest(
        argv=[
            "curl",
            "https://example.test/check?api_key=sk-secret-url",
            "--token",
            "sk-secret-arg",
        ]
    )
    discovered = DiscoveredCommand(
        name="custom",
        kind=CheckKind.CUSTOM,
        request=request,
        source="test",
    )
    check = VerificationCheck(
        kind=CheckKind.CUSTOM,
        command=request,
        scope="workspace",
        required=True,
        timeout=30.0,
        risk_tags=["custom"],
        failure_policy="fail_fast",
    )

    for payload in (discovered.to_dict(), check.to_dict()):
        serialized = json.dumps(payload, sort_keys=True)
        assert "sk-secret" not in serialized
        assert "<redacted>" in serialized
        assert payload["command_hash"]
        assert payload["argv"][-1] == "<redacted>"


def test_impact_analysis_handles_docs_source_and_high_risk_files(tmp_path: Path) -> None:
    profile = ProjectDetector(tmp_path).detect()
    analyzer = ImpactAnalyzer()

    docs = analyzer.analyze(
        changed_files=["docs/guide.md"],
        task_intent="docs",
        project_profile=profile,
    )
    source = analyzer.analyze(
        changed_files=["src/singularity/app.py"],
        task_intent="code",
        project_profile=profile,
    )
    high_risk = analyzer.analyze(
        changed_files=[".github/workflows/ci.yml", "package-lock.json"],
        task_intent="ci",
        project_profile=profile,
    )

    assert docs.risk_level == "low"
    assert docs.requires_full_test is False
    assert source.requires_typecheck is True
    assert high_risk.requires_manual_review is True
    assert high_risk.requires_full_test is True


def test_verification_plan_separates_required_optional_skipped_and_blocked(tmp_path: Path) -> None:
    (tmp_path / "README.md").write_text("docs\n", encoding="utf-8")
    runtime = VerificationRuntime(tmp_path)

    docs_plan = runtime.plan_verification(
        changed_files=["README.md"],
        task_intent="docs",
    )
    source_plan = runtime.plan_verification(
        changed_files=["src/app.py"],
        task_intent="source",
    )

    assert any(check.kind == CheckKind.MANUAL_REVIEW for check in docs_plan.skipped_checks)
    assert any(check.kind == CheckKind.UNIT_TEST for check in source_plan.blocked_checks)
    assert source_plan.required_checks or source_plan.blocked_checks


def test_runtime_executes_checks_through_command_runtime_and_records_trace(tmp_path: Path) -> None:
    request = CommandRequest(argv=[sys.executable, "-c", "print('ok')"])
    fake = FakeCommandRuntime(
        [
            command_result(
                request,
                command_id="cmd_ok",
                exit_code=0,
                semantic_status=SemanticStatus.SUCCEEDED,
                output="ok",
            )
        ]
    )
    (tmp_path / "package.json").write_text(
        json.dumps({"scripts": {"test": "echo ok"}}),
        encoding="utf-8",
    )
    trace = TraceWriter.create(tmp_path)
    runtime = VerificationRuntime(tmp_path, command_runtime=fake, trace=trace)

    plan = runtime.plan_verification(changed_files=["src/app.js"], task_intent="code")
    observation = runtime.run_plan(plan.id)

    assert fake.calls
    assert observation["verification"]["check_status"]
    assert "cmd_ok" in trace.path.read_text(encoding="utf-8")
    assert observation["verification"]["completion_assessment"]["status"] in {
        CompletionStatus.READY.value,
        CompletionStatus.READY_WITH_WARNINGS.value,
        CompletionStatus.BLOCKED.value,
    }


def test_verification_evidence_records_safe_capability_summaries(tmp_path: Path) -> None:
    request = CommandRequest(argv=[sys.executable, "-c", "print('ok')"])
    result = command_result(
        request,
        command_id="cmd_ok",
        exit_code=0,
        semantic_status=SemanticStatus.SUCCEEDED,
        output="ok",
    )
    result.metadata.update(
        {
            "provider_capabilities": {
                "provider": "mock",
                "supports_streaming": False,
                "raw_payload": {"secret": "must-not-leak"},
            },
            "command_capabilities": {
                "backend": "local_process",
                "timeout": True,
                "raw_command": ["python", "-c", "print('ok')"],
            },
            "sandbox_availability": {
                "available_backends": ["local_staging"],
                "selected_backend": "local_staging",
                "hard_isolation_available": False,
                "absolute_path": str(tmp_path),
            },
        }
    )
    fake = FakeCommandRuntime([result])
    (tmp_path / "package.json").write_text(
        json.dumps({"scripts": {"test": "echo ok"}}),
        encoding="utf-8",
    )
    runtime = VerificationRuntime(tmp_path, command_runtime=fake)

    plan = runtime.plan_verification(changed_files=["src/app.js"], task_intent="code")
    observation = runtime.run_plan(plan.id)
    evidence = next(
        result["evidence"]
        for result in observation["verification"]["results"]
        if result["evidence"]["command_id"] == "cmd_ok"
    )

    assert "artifact_ref" in evidence
    assert evidence["capability_summary"] == {
        "provider": {"provider": "mock", "supports_streaming": False},
        "command": {"backend": "local_process", "timeout": True},
        "sandbox": {
            "available_backends": ["local_staging"],
            "selected_backend": "local_staging",
            "hard_isolation_available": False,
        },
    }
    serialized = json.dumps(evidence, sort_keys=True)
    assert "must-not-leak" not in serialized
    assert str(tmp_path) not in serialized


def test_failure_parsers_extract_pytest_tsc_and_eslint_failures() -> None:
    output = """
FAILED tests/test_app.py::test_thing - AssertionError: nope
tests/test_app.py:12: AssertionError
src/app.ts(3,7): error TS2322: Type 'string' is not assignable to type 'number'.
C:\\repo\\src\\ui.tsx
  5:3  error  Unexpected console statement  no-console
"""
    failures = FailureParserRegistry().parse(output)

    assert any(failure.test_name == "test_thing" for failure in failures)
    assert any(failure.symbol == "TS2322" and failure.line == 3 for failure in failures)
    assert any(failure.symbol == "no-console" and failure.line == 5 for failure in failures)


def test_command_failures_convert_to_semantic_verification_results(tmp_path: Path) -> None:
    request = CommandRequest(argv=["missing-test-tool"])
    fake = FakeCommandRuntime(
        [
            command_result(
                request,
                command_id="cmd_missing",
                exit_code=None,
                semantic_status=SemanticStatus.RUNTIME_FAILED,
                output="missing-test-tool not found",
                error_code="command_not_found",
                execution_status=ExecutionStatus.SPAWN_FAILED,
            )
        ]
    )
    (tmp_path / "package.json").write_text(
        json.dumps({"scripts": {"test": "missing-test-tool"}}),
        encoding="utf-8",
    )
    runtime = VerificationRuntime(tmp_path, command_runtime=fake)

    plan = runtime.plan_verification(changed_files=["src/app.js"], task_intent="code")
    observation = runtime.run_plan(plan.id)
    failed = observation["verification"]["failed_checks"]

    assert any(check["failure_type"] == FailureType.MISSING_COMMAND.value for check in failed)


def test_timeout_converts_to_timeout_verification_result(tmp_path: Path) -> None:
    request = CommandRequest(argv=["pytest"])
    fake = FakeCommandRuntime(
        [
            command_result(
                request,
                command_id="cmd_timeout",
                exit_code=None,
                semantic_status=SemanticStatus.RUNTIME_FAILED,
                output="timeout",
                error_code="timeout",
                execution_status=ExecutionStatus.TIMED_OUT,
                timed_out=True,
            )
        ]
    )
    (tmp_path / "package.json").write_text(json.dumps({"scripts": {"test": "pytest"}}), encoding="utf-8")
    runtime = VerificationRuntime(tmp_path, command_runtime=fake)

    plan = runtime.plan_verification(changed_files=["src/app.js"], task_intent="code")
    observation = runtime.run_plan(plan.id)

    assert any(check["status"] == CheckStatus.TIMEOUT.value for check in observation["verification"]["failed_checks"])


def test_post_verification_review_report_is_written_to_observation(tmp_path: Path) -> None:
    request = CommandRequest(argv=["pytest"])
    fake = FakeCommandRuntime(
        [
            command_result(
                request,
                command_id="cmd_fail",
                exit_code=1,
                semantic_status=SemanticStatus.TESTS_FAILED,
                output="FAILED tests/test_app.py::test_bad - AssertionError",
                error_code="semantic_failure",
            ),
            command_result(
                request,
                command_id="cmd_fail_again",
                exit_code=1,
                semantic_status=SemanticStatus.TESTS_FAILED,
                output="FAILED tests/test_app.py::test_bad - AssertionError",
                error_code="semantic_failure",
            ),
        ]
    )
    (tmp_path / "package.json").write_text(json.dumps({"scripts": {"test": "pytest"}}), encoding="utf-8")
    review = ReviewRuntime(tmp_path, enable_model_critic=False)
    runtime = VerificationRuntime(tmp_path, command_runtime=fake, review_runtime=review)

    plan = runtime.plan_verification(changed_files=["src/app.js"], task_intent="code")
    observation = runtime.run_plan(plan.id)

    report = observation["verification"]["review_report"]
    assert report["target"]["stage"] == "post_verification"
    assert report["decision"]["action"] == "repair"
    assert report["decision"]["repair_targets"]


def test_verification_runtime_sends_structured_observation_to_memory(tmp_path: Path) -> None:
    request = CommandRequest(argv=["pytest"])
    fake = FakeCommandRuntime(
        [
            command_result(
                request,
                command_id="cmd_fail",
                exit_code=1,
                semantic_status=SemanticStatus.TESTS_FAILED,
                output="FAILED tests/test_app.py::test_bad - AssertionError",
                error_code="semantic_failure",
            ),
            command_result(
                request,
                command_id="cmd_fail_again",
                exit_code=1,
                semantic_status=SemanticStatus.TESTS_FAILED,
                output="FAILED tests/test_app.py::test_bad - AssertionError",
                error_code="semantic_failure",
            ),
        ]
    )

    class FakeMemoryRuntime:
        def __init__(self) -> None:
            self.observations = []

        def ingest_verification_observation(self, observation):
            self.observations.append(observation)

    memory = FakeMemoryRuntime()
    (tmp_path / "package.json").write_text(json.dumps({"scripts": {"test": "pytest"}}), encoding="utf-8")
    runtime = VerificationRuntime(tmp_path, command_runtime=fake)
    runtime.memory_runtime = memory

    plan = runtime.plan_verification(changed_files=["src/app.js"], task_intent="code")
    observation = runtime.run_plan(plan.id)

    assert memory.observations == [observation]


def test_flaky_rerun_is_recorded_and_marked_flaky(tmp_path: Path) -> None:
    request = CommandRequest(argv=["pytest"])
    fake = FakeCommandRuntime(
        [
            command_result(
                request,
                command_id="cmd_fail",
                exit_code=1,
                semantic_status=SemanticStatus.TESTS_FAILED,
                output="FAILED tests/test_app.py::test_flaky - AssertionError",
                error_code="semantic_failure",
            ),
            command_result(
                request,
                command_id="cmd_pass",
                exit_code=0,
                semantic_status=SemanticStatus.SUCCEEDED,
                output="passed",
            ),
        ]
    )
    (tmp_path / "package.json").write_text(json.dumps({"scripts": {"test": "pytest"}}), encoding="utf-8")
    runtime = VerificationRuntime(tmp_path, command_runtime=fake)

    plan = runtime.plan_verification(changed_files=["src/app.js"], task_intent="code")
    observation = runtime.run_plan(plan.id)

    assert any(check["status"] == CheckStatus.FLAKY.value for check in observation["verification"]["failed_checks"])
    assert any(
        check["failure_type"] == FailureType.FLAKY_FAILURE.value
        for check in observation["verification"]["failed_checks"]
    )


def test_repair_budget_blocks_repeated_same_failure() -> None:
    state = RepairLoopState(budget=RepairBudget(max_same_failure_retries=1))
    controller = RepairLoopController(state)
    evidence = {
        "command_id": "cmd",
        "command": "pytest",
        "exit_code": 1,
        "output_excerpt": "failed",
        "artifact_ref": None,
        "artifact_path": None,
        "parsed_failures": [],
        "duration_ms": 1,
        "timestamp": "now",
    }
    result = {
        "check_id": "check",
        "kind": CheckKind.UNIT_TEST.value,
        "status": CheckStatus.FAILED.value,
        "failure_type": FailureType.UNIT_TEST_FAILURE.value,
        "evidence": evidence,
        "repair_hints": [],
        "confidence_impact": -0.2,
        "duration_ms": 1,
        "attempts": [],
    }
    from singularity.verification.models import VerificationEvidence, VerificationResult

    model = VerificationResult(
        check_id=result["check_id"],
        kind=CheckKind.UNIT_TEST,
        status=CheckStatus.FAILED,
        failure_type=FailureType.UNIT_TEST_FAILURE,
        evidence=VerificationEvidence(
            command_id="cmd",
            command="pytest",
            exit_code=1,
            output_excerpt="failed",
            artifact_path=None,
            parsed_failures=[],
            duration_ms=1,
            timestamp="now",
        ),
        repair_hints=[],
        confidence_impact=-0.2,
        duration_ms=1,
    )

    controller.record_result(model)
    controller.record_result(model)

    assert controller.can_continue() is False
    assert state.blocked_reason == "same_failure_retry_budget_exceeded"


def test_completion_assessment_statuses(tmp_path: Path) -> None:
    runtime = VerificationRuntime(tmp_path)
    plan = runtime.plan_verification(changed_files=["src/app.py"], task_intent="code")
    assessment = CompletionAssessor().assess(plan=plan, results=[])

    assert isinstance(plan, VerificationPlan)
    assert assessment.status in {CompletionStatus.BLOCKED, CompletionStatus.NEEDS_REVIEW}


def test_verification_tool_observation_enters_context_manager(tmp_path: Path) -> None:
    registry = ToolRegistry(tmp_path, include_default_tools=False)
    register_verification_tools(registry, VerificationRuntime(tmp_path))
    runtime = ToolRuntime(
        registry=registry,
        policy=ToolPolicy.coding_agent(),
        trace=None,
        workspace_root=tmp_path,
        policy_runtime=runtime_default_policy_runtime(tmp_path),
    )
    result = runtime.execute_tool_call(
        tool_call("plan_verification", {"changed_files": ["README.md"], "task_intent": "docs"})
    )
    context = ContextManager(system_prompt="system", user_goal="verify")

    observation = context.add_tool_result(
        tool_call=tool_call("plan_verification", {}),
        result=result.model_dump(mode="json"),
    )

    assert result.ok is True
    assert "verification_plan" in observation.preview


def test_direct_run_command_rejects_verification_like_commands(tmp_path: Path) -> None:
    registry = ToolRegistry(tmp_path, include_default_tools=False)
    register_command_tools(registry)
    runtime = ToolRuntime(
        registry=registry,
        policy=ToolPolicy.coding_agent(),
        trace=None,
        workspace_root=tmp_path,
        policy_runtime=runtime_default_policy_runtime(tmp_path),
    )

    result = runtime.execute_tool_call(
        tool_call(
            "run_command",
            {
                "argv": [sys.executable, "-m", "pytest"],
                "purpose": "PROJECT_VERIFICATION",
            },
        )
    )

    assert result.ok is False
    assert result.error_code == "verification_runtime_required"
