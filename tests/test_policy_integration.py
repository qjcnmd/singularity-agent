import json
import sys
from pathlib import Path
from typing import Any

from pydantic import BaseModel

from singularity.command import (
    CommandDecision,
    CommandExecutor,
    CommandPolicy,
    CommandRequest,
    ExecutionStatus,
)
from singularity.planner import EvidenceLedger, Planner, TaskState, TaskStatus
from singularity.planner.finalizer import Finalizer
from singularity.policy import (
    Capability,
    DecisionOutcome,
    OperationKind,
    PolicyComponent,
    PolicyConfig,
    PolicyEngine,
    PolicyRequest,
    PolicySubject,
    ResourceRef,
)
from singularity.policy.permissions import ApprovalPolicy, NetworkAccess, PermissionProfile, PermissionProfileName
from singularity.tools import PermissionLevel, ToolExecutor, ToolPolicy, ToolRegistry, ToolSpec
from singularity.verification import VerificationRunner
from singularity.verification.discovery import ProjectDetector
from singularity.verification.impact import ImpactAnalyzer
from singularity.verification.models import (
    CheckKind,
    VerificationCheck,
    VerificationDecision,
    VerificationPlan,
)
from singularity.verification.policy import VerificationPolicy
from singularity.workspace import CreateFile, WorkspaceMutationManager


class EmptyInput(BaseModel):
    pass


class CountingPolicyEngine(PolicyEngine):
    def __init__(self, tmp_path: Path) -> None:
        super().__init__(
            PolicyConfig(
                workspace_root=tmp_path,
                permission_profile=PermissionProfile.default_for_workspace(
                    tmp_path,
                    profile=PermissionProfileName.DANGER_FULL_ACCESS,
                ),
            )
        )
        self.calls: list[str] = []

    def enforce(self, request):  # type: ignore[no-untyped-def]
        self.calls.append(f"{request.component.value}:{request.operation.value}:{request.resource.identifier}")
        decision = super().evaluate(request)
        return decision.model_copy_with(
            outcome=DecisionOutcome.ALLOW,
            reason="test policy allows after recording",
            required_approval=None,
        )


class FailingCommandPolicy(CommandPolicy):
    def evaluate(self, *args: Any, **kwargs: Any):  # type: ignore[no-untyped-def]
        raise AssertionError("CommandPolicy.evaluate must not be a policy decision authority")


def tool_call(name: str, arguments: dict[str, Any]) -> dict[str, Any]:
    return {
        "id": "call_policy",
        "type": "function",
        "function": {"name": name, "arguments": json.dumps(arguments)},
    }


def test_tool_executor_dispatch_calls_policy_before_handler(tmp_path: Path) -> None:
    called = False

    def handler(_args: EmptyInput) -> str:
        nonlocal called
        called = True
        return "ok"

    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="safe_read",
            description="safe",
            input_model=EmptyInput,
            handler=handler,
            permission_level=PermissionLevel.READ_ONLY,
        )
    )
    policy = CountingPolicyEngine(tmp_path)
    component = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.coding_agent(),
        trace=None,
        workspace_root=tmp_path,
        policy_engine=policy,
    )

    result = component.execute_tool_call(tool_call("safe_read", {}))

    assert result.ok is True
    assert called is True
    assert policy.calls and policy.calls[0].startswith("tool:")


def test_mutation_manager_calls_policy_before_apply(tmp_path: Path) -> None:
    policy = CountingPolicyEngine(tmp_path)
    component = WorkspaceMutationManager(tmp_path, policy_engine=policy)

    result = component.apply_operations(
        [CreateFile(path="app.py", content="print('ok')\n")],
        intent="create app",
        created_by="test",
    )

    assert result.ok is True
    assert any(":create_file:" in call or ":mutate_file:" in call for call in policy.calls)


def test_command_executor_calls_policy_before_execute(tmp_path: Path) -> None:
    policy = CountingPolicyEngine(tmp_path)
    component = CommandExecutor(tmp_path, policy_engine=policy, policy=FailingCommandPolicy())

    result = component.run(
        CommandRequest(argv=[sys.executable, "-c", "print('ok')"], cwd=".")
    )

    assert result.execution_status == ExecutionStatus.COMPLETED
    assert result.policy_decision.decision == CommandDecision.ALLOW
    assert any(call.startswith("command:execute_command") for call in policy.calls)


def test_command_plan_projects_policy_engine_decision_without_command_policy_evaluate(
    tmp_path: Path,
) -> None:
    policy = CountingPolicyEngine(tmp_path)
    component = CommandExecutor(tmp_path, policy_engine=policy, policy=FailingCommandPolicy())

    plan = component.plan(CommandRequest(argv=[sys.executable, "-c", "print('plan')"], cwd="."))

    assert plan.policy_decision.decision == CommandDecision.ALLOW
    assert any(call.startswith("command:execute_command") for call in policy.calls)


def test_command_execution_uses_same_policy_authority_for_network_and_verification(
    tmp_path: Path,
) -> None:
    profile = PermissionProfile.default_for_workspace(
        tmp_path,
        network_access=NetworkAccess.DENIED,
        approval_policy=ApprovalPolicy.NEVER,
    )
    policy = PolicyEngine(PolicyConfig(workspace_root=tmp_path, permission_profile=profile))
    component = CommandExecutor(tmp_path, policy_engine=policy, policy=FailingCommandPolicy())
    request = CommandRequest(
        argv=[sys.executable, "-c", "print('net')"],
        cwd=".",
        network_mode="ALLOW_ALL",
    )

    command_result = component.run(request)
    runner = VerificationRunner(
        tmp_path,
        command_executor=component,
        policy_engine=policy,
    )
    check = VerificationCheck(
        kind=CheckKind.UNIT_TEST,
        command=request,
        scope="tests",
        required=True,
        timeout=30,
        risk_tags=[],
        failure_policy="fail_fast",
    )
    profile_for_plan = ProjectDetector(tmp_path).detect()
    plan = VerificationPlan(
        project_profile=profile_for_plan,
        impact_analysis=ImpactAnalyzer().analyze(
            changed_files=[],
            task_intent="tests",
            project_profile=profile_for_plan,
        ),
        required_checks=[check],
        optional_checks=[],
        skipped_checks=[],
        blocked_checks=[],
    )
    verification_request = runner._policy_request(plan, check)
    verification_decision = policy.enforce(verification_request)

    assert command_result.execution_status == ExecutionStatus.POLICY_DENIED
    assert command_result.error_code == "policy_denied"
    assert command_result.policy_decision.decision == CommandDecision.DENY
    assert verification_decision.outcome == DecisionOutcome.DENY


def test_verification_policy_preflight_uses_command_classification_only(
    tmp_path: Path,
) -> None:
    request = CommandRequest(
        argv=[sys.executable, "-c", "print('ok')"],
        cwd=".",
        purpose="PROJECT_VERIFICATION",
    )
    check = VerificationCheck(
        kind=CheckKind.UNIT_TEST,
        command=request,
        scope="tests",
        required=True,
        timeout=30,
        risk_tags=["unit_test"],
        failure_policy="fail_fast",
    )

    decision = VerificationPolicy(FailingCommandPolicy()).evaluate(
        check,
        workspace_root=tmp_path,
    )

    assert decision.decision == VerificationDecision.ALLOW
    assert decision.command_policy is None
    assert "EXECUTES_PROJECT_CODE" in decision.risk_tags


def test_permission_profile_controls_local_command_sandboxing(tmp_path: Path) -> None:
    request = PolicyRequest(
        session_id="session",
        task_id="task",
        phase_id="command",
        action_id="cmd",
        component=PolicyComponent.COMMAND,
        operation=OperationKind.EXECUTE_COMMAND,
        capability=Capability.EXECUTE_COMMAND,
        subject=PolicySubject(subject_type="component", name="CommandExecutor"),
        resource=ResourceRef("command", f"{sys.executable} -c \"print('ok')\""),
        reason=f"{sys.executable} -c \"print('ok')\"",
        proposed_by_model=True,
        metadata={
            "command": f"{sys.executable} -c \"print('ok')\"",
            "network_policy": "DISABLED",
            "filesystem_mode": "READ_ONLY_WORKSPACE",
        },
        workspace_root=str(tmp_path),
    )

    workspace_write = PolicyEngine(PolicyConfig(workspace_root=tmp_path))
    danger_full_access = PolicyEngine(
        PolicyConfig(
            workspace_root=tmp_path,
            permission_profile=PermissionProfile.default_for_workspace(
                tmp_path,
                profile=PermissionProfileName.DANGER_FULL_ACCESS,
            ),
        )
    )

    assert workspace_write.enforce(request).outcome == DecisionOutcome.SANDBOX_REQUIRED
    assert danger_full_access.enforce(request).outcome == DecisionOutcome.ALLOW


def test_verification_runner_does_not_bypass_policy(tmp_path: Path) -> None:
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
    policy = CountingPolicyEngine(tmp_path)
    command_executor = CommandExecutor(tmp_path, policy_engine=policy)
    component = VerificationRunner(tmp_path, command_executor=command_executor, policy_engine=policy)

    plan = component.plan_verification(changed_files=["tests/test_sample.py"], task_intent="tests")
    component.run_plan(plan.id)

    assert any(call.startswith("verification:verification") for call in policy.calls)
    assert any(call.startswith("command:verification") for call in policy.calls)


def test_planner_records_policy_observation_and_final_report_summary(tmp_path: Path) -> None:
    planner = Planner(tmp_path, session_id="session", task_id="task")
    planner.start_task("Policy blocked task")
    planner.record_policy_observation(
        {
            "outcome": "deny",
            "component": "command",
            "operation": "package_install",
            "reason": "package install requires review but session is non-interactive.",
            "risk_level": "high",
            "resource": "npm install",
        }
    )

    assert planner.evidence.policy_observations
    assert "[policy] Command denied" in planner.renderer.render(
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
    evidence = EvidenceLedger(policy_observations=planner.evidence.policy_observations)
    report = Finalizer().build(state=state, evidence=evidence)

    assert report.policy_approval_summary["denied_actions_count"] == 1
    assert report.policy_approval_summary["skipped_actions_due_to_policy"] == 1


def test_finalizer_sandbox_summary_reports_backend_and_enforcement_evidence() -> None:
    evidence = EvidenceLedger(
        sandbox_observations=[
            {
                "source": "verification",
                "backend": "windows_elevated",
                "status": "success",
                "enforcement_status": "available",
                "execution_backend": "account_restricted_token",
                "network_denied_verified": True,
                "process_tree_kill": True,
                "job_killed": False,
                "timeout_enforced": False,
                "artifact_count": 1,
                "artifact_refs": ["artifact_stdout"],
            }
        ]
    )

    summary = Finalizer._sandbox_summary(evidence)

    assert summary["selected_backends"] == ["windows_elevated"]
    assert summary["network_denied_verified_count"] == 1
    assert summary["local_process_backend_count"] == 0
    assert summary["artifact_refs"] == ["artifact_stdout"]


def test_finalizer_sandbox_summary_reports_reduced_backend_and_elevated_blocker() -> None:
    evidence = EvidenceLedger(
        sandbox_observations=[
            {
                "source": "verification",
                "backend": "windows_unelevated",
                "status": "success",
                "sandbox_enforcement": "reduced",
                "enforcement_status": "degraded",
                "execution_backend": "current_user_process",
                "fallback_used": True,
                "fallback_reason": "python_c_extension_low_integrity_runtime_initialization_failed",
                "elevated_available": False,
                "elevated_blocker_summary": (
                    "python_c_extension_low_integrity_runtime_initialization_failed"
                ),
                "artifact_count": 0,
                "artifact_refs": [],
            }
        ]
    )

    summary = Finalizer._sandbox_summary(evidence)

    assert summary["selected_backends"] == ["windows_unelevated"]
    assert summary["local_process_backend_count"] == 0
    assert summary["reduced_backend_count"] == 1
    assert summary["reduced_backends"] == ["windows_unelevated"]
    assert summary["elevated_blocker_summaries"] == [
        "python_c_extension_low_integrity_runtime_initialization_failed"
    ]
