from __future__ import annotations

import json
from pathlib import Path

from singularity.command import CommandRequest, SemanticStatus
from singularity.planner import Planner, SemanticPlanner, TaskContractBuilder, TaskStatus
from singularity.verification import VerificationRunner
from tests.test_verification_runner import FakeCommandExecutor, command_result


def test_semantic_planner_generates_multi_requirement_rolling_plan() -> None:
    contract = TaskContractBuilder().build(
        "fallback",
        structured_output={
            "user_goal": "create and verify",
            "acceptance_criteria": [
                {
                    "criterion_id": "deliver_app",
                    "description": "app.py exists",
                    "evidence": ["applied_changes"],
                },
                {
                    "criterion_id": "verify_app",
                    "description": "pytest passes",
                    "evidence": ["verification_results"],
                },
            ],
            "deliverables": [{"kind": "file", "description": "app", "path": "app.py"}],
            "verification_requirements": [
                {"description": "run pytest", "command": ["python", "-m", "pytest"]}
            ],
        },
    )

    plan = SemanticPlanner().initial_plan(contract)

    criterion_steps = [step for step in plan.steps if step.acceptance_criterion_id]
    assert [step.acceptance_criterion_id for step in criterion_steps] == [
        "deliver_app",
        "verify_app",
    ]
    assert criterion_steps[0].allowed_capabilities == ["apply_patch", "inspect_diff", "read_file", "write_file"]
    assert criterion_steps[0].expected_evidence[0].evidence_key == "applied_changes"
    assert criterion_steps[1].dependencies[0].step_id == criterion_steps[0].step_id
    assert "run_verification" in criterion_steps[1].allowed_capabilities


def test_repair_step_binds_failed_acceptance_criterion() -> None:
    contract = TaskContractBuilder().build("Create quicksort.py and run smoke verification")
    analysis = {
        "check_id": "check_smoke",
        "failure_type": "unit_test_failure",
        "suspect_files": ["quicksort.py"],
        "root_cause": {"description": "quicksort.py assertion failed"},
        "repair_plan": {
            "steps": [{"target_file": "quicksort.py", "next_verification": {"command": ["python", "quicksort.py"]}}],
            "next_verification": {"check_id": "check_smoke", "command": ["python", "quicksort.py"]},
        },
    }

    plan = SemanticPlanner().repair_plan(analysis, task_contract=contract)

    assert plan.steps[0].kind == "repair"
    assert plan.steps[0].acceptance_criterion_id == "verify_quicksort_py"
    assert plan.steps[0].expected_evidence[0].evidence_key == "verification_results"
    assert plan.steps[0].fallback_steps[0].reason == "repair_step_failed"


def test_planner_context_exposes_rolling_plan_and_current_step_tools(tmp_path: Path) -> None:
    planner = Planner(tmp_path, session_id="session_1", task_id="task_1")
    planner.start_task("Create quicksort.py and run smoke verification")
    planner.state.status = TaskStatus.UNDERSTANDING_TASK
    planner.state.current_phase = "understanding_task"
    deliver_step = next(
        step
        for step in planner.semantic_rolling_plan().steps
        if step.acceptance_criterion_id == "deliver_quicksort_py"
    )
    planner.state.rolling_plan["current_step_id"] = deliver_step.step_id
    tools = [
        {"function": {"name": "read_file"}},
        {"function": {"name": "write_file"}},
        {"function": {"name": "run_verification"}},
    ]

    exposed = {tool["function"]["name"] for tool in planner.filtered_tools(tools)}
    context = json.loads(planner.planner_context_message()["content"])["planner"]

    assert "write_file" in exposed
    assert context["rolling_plan"]["current_step_id"] == deliver_step.step_id
    assert context["rolling_plan"]["steps"][1]["acceptance_criterion_id"] == "deliver_quicksort_py"


def test_failure_analysis_updates_repair_rolling_plan(tmp_path: Path) -> None:
    request = CommandRequest(argv=["python", "quicksort.py"])
    fake = FakeCommandExecutor(
        [
            command_result(
                request,
                command_id="cmd_fail",
                exit_code=1,
                semantic_status=SemanticStatus.TESTS_FAILED,
                output="FAILED quicksort.py::test_smoke - AssertionError",
                error_code="semantic_failure",
            )
        ]
    )
    planner = Planner(tmp_path, session_id="session_1", task_id="task_1")
    planner.start_task("Create quicksort.py and run smoke verification")
    component = VerificationRunner(tmp_path, command_executor=fake, planner=planner)
    plan = component.plan_verification(
        changed_files=[],
        task_intent="verify quicksort",
        smoke_commands=[["python", "quicksort.py"]],
    )

    component.run_plan(plan.id)

    rolling = planner.semantic_rolling_plan()
    assert rolling.steps[0].kind == "repair"
    assert rolling.steps[0].acceptance_criterion_id == "verify_quicksort_py"
    assert rolling.steps[0].expected_evidence[0].evidence_key == "verification_results"
