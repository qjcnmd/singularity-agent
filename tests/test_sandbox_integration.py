import sys
from pathlib import Path

from singularity.command import CommandRequest, CommandRuntime, ExecutionStatus, SemanticStatus
from singularity.planner import EvidenceLedger, PlannerRuntime, TaskState, TaskStatus
from singularity.planner.finalizer import Finalizer
from singularity.policy import (
    DecisionOutcome,
    PolicyConfig,
    PolicyConstraints,
    PolicyDecision,
    PolicyRuntime,
    SecurityMode,
)
from singularity.verification import FailureType, VerificationRuntime


class SandboxRequiredPolicy(PolicyRuntime):
    def __init__(
        self,
        root: Path,
        *,
        hard_network: bool = False,
        security_mode: SecurityMode = SecurityMode.STRICT,
    ) -> None:
        super().__init__(PolicyConfig(workspace_root=root, security_mode=security_mode))
        self.hard_network = hard_network

    def enforce(self, request):  # type: ignore[no-untyped-def]
        return PolicyDecision(
            request_id=request.request_id,
            outcome=DecisionOutcome.SANDBOX_REQUIRED,
            reason="test requires sandbox",
            constraints=PolicyConstraints(
                sandbox_required=True,
                filesystem_mode="copy_on_write_workspace",
                network_allowed=False,
                max_duration_seconds=request.metadata.get("timeout"),
                max_output_chars=20000,
                allowed_hosts=["hard-network-required"] if self.hard_network else [],
            ),
        )


def test_command_runtime_routes_sandbox_required_command_without_real_workspace_write(tmp_path: Path) -> None:
    runtime = CommandRuntime(
        tmp_path,
        policy_runtime=SandboxRequiredPolicy(
            tmp_path,
            security_mode=SecurityMode.COMPAT,
        ),
    )

    result = runtime.run(
        CommandRequest(
            argv=[
                sys.executable,
                "-c",
                "from pathlib import Path; Path('only-sandbox.txt').write_text('x', encoding='utf-8'); print('ok')",
            ],
            cwd=".",
        )
    )

    assert result.execution_status == ExecutionStatus.COMPLETED
    assert result.semantic_status == SemanticStatus.SUCCEEDED
    assert not (tmp_path / "only-sandbox.txt").exists()
    assert result.isolation_report["sandbox"]["status"] == "success"
    assert result.changed_files == ["only-sandbox.txt"]


def test_strict_sandbox_required_command_fails_closed_without_hard_isolation(tmp_path: Path) -> None:
    runtime = CommandRuntime(
        tmp_path,
        policy_runtime=SandboxRequiredPolicy(tmp_path),
    )

    result = runtime.run(
        CommandRequest(
            argv=[sys.executable, "-c", "print('ok')"],
            cwd=".",
        )
    )

    assert result.execution_status == ExecutionStatus.BACKEND_ERROR
    assert result.error_code == "sandbox_unavailable"
    assert result.isolation_report["sandbox"]["status"] == "backend_unavailable"


def test_verification_evidence_records_sandbox_metadata(tmp_path: Path) -> None:
    (tmp_path / "pyproject.toml").write_text(
        """
[project]
name = "sample"

[tool.pytest.ini_options]
testpaths = ["tests"]
""",
        encoding="utf-8",
    )
    tests_dir = tmp_path / "tests"
    tests_dir.mkdir()
    (tests_dir / "test_sample.py").write_text("def test_ok():\n    assert True\n", encoding="utf-8")
    command_runtime = CommandRuntime(
        tmp_path,
        policy_runtime=SandboxRequiredPolicy(
            tmp_path,
            security_mode=SecurityMode.COMPAT,
        ),
    )
    runtime = VerificationRuntime(
        tmp_path,
        command_runtime=command_runtime,
        policy_runtime=SandboxRequiredPolicy(
            tmp_path,
            security_mode=SecurityMode.COMPAT,
        ),
    )

    plan = runtime.plan_verification(changed_files=["tests/test_sample.py"], task_intent="tests")
    observation = runtime.run_plan(plan.id)
    results = observation["verification"]["results"]

    assert observation["verification"]["check_status"]
    sandboxed = [result for result in results if result["evidence"]["sandbox_id"]]
    assert sandboxed
    assert any(result["evidence"]["sandbox_status"] == "success" for result in sandboxed)
    assert all(result["evidence"]["sandbox_backend"] == "local_staging" for result in sandboxed)


def test_sandbox_unavailable_becomes_verification_failure_evidence(tmp_path: Path) -> None:
    (tmp_path / "pyproject.toml").write_text(
        """
[project]
name = "sample"

[tool.pytest.ini_options]
testpaths = ["tests"]
""",
        encoding="utf-8",
    )
    (tmp_path / "tests").mkdir()
    (tmp_path / "tests" / "test_sample.py").write_text("def test_ok():\n    assert True\n", encoding="utf-8")
    command_runtime = CommandRuntime(
        tmp_path,
        policy_runtime=SandboxRequiredPolicy(tmp_path, hard_network=True),
    )
    runtime = VerificationRuntime(
        tmp_path,
        command_runtime=command_runtime,
        policy_runtime=SandboxRequiredPolicy(tmp_path),
    )

    plan = runtime.plan_verification(changed_files=["tests/test_sample.py"], task_intent="tests")
    observation = runtime.run_plan(plan.id)

    assert any(
        check["failure_type"] == FailureType.SANDBOX_LIMITATION.value
        for check in observation["verification"]["failed_checks"]
    )


def test_planner_context_and_final_report_include_sandbox_summary(tmp_path: Path) -> None:
    planner = PlannerRuntime(tmp_path, session_id="session", task_id="task")
    planner.start_task("Sandbox summary")
    planner.update_from_command(
        {
            "command_result": {
                "command_id": "cmd_1",
                "semantic_status": "succeeded",
                "changed_files": ["generated.txt"],
                "isolation_report": {
                    "sandbox": {
                        "sandbox_id": "sandbox_1",
                        "backend": "local_staging",
                        "status": "success",
                        "artifact_count": 1,
                        "changed_files_count": 1,
                    }
                },
            }
        }
    )

    rendered = planner.renderer.render(
        state=planner.state,
        plan=planner.plan,
        evidence=planner.evidence,
    )
    state = TaskState(
        task_id="task",
        session_id="session",
        user_goal="report",
        normalized_goal="report",
        status=TaskStatus.COMPLETED,
    )
    evidence = EvidenceLedger(command_results=planner.evidence.command_results)
    report = Finalizer().build(state=state, evidence=evidence)

    assert "[sandbox]" in rendered
    assert report.sandbox_isolation_summary["sandboxed_commands_count"] == 1
    assert report.sandbox_isolation_summary["artifact_count"] == 1
    assert report.sandbox_isolation_summary["imported_changes_count"] == 0
