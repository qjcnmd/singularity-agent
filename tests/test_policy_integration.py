import json
import sys
from pathlib import Path
from typing import Any

from pydantic import BaseModel

from singularity.command import CommandRequest, CommandExecutor, ExecutionStatus
from singularity.planner import EvidenceLedger, Planner, TaskStatus, TaskState
from singularity.planner.finalizer import Finalizer
from singularity.policy import (
    Capability,
    DecisionOutcome,
    OperationKind,
    PolicyConfig,
    PolicyRequest,
    PolicyEngine,
    PolicySubject,
    ResourceRef,
    PolicyComponent,
)
from singularity.policy.permissions import PermissionProfile, PermissionProfileName
from singularity.tools import PermissionLevel, ToolPolicy, ToolRegistry, ToolExecutor, ToolSpec
from singularity.verification import VerificationRunner
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
    component = CommandExecutor(tmp_path, policy_engine=policy)

    result = component.run(
        CommandRequest(argv=[sys.executable, "-c", "print('ok')"], cwd=".")
    )

    assert result.execution_status == ExecutionStatus.COMPLETED
    assert any(call.startswith("command:execute_command") for call in policy.calls)


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
