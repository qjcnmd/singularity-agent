import json
from pathlib import Path

from miniharness.planner import (
    ActionKind,
    AgentAction,
    EvidenceLedger,
    ExecutionBudget,
    PlannerRuntime,
    PlannerStore,
    ReplanDecisionKind,
    RiskDecisionKind,
    TaskStatus,
)
from miniharness.tools.models import ToolResult, ToolSpec, PermissionLevel
from pydantic import BaseModel
from miniharness.workspace import CreateFile, MutationRuntime
from miniharness.command import CommandRequest, CommandRuntime
from miniharness.verification import VerificationRuntime


class EmptyInput(BaseModel):
    pass


def spec(name: str, *, permission: PermissionLevel = PermissionLevel.READ_ONLY) -> ToolSpec:
    return ToolSpec(
        name=name,
        version="test",
        description=name,
        input_model=EmptyInput,
        handler=lambda _args: {"ok": True},
        permission_level=permission,
        uses_mutation_runtime=permission == PermissionLevel.WRITE,
        uses_command_runtime=permission == PermissionLevel.SHELL,
    )


def test_start_task_builds_state_plan_and_persists(tmp_path: Path) -> None:
    planner = PlannerRuntime(tmp_path, session_id="session_1", task_id="task_1")

    state = planner.start_task("Add planner runtime")

    assert state.task_id == "task_1"
    assert state.session_id == "session_1"
    assert state.user_goal == "Add planner runtime"
    assert state.status == TaskStatus.UNDERSTANDING_TASK
    assert planner.plan is not None
    assert [phase.phase_id for phase in planner.plan.phases] == [
        "understanding_task",
        "inspecting_workspace",
        "planning_changes",
        "applying_changes",
        "running_verification",
        "repairing_failures",
        "finalizing",
    ]
    assert (tmp_path / ".miniharness" / "planner" / "session_1" / "state.json").exists()
    assert (tmp_path / ".miniharness" / "planner" / "session_1" / "planner_events.jsonl").exists()


def test_phase_policy_allows_read_tools_before_mutation_and_blocks_write(tmp_path: Path) -> None:
    planner = PlannerRuntime(tmp_path, session_id="session_1", task_id="task_1")
    planner.start_task("Inspect first")
    planner.state.status = TaskStatus.INSPECTING_WORKSPACE
    planner.state.current_phase = "inspecting_workspace"

    allowed = planner.authorize_tool_call(
        tool_name="read_file",
        tool_call_id="call_read",
        spec=spec("read_file"),
        arguments={"path": "README.md"},
    )
    denied = planner.authorize_tool_call(
        tool_name="workspace_create_file",
        tool_call_id="call_write",
        spec=spec("workspace_create_file", permission=PermissionLevel.WRITE),
        arguments={"path": "x.txt"},
    )

    assert allowed.allowed is True
    assert allowed.action is not None
    assert allowed.action.kind == ActionKind.READ_RELEVANT_FILES
    assert denied.allowed is False
    assert denied.error_code == "action_not_allowed"


def test_tool_result_updates_evidence_ledger_and_advances_phase(tmp_path: Path) -> None:
    planner = PlannerRuntime(tmp_path, session_id="session_1", task_id="task_1")
    planner.start_task("Read README")
    planner.state.status = TaskStatus.INSPECTING_WORKSPACE
    planner.state.current_phase = "inspecting_workspace"
    decision = planner.authorize_tool_call(
        tool_name="read_file",
        tool_call_id="call_read",
        spec=spec("read_file"),
        arguments={"path": "README.md"},
    )

    planner.update_from_tool_result(
        tool_call_id="call_read",
        tool_name="read_file",
        result=ToolResult.success(
            content={"path": "README.md", "content": "hello", "bytes_read": 5}
        ),
        action_id=decision.action.action_id,
    )

    assert planner.evidence.inspected_files == ["README.md"]
    assert planner.state.current_phase == "planning_changes"
    assert planner.state.status == TaskStatus.PLANNING_CHANGES


def test_mutation_command_and_verification_results_update_evidence(tmp_path: Path) -> None:
    planner = PlannerRuntime(tmp_path, session_id="session_1", task_id="task_1")
    planner.start_task("Change code")

    planner.update_from_mutation(
        {
            "mutation_status": "applied",
            "changed_files": ["src/app.py"],
            "changeset_id": "change_1",
            "transaction_id": "tx_1",
        },
        tool_call_id="call_mutate",
    )
    planner.update_from_command(
        {
            "command_result": {
                "command_id": "cmd_1",
                "semantic_status": "succeeded",
                "changed_files": [],
            }
        },
        tool_call_id="call_cmd",
    )
    planner.update_from_verification(
        {
            "verification": {
                "completion_assessment": {
                    "status": "ready",
                    "warnings": [],
                    "remaining_risks": [],
                },
                "check_status": [
                    {"check_id": "check_1", "kind": "unit_test", "status": "passed"}
                ],
            }
        },
        tool_call_id="call_verify",
    )

    assert planner.evidence.applied_changes[0]["transaction_id"] == "tx_1"
    assert planner.state.linked_transactions == ["tx_1"]
    assert planner.state.linked_commands == ["cmd_1"]
    assert planner.state.linked_verifications == ["check_1"]
    assert planner.state.final_assessment["status"] == "ready"


def test_completion_requires_evidence_before_completed(tmp_path: Path) -> None:
    planner = PlannerRuntime(tmp_path, session_id="session_1", task_id="task_1")
    planner.start_task("Change code")
    planner.evidence.inspected_files.append("README.md")

    assessment = planner.assess_completion()

    assert assessment["status"] == TaskStatus.BLOCKED.value
    assert "required_changes_applied" in assessment["unmet"]
    assert planner.state.status != TaskStatus.COMPLETED


def test_final_report_is_generated_from_evidence(tmp_path: Path) -> None:
    planner = PlannerRuntime(tmp_path, session_id="session_1", task_id="task_1")
    planner.start_task("Change code")
    planner.evidence.inspected_files.append("README.md")
    planner.evidence.applied_changes.append(
        {"changed_files": ["README.md"], "transaction_id": "tx_1"}
    )
    planner.state.linked_transactions.append("tx_1")
    planner.evidence.verification_results.append(
        {
            "completion_assessment": {
                "status": "ready",
                "warnings": [],
                "remaining_risks": [],
            },
            "check_status": [{"check_id": "check_1", "status": "passed"}],
        }
    )
    planner.state.final_assessment = {"status": "ready"}

    report = planner.finalize()

    assert report.status == TaskStatus.COMPLETED
    assert report.files_changed == ["README.md"]
    assert report.verification_summary["status"] == "ready"
    assert (tmp_path / ".miniharness" / "planner" / "session_1" / "final_report.json").exists()


def test_replanner_maps_required_failures(tmp_path: Path) -> None:
    planner = PlannerRuntime(tmp_path, session_id="session_1", task_id="task_1")
    planner.start_task("Repair")

    assert planner.replan({"error_code": "patch_context_not_found"}).decision == ReplanDecisionKind.READ_FRESH_FILE
    assert planner.replan({"error_code": "snapshot_mismatch"}).decision == ReplanDecisionKind.READ_FRESH_FILE
    assert planner.replan({"verification_failed": True}).decision == ReplanDecisionKind.REPAIR_FAILURE

    planner.budget.repeated_failures["same"] = planner.budget.max_repeated_failures
    decision = planner.replan({"failure_fingerprint": "same"})
    assert decision.decision == ReplanDecisionKind.ASK_USER
    assert planner.state.status == TaskStatus.BLOCKED


def test_risk_escalation_requires_review_for_high_risk_actions(tmp_path: Path) -> None:
    planner = PlannerRuntime(tmp_path, session_id="session_1", task_id="task_1")
    planner.start_task("Change CI")
    planner.state.status = TaskStatus.APPLYING_CHANGES
    planner.state.current_phase = "applying_changes"

    decision = planner.authorize_tool_call(
        tool_name="workspace_create_file",
        tool_call_id="call_ci",
        spec=spec("workspace_create_file", permission=PermissionLevel.WRITE),
        arguments={"path": ".github/workflows/ci.yml"},
    )

    assert decision.allowed is False
    assert decision.risk_decision == RiskDecisionKind.REQUIRE_REVIEW
    assert planner.state.status == TaskStatus.NEEDS_REVIEW


def test_interrupt_and_resume_restore_state_and_health_conflict(tmp_path: Path) -> None:
    planner = PlannerRuntime(tmp_path, session_id="session_1", task_id="task_1")
    planner.start_task("Resume")
    planner.evidence.inspected_files.append("README.md")
    planner.interrupt("pause")

    resumed = PlannerRuntime(tmp_path).resume("session_1")

    assert resumed.state.status == TaskStatus.RECOVERING
    assert resumed.evidence.inspected_files == ["README.md"]

    conflicted = resumed.resume(
        "session_1",
        workspace_health={"status": "conflicted", "external_changes": ["README.md"]},
    )
    assert conflicted.state.status == TaskStatus.NEEDS_REVIEW
    assert "workspace conflict on resume" in conflicted.state.blocked_reasons


def test_planner_trace_records_phase_action_budget_and_risk(tmp_path: Path) -> None:
    planner = PlannerRuntime(tmp_path, session_id="session_1", task_id="task_1")
    planner.start_task("Trace")
    planner.step()

    events = [
        json.loads(line)
        for line in (tmp_path / ".miniharness" / "planner" / "session_1" / "planner_events.jsonl")
        .read_text(encoding="utf-8")
        .splitlines()
    ]
    last = events[-1]

    assert last["event"] == "planner"
    assert last["task_id"] == "task_1"
    assert "phase" in last
    assert "budget_state" in last
    assert "risk_level" in last


def test_mutation_runtime_observer_updates_planner_with_rich_result(tmp_path: Path) -> None:
    planner = PlannerRuntime(tmp_path, session_id="session_1", task_id="task_1")
    planner.start_task("Create file")
    runtime = MutationRuntime(tmp_path, planner=planner)

    result = runtime.apply_operations(
        [CreateFile(path="app.py", content="print('ok')\n")],
        intent="create app",
        created_by="test",
        tool_call_id="call_mutate",
    )

    assert result.ok is True
    assert planner.evidence.applied_changes[0]["changed_files"] == ["app.py"]
    assert planner.state.linked_transactions == [result.transaction_id]


def test_command_runtime_observer_updates_planner_with_rich_result(tmp_path: Path) -> None:
    planner = PlannerRuntime(tmp_path, session_id="session_1", task_id="task_1")
    planner.start_task("Run command")
    runtime = CommandRuntime(tmp_path, planner=planner)

    result = runtime.run(CommandRequest(argv=["python", "-c", "print('ok')"]), tool_call_id="call_cmd")

    assert result.command_id in planner.state.linked_commands
    assert planner.evidence.command_results[0]["command_id"] == result.command_id


def test_verification_runtime_observer_updates_planner_with_assessment(tmp_path: Path) -> None:
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
    planner = PlannerRuntime(tmp_path, session_id="session_1", task_id="task_1")
    planner.start_task("Verify")
    runtime = VerificationRuntime(tmp_path, planner=planner)

    plan = runtime.plan_verification(changed_files=["tests/test_sample.py"], task_intent="tests")
    runtime.run_plan(plan.id)

    assert planner.evidence.verification_results
    assert planner.state.final_assessment["status"] in {
        "ready",
        "ready_with_warnings",
        "blocked",
        "failed",
        "needs_review",
    }
