from __future__ import annotations

from pathlib import Path

from singularity.command import CommandRequest, SemanticStatus
from singularity.planner import Planner
from singularity.verification import FailureAnalysisPipeline, FailureType, VerificationRunner
from tests.test_verification_runner import FakeCommandExecutor, command_result


def test_failing_pytest_generates_failure_analysis_and_repair_plan(tmp_path: Path) -> None:
    request = CommandRequest(argv=["python", "-m", "pytest", "tests/test_app.py::test_bad"])
    fake = FakeCommandExecutor(
        [
            command_result(
                request,
                command_id="cmd_fail",
                exit_code=1,
                semantic_status=SemanticStatus.TESTS_FAILED,
                output="FAILED tests/test_app.py::test_bad - AssertionError: bad value",
                error_code="semantic_failure",
            )
        ]
    )
    component = VerificationRunner(tmp_path, command_executor=fake)

    plan = component.plan_verification(
        changed_files=[],
        task_intent="fix failing app test",
        smoke_commands=[["python", "-m", "pytest", "tests/test_app.py::test_bad"]],
    )
    observation = component.run_plan(plan.id)

    analysis = observation["verification"]["failure_analysis"][0]
    repair_plan = observation["verification"]["repair_plan"]
    assert analysis["failure_type"] == FailureType.UNIT_TEST_FAILURE.value
    assert analysis["suspect_files"] == ["tests/test_app.py"]
    assert "AssertionError" in analysis["root_cause"]["description"]
    assert repair_plan["steps"][0]["target_file"] == "tests/test_app.py"
    assert repair_plan["next_verification"]["command"] == [
        "python",
        "-m",
        "pytest",
        "tests/test_app.py::test_bad",
    ]


def test_same_failure_repeated_routes_to_no_progress() -> None:
    component = FailureAnalysisPipeline(max_same_failure_retries=1)
    result = {
        "check_id": "check_pytest",
        "failure_type": "unit_test_failure",
        "evidence": {
            "command": "python -m pytest tests/test_app.py::test_bad",
            "stdout_excerpt": "FAILED tests/test_app.py::test_bad - AssertionError",
            "stderr_excerpt": "",
            "parsed_failures": [
                {
                    "file": "tests/test_app.py",
                    "line": None,
                    "symbol": None,
                    "test_name": "test_bad",
                    "message": "AssertionError",
                }
            ],
        },
        "repair_hints": [],
    }

    first = component.analyze_result(result, changed_files=["app.py"])
    second = component.analyze_result(result, changed_files=["app.py"])

    assert first.no_progress_reason is None
    assert second.no_progress_reason == "same_failure_retry_budget_exceeded"
    assert second.repair_plan.strategy == "stop_and_ask"


def test_golden_failure_repair_rerun_passes(tmp_path: Path) -> None:
    request = CommandRequest(argv=["python", "-m", "pytest", "tests/test_app.py::test_bad"])
    fake = FakeCommandExecutor(
        [
            command_result(
                request,
                command_id="cmd_fail",
                exit_code=1,
                semantic_status=SemanticStatus.TESTS_FAILED,
                output="FAILED tests/test_app.py::test_bad - AssertionError: bad value",
                error_code="semantic_failure",
            ),
            command_result(
                request,
                command_id="cmd_pass",
                exit_code=0,
                semantic_status=SemanticStatus.SUCCEEDED,
                output="1 passed",
            ),
        ]
    )
    component = VerificationRunner(tmp_path, command_executor=fake)
    plan = component.plan_verification(
        changed_files=[],
        task_intent="fix failing app test",
        smoke_commands=[["python", "-m", "pytest", "tests/test_app.py::test_bad"]],
    )

    failed = component.run_plan(plan.id)
    (tmp_path / "app.py").write_text("FIXED = True\n", encoding="utf-8")
    passed = component.rerun_check(
        plan_id=plan.id,
        check_id=failed["verification"]["repair_plan"]["next_verification"]["check_id"],
    )

    assert failed["verification"]["repair_plan"]["steps"][0]["next_verification"]["command"] == [
        "python",
        "-m",
        "pytest",
        "tests/test_app.py::test_bad",
    ]
    assert passed["verification"]["completion_assessment"]["status"] == "ready"
    assert "failure_analysis" not in passed["verification"]


def test_verification_failure_analysis_updates_planner_context(tmp_path: Path) -> None:
    request = CommandRequest(argv=["python", "-m", "pytest", "tests/test_app.py::test_bad"])
    fake = FakeCommandExecutor(
        [
            command_result(
                request,
                command_id="cmd_fail",
                exit_code=1,
                semantic_status=SemanticStatus.TESTS_FAILED,
                output="FAILED tests/test_app.py::test_bad - AssertionError: bad value",
                error_code="semantic_failure",
            )
        ]
    )
    planner = Planner(tmp_path, session_id="session_1", task_id="task_1")
    planner.start_task("fix failing pytest")
    component = VerificationRunner(tmp_path, command_executor=fake, planner=planner)
    plan = component.plan_verification(
        changed_files=[],
        task_intent="fix failing app test",
        smoke_commands=[["python", "-m", "pytest", "tests/test_app.py::test_bad"]],
    )

    component.run_plan(plan.id)

    assert planner.evidence.failure_analyses
    assert planner.evidence.repair_plans
    context = planner.planner_context_message()["content"]
    assert "failure_analysis" in context
    assert "repair_plan" in context
