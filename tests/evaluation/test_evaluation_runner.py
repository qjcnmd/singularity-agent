from __future__ import annotations

import json
import sys
from pathlib import Path
from typing import Any

from singularity.evaluation.runner import (
    EVALUATION_RESULT_SCHEMA_VERSION,
    EVALUATION_TASK_SET_SCHEMA_VERSION,
    EvaluationRunner,
    EvaluationTaskSet,
    EvaluationTaskResult,
    SingularityPrivateBenchmarkAdapter,
    load_evaluation_task_set,
    summarize_evaluation_results,
)
from singularity.evaluation.failure_case_replay import FailureCaseReplayRunner
from singularity.evaluation.models import FAILURE_CASE_RECORD_SCHEMA_VERSION
from singularity.kernel.finalization import FinalReport
from singularity.kernel.models import RunStatus
from tests.agent_loop_helpers import make_agent_session


def test_load_evaluation_task_set_example() -> None:
    manifest = load_evaluation_task_set(Path("docs/evaluation/capability-minimal-tasks.json"))

    assert manifest.schema_version == EVALUATION_TASK_SET_SCHEMA_VERSION
    assert [task.task_id for task in manifest.tasks] == [
        "benchmark.create_quicksort",
        "benchmark.modify_existing_code",
        "benchmark.fix_math_test",
        "benchmark.reject_out_of_scope_change",
        "benchmark.verification_contract",
        "benchmark.completion_rejected_repair",
        "benchmark.policy_blocked",
    ]
    assert manifest.tasks[0].workspace.kind == "fixture"
    assert manifest.tasks[0].allowed_paths == ["quicksort.py"]
    assert manifest.tasks[0].expected_file_changes == ["quicksort.py"]
    assert manifest.tasks[0].completion_standard
    assert "smoke-test" in manifest.tasks[0].risk_tags
    assert manifest.tasks[-1].tool_policy == "read_only"


def test_load_evaluation_regression_manifest_declares_required_task_classes() -> None:
    manifest = load_evaluation_task_set(Path("docs/evaluation/capability-regression-tasks.json"))

    assert len(manifest.tasks) == 4
    by_type = {task.task_type: task for task in manifest.tasks}
    assert set(by_type) == {
        "simple_patch",
        "multi_file_reasoning",
        "failure_repair",
        "completion_gate",
    }
    for task in manifest.tasks:
        assert task.workspace.kind == "fixture"
        assert task.expected_file_changes
        assert task.verification_command
        assert task.public_verification_command
        assert task.hidden_verification_command
        assert task.completion_standard
        assert task.risk_tags
        assert "public-verification" in task.risk_tags
        assert "hidden-verification" in task.risk_tags
    assert by_type["multi_file_reasoning"].expected_file_changes == ["cart.py", "policy.py"]
    assert "completion-gate" in by_type["completion_gate"].risk_tags


def test_load_v7_focused_smoke_manifest_remains_single_task() -> None:
    manifest = load_evaluation_task_set(Path("docs/evaluation/capability-fix-math-test-only.json"))

    assert [task.task_id for task in manifest.tasks] == ["benchmark.fix_math_test"]
    assert manifest.tasks[0].task_type == "v7_smoke"
    assert manifest.tasks[0].public_verification_command == manifest.tasks[0].verification_command
    assert manifest.tasks[0].hidden_verification_command == manifest.tasks[0].verification_command


def test_evaluation_sanitized_baseline_example_is_safe_and_shape_current() -> None:
    path = Path("docs/evaluation/evaluation-baseline-example.json")
    text = path.read_text(encoding="utf-8")
    payload = json.loads(text)

    assert payload["schema_version"] == EVALUATION_RESULT_SCHEMA_VERSION
    assert "SINGULARITY_API_KEY" not in text
    assert "api_key" not in text.lower()
    assert "sk-" not in text
    task = payload["tasks"][0]
    assert task["reproducible_environment"]["schema_version"] == "evaluation.environment/v1"
    for field in [
        "agent_completed",
        "evaluation_passed",
        "completed",
        "patch_applicable",
        "allowed_scope_passed",
        "public_verification_passed",
        "hidden_verification_passed",
        "contract_satisfaction",
        "miscompletion_count",
        "repair_attempt_count",
        "repair_execution_count",
        "turn_count",
        "tool_calls",
        "blocked_reason",
        "failure_category",
        "final_report_status",
    ]:
        assert field in task
    for removed in [
        "tool_call_count",
        "failure_repair_count",
        "task_verification_result",
        "repair_verification_contract",
        "result_extraction",
        "agent_loop_ref",
    ]:
        assert removed not in task


def test_private_adapter_converts_benchmark_tasks_to_evaluation_task_set(tmp_path: Path) -> None:
    task_set = tmp_path / "private.json"
    task_set.write_text(
        json.dumps(
            {
                "schema_version": "evaluation.golden_task_set/v1",
                "task_schema_version": "evaluation.benchmark_task/v1",
                "tasks": [
                    {
                        "schema_version": "evaluation.benchmark_task/v1",
                        "task_id": "private.fix_bug",
                        "version": "v1",
                        "title": "Fix bug",
                        "task_type": "repo_issue_repair",
                        "visibility": "private",
                        "adapter": "singularity_private",
                        "input": {
                            "prompt": "Fix math_utils.py.",
                            "metadata": {"allowed_paths": ["math_utils.py"]},
                        },
                        "workspace_snapshot": {
                            "kind": "inline_files",
                            "inline_files": {"math_utils.py": "def add(a, b):\n    return a - b\n"},
                        },
                        "allowed_tools": ["read_file", "write_file", "run_verification"],
                        "strategy": {"tool_policy": "read_write", "approval_mode": "auto_safe"},
                        "expected_file_changes": ["math_utils.py"],
                        "completion_standard": "Focused pytest passes.",
                        "risk_tags": ["test-repair"],
                        "expected_outcomes": [
                            {"kind": "test", "weight": 1.0, "command": f"{json.dumps(sys.executable)} -m pytest tests/test_math.py"}
                        ],
                        "tags": ["easy"],
                    },
                    {
                        "schema_version": "evaluation.benchmark_task/v1",
                        "task_id": "private.repo_issue",
                        "version": "v1",
                        "title": "Fix repo bug",
                        "task_type": "repo_issue_repair",
                        "visibility": "private",
                        "adapter": "singularity_private",
                        "input": {
                            "prompt": "Fix the repo issue.",
                            "metadata": {"repo_path": str(tmp_path / "repo"), "allowed_paths": ["src"]},
                        },
                        "workspace_snapshot": {"kind": "git_ref", "git_ref": "abc123"},
                        "expected_outcomes": [
                            {"kind": "test", "weight": 1.0, "command": "python -m pytest"}
                        ],
                        "tags": ["medium"],
                    }
                ],
            },
            indent=2,
        ),
        encoding="utf-8",
    )

    manifest = SingularityPrivateBenchmarkAdapter().load(task_set)

    assert manifest.schema_version == EVALUATION_TASK_SET_SCHEMA_VERSION
    assert manifest.tasks[0].task_id == "private.fix_bug"
    assert manifest.tasks[0].workspace.kind == "fixture"
    assert manifest.tasks[0].allowed_paths == ["math_utils.py"]
    assert manifest.tasks[0].allowed_tools == ["read_file", "run_verification", "write_file"]
    assert manifest.tasks[0].expected_file_changes == ["math_utils.py"]
    assert manifest.tasks[0].completion_standard == "Focused pytest passes."
    assert manifest.tasks[0].risk_tags == ["test-repair"]
    assert manifest.tasks[0].verification_command == f"{json.dumps(sys.executable)} -m pytest tests/test_math.py"
    assert manifest.tasks[1].task_id == "private.repo_issue"
    assert manifest.tasks[1].workspace.kind == "repo"
    assert manifest.tasks[1].workspace.path == str(tmp_path / "repo")
    assert manifest.tasks[1].workspace.start_commit == "abc123"


def test_summarize_evaluation_results_reports_cache_and_rates(tmp_path: Path) -> None:
    first = EvaluationTaskResult(
        task_id="one",
        success=True,
        tests_passed=True,
        infrastructure_blocked=False,
        prompt_tokens=100,
        cached_tokens=25,
        request_cache_hit_rate=0.25,
        run_cache_hit_rate=0.25,
        tool_calls=2,
        files_changed=["a.py"],
        duration_seconds=1.0,
        error_summary="",
        workspace=str(tmp_path),
        trace=str(tmp_path / "trace"),
        status="success",
        turn_count=2,
        agent_completed=True,
        final_report_status="completed",
    )
    second = EvaluationTaskResult(
        task_id="two",
        success=False,
        tests_passed=True,
        infrastructure_blocked=False,
        prompt_tokens=100,
        cached_tokens=75,
        request_cache_hit_rate=0.75,
        run_cache_hit_rate=0.75,
        tool_calls=3,
        files_changed=["b.py"],
        duration_seconds=2.0,
        error_summary="agent status: failed",
        workspace=str(tmp_path),
        trace=str(tmp_path / "trace2"),
        status="verification_failed",
        turn_count=3,
        agent_completed=True,
        final_report_status="completed",
    )
    blocked = EvaluationTaskResult(
        task_id="blocked",
        success=False,
        tests_passed=False,
        infrastructure_blocked=True,
        prompt_tokens=0,
        cached_tokens=0,
        request_cache_hit_rate=0.0,
        run_cache_hit_rate=0.0,
        tool_calls=0,
        files_changed=[],
        duration_seconds=0.5,
        error_summary="infrastructure blocked",
        workspace=str(tmp_path),
        trace=str(tmp_path / "trace3"),
        status="infrastructure_blocked",
        turn_count=0,
    )

    summary = summarize_evaluation_results([first, second, blocked])

    assert summary == {
        "task_count": 3,
        "scored_task_count": 2,
        "infrastructure_blocked_count": 1,
        "score_status": "scored",
        "success_count": 1,
        "task_completion_rate": 1.0,
        "tests_passed_count": 2,
        "test_pass_rate": 1.0,
        "prompt_tokens": 200,
        "cached_tokens": 100,
        "request_cache_hit_rate": 0.5,
        "run_cache_hit_rate": 0.5,
        "tool_calls": 5,
        "success_rate": 0.5,
        "verification_pass_rate": 1.0,
        "average_turns": 2.5,
        "average_tool_calls": 2.5,
        "completed_count": 2,
        "agent_completed_count": 2,
        "evaluation_passed_count": 1,
        "repair_attempt_count": 0,
        "repair_execution_count": 0,
        "policy_blocks": 0,
        "miscompletion_count": 1,
        "failure_reasons": {"verification_failed": 1},
    }


def test_summarize_evaluation_results_does_not_count_kernel_finalized_as_completed(tmp_path: Path) -> None:
    finalized_only = EvaluationTaskResult(
        task_id="kernel-finalized-blocked",
        success=False,
        tests_passed=False,
        infrastructure_blocked=False,
        prompt_tokens=10,
        cached_tokens=0,
        request_cache_hit_rate=0.0,
        run_cache_hit_rate=0.0,
        tool_calls=0,
        files_changed=[],
        duration_seconds=1.0,
        error_summary="agent status: blocked",
        workspace=str(tmp_path),
        trace=str(tmp_path / "trace"),
        status="blocked",
        final_report_status="finalized",
        completed=False,
    )
    false_completed = EvaluationTaskResult(
        task_id="completed-but-verification-failed",
        success=False,
        tests_passed=False,
        infrastructure_blocked=False,
        prompt_tokens=10,
        cached_tokens=0,
        request_cache_hit_rate=0.0,
        run_cache_hit_rate=0.0,
        tool_calls=0,
        files_changed=[],
        duration_seconds=1.0,
        error_summary="verification failed",
        workspace=str(tmp_path),
        trace=str(tmp_path / "trace2"),
        status="verification_failed",
        final_report_status="completed",
        completed=True,
    )

    summary = summarize_evaluation_results([finalized_only, false_completed])

    assert summary["completed_count"] == 1
    assert summary["miscompletion_count"] == 1
    assert summary["failure_reasons"] == {"blocked": 1, "verification_failed": 1}


def test_evaluation_runner_writes_result_without_provider(tmp_path: Path) -> None:
    py = json.dumps(sys.executable)
    manifest_payload = {
        "schema_version": EVALUATION_TASK_SET_SCHEMA_VERSION,
        "tasks": [
            {
                "task_id": "fake.write_file",
                "workspace": {"type": "fixture", "files": {"README.md": "fixture\n"}},
                "prepare_commands": [f"{py} -c \"print('ready')\""],
                "user_task": "Write done.txt with ok.",
                "allowed_paths": ["done.txt"],
                "verification_command": f"{py} -c \"from pathlib import Path; assert Path('done.txt').read_text(encoding='utf-8') == 'ok'\"",
                "success": {"type": "verification_exit_code", "exit_code": 0},
            }
        ],
    }
    manifest = EvaluationTaskSet.from_dict(manifest_payload, base_dir=tmp_path)

    class FakeTraceStore:
        run_dir = tmp_path / "trace"

    class FakeTrace:
        store = FakeTraceStore()

    class FakeGraph:
        trace = FakeTrace()

    class FakeResult:
        status = RunStatus.COMPLETED
        final_report = FinalReport(
            run_id="run_1",
            session_id="session_1",
            task_id="task_1",
            kernel_status="finalized",
            shutdown_reason="normal",
            diagnostics_count=0,
            cleanup_status="completed",
            recovered_previous_run=False,
            uncertain_transactions=[],
            workspace_lock_status="released",
            trace_summary={
                "key_artifacts": ["trace/artifacts/model.json"],
                "tool_calls": 2,
                "model_usage_summary": {
                    "requests": 2,
                    "input_tokens": 10,
                    "cached_input_tokens": 4,
                    "request_cache_hit_rates": {"req_1": 0.4},
                    "run_cache_hit_rate": 0.4,
                },
            },
            planner_summary={
                "status": "completed",
                "failure_repair_summary": {
                    "repair_attempt_count": 1,
                    "verification_contract_id": "vc_1",
                    "verification_contract_step_count": 1,
                    "verification_contract_status": "satisfied",
                    "latest_repair_contract_id": "rc_1",
                    "latest_verification_plan": ["python -m pytest tests/test_app.py"],
                    "latest_target_files": ["done.txt"],
                },
                "execution_trace_summary": {
                    "policy_denials": 2,
                    "key_artifacts": ["planner/artifact.json"],
                },
                "artifacts": ["planner/report.md"],
            },
            policy_summary={"denied_actions_count": 2},
        )

    class FakeKernel:
        graph = FakeGraph()

        def __init__(self, project_root: Path) -> None:
            self.project_root = project_root

        def run_task(self, _goal: str) -> FakeResult:
            (self.project_root / "done.txt").write_text("ok", encoding="utf-8")
            return FakeResult()

        def close_resources(self) -> None:
            return None

    class FakeBootstrap:
        def __init__(self, **kwargs) -> None:
            self.project_root = kwargs["project_root"]

        def boot(self, _goal: str) -> FakeKernel:
            return FakeKernel(self.project_root)

    result = EvaluationRunner(
        output_root=tmp_path / "out",
        run_id="run_fake",
        bootstrap_cls=FakeBootstrap,
    ).run(manifest)

    assert result["schema_version"] == EVALUATION_RESULT_SCHEMA_VERSION
    assert result["summary"]["success_count"] == 1
    task = result["tasks"][0]
    assert task["task_id"] == "fake.write_file"
    assert task["success"] is True
    assert task["tests_passed"] is True
    assert task["prompt_tokens"] == 10
    assert task["cached_tokens"] == 4
    assert task["request_cache_hit_rate"] == 0.4
    assert task["run_cache_hit_rate"] == 0.4
    assert task["status"] == "success"
    assert task["turn_count"] == 2
    assert task["tool_calls"] == 2
    assert task["files_changed"] == ["done.txt"]
    assert task["patch"]["applicable"] is True
    assert task["patch_applicable"] is True
    assert "done.txt" in task["patch"]["diff"]
    assert task["checks"]["public"]["passed"] is True
    assert task["checks"]["public"]["resolved_argv"][0] == sys.executable
    assert task["checks"]["public"]["interpreter_strategy"]["mapped_bare_python"] is False
    assert task["public_verification_passed"] is True
    assert task["hidden_verification_passed"] is True
    assert task["verification_result"]["status"] == "passed"
    assert task["agent_completed"] is True
    assert task["evaluation_passed"] is True
    assert task["completed"] is True
    assert task["contract_satisfaction"]["status"] == "satisfied"
    assert task["contract_satisfaction"]["repair_phase_contract_satisfaction"]["status"] == "not_recorded"
    assert task["final_report_status"] == "completed"
    assert task["repair_attempt_count"] == 1
    assert task["repair_execution_count"] == 0
    assert task["miscompletion_count"] == 0
    assert task["blocked_reason"] == ""
    assert task["failure_category"] == "none"
    assert task["policy_blocks"] == 2
    assert task["trace_artifact_refs"] == [
        "planner/artifact.json",
        "planner/report.md",
        "trace/artifacts/model.json",
    ]
    assert task["token_usage"]["input_tokens"] == 10
    assert task["cache_usage"]["run_cache_hit_rate"] == 0.4
    for removed in [
        "tool_call_count",
        "failure_repair_count",
        "task_verification_result",
        "repair_verification_contract",
        "result_extraction",
        "agent_loop_ref",
    ]:
        assert removed not in task
    assert task["reproducible_environment"]["workspace"]["type"] == "fixture"
    assert task["reproducible_environment"]["verification_command"] == manifest.tasks[0].verification_command
    assert task["reproducible_environment"]["public_verification_command"] == manifest.tasks[0].verification_command
    assert task["reproducible_environment"]["hidden_verification_command"] == manifest.tasks[0].verification_command
    assert task["reproducible_environment"]["runtime"]["interpreter_strategy"]["shell"] is False
    assert result["failure_case_count"] == 0
    assert Path(result["failure_cases_path"]).exists()
    env_base_url = task["reproducible_environment"]["model_profile"]["base_url"]
    assert env_base_url is None or "sk-" not in env_base_url
    assert "env_file" in task["reproducible_environment"]["model_profile"]["sources"]
    assert Path(result["result_path"]).exists()
    assert Path(result["report_path"]).exists()
    assert Path(result["markdown_path"]).exists()
    assert "Agent Evaluation" in Path(result["markdown_path"]).read_text(encoding="utf-8")


def test_evaluation_runner_compares_against_previous_run(tmp_path: Path) -> None:
    py = json.dumps(sys.executable)
    manifest = EvaluationTaskSet.from_dict(
        {
            "schema_version": EVALUATION_TASK_SET_SCHEMA_VERSION,
            "tasks": [
                {
                    "task_id": "fake.regression",
                    "workspace": {"type": "fixture", "files": {"README.md": "fixture\n"}},
                    "user_task": "Write done.txt with ok.",
                    "allowed_paths": ["done.txt"],
                    "expected_file_changes": ["done.txt"],
                    "verification_command": f"{py} -c \"from pathlib import Path; assert Path('done.txt').read_text(encoding='utf-8') == 'ok'\"",
                    "success": {"type": "verification_exit_code", "exit_code": 0},
                }
            ],
        },
        base_dir=tmp_path,
    )

    class FakeTraceStore:
        run_dir = tmp_path / "trace"

    class FakeTrace:
        store = FakeTraceStore()

    class FakeGraph:
        trace = FakeTrace()

    class FakeResult:
        status = RunStatus.COMPLETED
        final_report = FinalReport(
            run_id="run_1",
            session_id="session_1",
            task_id="task_1",
            kernel_status="finalized",
            shutdown_reason="normal",
            diagnostics_count=0,
            cleanup_status="completed",
            recovered_previous_run=False,
            uncertain_transactions=[],
            workspace_lock_status="released",
            trace_summary={"tool_calls": 1, "model_usage_summary": {"requests": 1, "input_tokens": 10}},
        )

    class FakeKernel:
        graph = FakeGraph()

        def __init__(self, project_root: Path, should_write: bool) -> None:
            self.project_root = project_root
            self.should_write = should_write

        def run_task(self, _goal: str) -> FakeResult:
            if self.should_write:
                (self.project_root / "done.txt").write_text("ok", encoding="utf-8")
            return FakeResult()

        def close_resources(self) -> None:
            return None

    class FakeBootstrap:
        should_write = True

        def __init__(self, **kwargs) -> None:
            self.project_root = kwargs["project_root"]

        def boot(self, _goal: str) -> FakeKernel:
            return FakeKernel(self.project_root, self.should_write)

    output_root = tmp_path / "out"
    first = EvaluationRunner(
        output_root=output_root,
        run_id="baseline",
        bootstrap_cls=FakeBootstrap,
    ).run(manifest)
    FakeBootstrap.should_write = False
    second = EvaluationRunner(
        output_root=output_root,
        run_id="candidate",
        bootstrap_cls=FakeBootstrap,
    ).run(manifest)

    assert first["summary"]["success_count"] == 1
    assert second["summary"]["success_count"] == 0
    assert second["regression"]["summary"]["regression_count"] == 1
    assert second["regression"]["task_diffs"][0]["task_id"] == "fake.regression"
    assert Path(second["regression_path"]).exists()
    assert Path(second["regression_markdown_path"]).exists()


def test_evaluation_maps_bare_python_verification_to_harness_executable(tmp_path: Path) -> None:
    manifest = EvaluationTaskSet.from_dict(
        {
            "schema_version": EVALUATION_TASK_SET_SCHEMA_VERSION,
            "tasks": [
                {
                    "task_id": "fake.bare_python",
                    "workspace": {"type": "fixture", "files": {"README.md": "fixture\n"}},
                    "user_task": "Write done.txt with ok.",
                    "allowed_paths": ["done.txt"],
                    "expected_file_changes": ["done.txt"],
                    "verification_command": "python -c \"from pathlib import Path; assert Path('done.txt').read_text(encoding='utf-8') == 'ok'\"",
                    "success": {"type": "verification_exit_code", "exit_code": 0},
                }
            ],
        },
        base_dir=tmp_path,
    )

    class FakeTraceStore:
        run_dir = tmp_path / "trace"

    class FakeTrace:
        store = FakeTraceStore()

    class FakeGraph:
        trace = FakeTrace()

    class FakeResult:
        status = RunStatus.COMPLETED
        final_report = FinalReport(
            run_id="run_1",
            session_id="session_1",
            task_id="task_1",
            kernel_status="finalized",
            shutdown_reason="normal",
            diagnostics_count=0,
            cleanup_status="completed",
            recovered_previous_run=False,
            uncertain_transactions=[],
            workspace_lock_status="released",
            trace_summary={"tool_calls": 1, "model_usage_summary": {"requests": 1, "input_tokens": 10}},
            planner_summary={"status": "completed"},
        )

    class FakeKernel:
        graph = FakeGraph()

        def __init__(self, project_root: Path) -> None:
            self.project_root = project_root

        def run_task(self, _goal: str) -> FakeResult:
            (self.project_root / "done.txt").write_text("ok", encoding="utf-8")
            return FakeResult()

        def close_resources(self) -> None:
            return None

    class FakeBootstrap:
        def __init__(self, **kwargs) -> None:
            self.project_root = kwargs["project_root"]

        def boot(self, _goal: str) -> FakeKernel:
            return FakeKernel(self.project_root)

    result = EvaluationRunner(
        output_root=tmp_path / "out",
        run_id="bare_python",
        bootstrap_cls=FakeBootstrap,
    ).run(manifest)

    task = result["tasks"][0]
    assert task["success"] is True
    assert task["checks"]["public"]["resolved_argv"][0] == sys.executable
    assert task["checks"]["public"]["interpreter_strategy"]["mapped_bare_python"] is True
    assert task["checks"]["hidden"]["resolved_argv"][0] == sys.executable
    assert task["verification"]["failure_category"] == "none"


def test_evaluation_reports_completion_rejected_repair_and_verification_contract(tmp_path: Path) -> None:
    py = json.dumps(sys.executable)
    manifest = EvaluationTaskSet.from_dict(
        {
            "schema_version": EVALUATION_TASK_SET_SCHEMA_VERSION,
            "tasks": [
                {
                    "task_id": "fake.completion_repair",
                    "workspace": {"type": "fixture", "files": {"app.py": "def answer():\n    return 0\n"}},
                    "user_task": "Repair app.answer and do not finish before verification evidence exists.",
                    "allowed_paths": ["app.py"],
                    "expected_file_changes": ["app.py"],
                    "verification_command": f"{py} -c \"from app import answer; assert answer() == 42\"",
                    "completion_standard": "Premature completion is rejected, repair plan records a verification contract, and verification passes.",
                    "risk_tags": ["completion-rejected-repair", "verification-contract"],
                    "success": {"type": "verification_exit_code", "exit_code": 0},
                }
            ],
        },
        base_dir=tmp_path,
    )

    class FakeTraceStore:
        run_dir = tmp_path / "trace"

    class FakeTrace:
        store = FakeTraceStore()

    class FakeGraph:
        trace = FakeTrace()

    class FakeResult:
        status = RunStatus.COMPLETED
        final_report = FinalReport(
            run_id="run_1",
            session_id="session_1",
            task_id="task_1",
            kernel_status="finalized",
            shutdown_reason="normal",
            diagnostics_count=0,
            cleanup_status="completed",
            recovered_previous_run=False,
            uncertain_transactions=[],
            workspace_lock_status="released",
            trace_summary={
                "tool_calls": 4,
                "key_artifacts": ["trace/final-review.json"],
                "model_usage_summary": {"requests": 3, "input_tokens": 120, "cached_input_tokens": 20},
            },
            planner_summary={
                "status": "completed",
                "verification_summary": {"status": "ready"},
                "failure_repair_summary": {
                    "failure_analysis_count": 1,
                    "repair_plan_count": 1,
                    "repair_attempt_count": 1,
                    "latest_failure_category": "completion_stalled",
                    "latest_repair_strategy": "repair_then_verify",
                    "latest_repair_contract_id": "repair_contract_1",
                    "latest_verification_plan": [f"{py} -c \"from app import answer; assert answer() == 42\""],
                    "latest_target_files": ["app.py"],
                    "verification_contract_id": "verification_contract_1",
                    "verification_contract_step_count": 1,
                    "verification_contract_status": "satisfied",
                    "verification_contract_validation_errors": [],
                },
                "contract_satisfaction": {
                    "contract_id": "verification_contract_1",
                    "satisfied": True,
                    "completed_steps": ["step_1"],
                    "failed_steps": [],
                    "reason": None,
                },
                "execution_trace_summary": {
                    "policy_denials": 0,
                    "key_artifacts": ["planner/repair-plan.json"],
                },
            },
        )

    class FakeKernel:
        graph = FakeGraph()

        def __init__(self, project_root: Path) -> None:
            self.project_root = project_root

        def run_task(self, _goal: str) -> FakeResult:
            (self.project_root / "app.py").write_text("def answer():\n    return 42\n", encoding="utf-8")
            return FakeResult()

        def close_resources(self) -> None:
            return None

    class FakeBootstrap:
        def __init__(self, **kwargs) -> None:
            self.project_root = kwargs["project_root"]

        def boot(self, _goal: str) -> FakeKernel:
            return FakeKernel(self.project_root)

    result = EvaluationRunner(
        output_root=tmp_path / "out",
        run_id="completion_repair",
        bootstrap_cls=FakeBootstrap,
    ).run(manifest)

    task = result["tasks"][0]
    assert task["success"] is True
    assert task["repair_attempt_count"] == 1
    assert task["repair_execution_count"] == 0
    assert task["turn_count"] == 3
    assert task["tool_calls"] == 4
    assert task["contract_satisfaction"]["status"] == "satisfied"
    assert task["contract_satisfaction"]["repair_phase_contract_satisfaction"] == {
        "status": "satisfied",
        "source": "kernel.final_report.planner_summary.contract_satisfaction",
        "contract_id": "verification_contract_1",
        "completed_steps": ["step_1"],
        "failed_steps": [],
        "skipped_steps": [],
        "reason": None,
    }
    assert task["trace_artifact_refs"] == ["planner/repair-plan.json", "trace/final-review.json"]
    report_text = Path(result["markdown_path"]).read_text(encoding="utf-8")
    assert "completion_repair" in report_text
    result_text = Path(result["result_path"]).read_text(encoding="utf-8")
    assert "verification_contract_1" in result_text
    assert "repair_verification_contract" not in result_text


def test_evaluation_uses_env_root_for_fixture_workspace_config(
    monkeypatch,
    tmp_path: Path,
) -> None:
    monkeypatch.delenv("SINGULARITY_API_KEY", raising=False)
    monkeypatch.delenv("SINGULARITY_BASE_URL", raising=False)
    monkeypatch.delenv("SINGULARITY_MODEL", raising=False)
    env_root = tmp_path / "repo"
    env_root.mkdir()
    (env_root / ".env").write_text(
        "\n".join(
            [
                "SINGULARITY_API_KEY=sk-local-test-secret",
                "SINGULARITY_BASE_URL=https://provider.example/v1",
                "SINGULARITY_MODEL=env-root-model",
            ]
        )
        + "\n",
        encoding="utf-8",
    )
    py = json.dumps(sys.executable)
    manifest = EvaluationTaskSet.from_dict(
        {
            "schema_version": EVALUATION_TASK_SET_SCHEMA_VERSION,
            "tasks": [
                {
                    "task_id": "fake.env_root",
                    "workspace": {"type": "fixture", "files": {"README.md": "fixture\n"}},
                    "user_task": "Write done.txt.",
                    "allowed_paths": ["done.txt"],
                    "verification_command": f"{py} -c \"from pathlib import Path; assert Path('done.txt').read_text(encoding='utf-8') == 'ok'\"",
                    "success": {"type": "verification_exit_code", "exit_code": 0},
                }
            ],
        },
        base_dir=tmp_path,
    )

    class FakeTraceStore:
        run_dir = tmp_path / "trace"

    class FakeTrace:
        store = FakeTraceStore()

    class FakeGraph:
        trace = FakeTrace()

    class FakeResult:
        status = RunStatus.COMPLETED
        final_report = FinalReport(
            run_id="run_1",
            session_id="session_1",
            task_id="task_1",
            kernel_status="finalized",
            shutdown_reason="normal",
            diagnostics_count=0,
            cleanup_status="completed",
            recovered_previous_run=False,
            uncertain_transactions=[],
            workspace_lock_status="released",
            trace_summary={"tool_calls": 1, "model_usage_summary": {"requests": 1, "input_tokens": 10}},
        )

    class FakeKernel:
        graph = FakeGraph()

        def __init__(self, project_root: Path) -> None:
            self.project_root = project_root

        def run_task(self, _goal: str) -> FakeResult:
            (self.project_root / "done.txt").write_text("ok", encoding="utf-8")
            return FakeResult()

        def close_resources(self) -> None:
            return None

    class FakeBootstrap:
        def __init__(self, **kwargs) -> None:
            self.project_root = kwargs["project_root"]
            self.config = kwargs["config"]

        def boot(self, _goal: str) -> FakeKernel:
            assert self.config.model == "env-root-model"
            assert self.config.base_url == "https://provider.example/v1"
            return FakeKernel(self.project_root)

    result = EvaluationRunner(
        output_root=tmp_path / "out",
        run_id="env_root",
        env_root=env_root,
        bootstrap_cls=FakeBootstrap,
    ).run(manifest)

    task = result["tasks"][0]
    env = task["reproducible_environment"]["model_profile"]
    assert env["model"] == "env-root-model"
    assert env["base_url"] == "https://provider.example/v1"
    assert env["sources"]["model"] == "env:SINGULARITY_MODEL"
    assert env["sources"]["base_url"] == "env:SINGULARITY_BASE_URL"
    assert env["sources"]["env_file"].replace("\\", "/").endswith("/repo/.env")
    assert "sk-local-test-secret" not in json.dumps(result)


def test_evaluation_runner_can_drive_real_agent_loop(tmp_path: Path) -> None:
    py = json.dumps(sys.executable)
    manifest = EvaluationTaskSet.from_dict(
        {
            "schema_version": EVALUATION_TASK_SET_SCHEMA_VERSION,
            "tasks": [
                {
                    "task_id": "fake.real_agent_loop",
                    "workspace": {"type": "fixture", "files": {"README.md": "fixture\n"}},
                    "user_task": "Say done without modifying files.",
                    "allowed_paths": ["."],
                    "verification_command": f"{py} -c \"print('ok')\"",
                    "success": {"type": "verification_exit_code", "exit_code": 0},
                }
            ],
        },
        base_dir=tmp_path,
    )
    calls: list[str] = []

    class MockProvider:
        def chat(self, *, messages: list[dict[str, Any]], tools: list[dict[str, Any]]) -> dict[str, Any]:
            _ = messages, tools
            calls.append("chat")
            return {"choices": [{"message": {"role": "assistant", "content": "done"}}]}

    class FakeKernel:
        graph = None

        def __init__(self, project_root: Path) -> None:
            self.project_root = project_root

        def run_task(self, goal: str):
            agent = make_agent_session(
                self.project_root,
                provider=MockProvider(),
                max_turns=1,
            )
            agent_result = agent.run(goal)

            class FakeGraph:
                trace = agent.trace

            self.graph = FakeGraph()

            class Result:
                status = RunStatus.COMPLETED
                final_report = FinalReport(
                    run_id="run_1",
                    session_id="session_1",
                    task_id="task_1",
                    kernel_status="finalized",
                    shutdown_reason="normal",
                    diagnostics_count=0,
                        cleanup_status="completed",
                        recovered_previous_run=False,
                        uncertain_transactions=[],
                        workspace_lock_status="released",
                        trace_summary={"tool_calls": 0, "model_usage_summary": {"requests": 1, "input_tokens": 1}},
                    )
                final_answer = agent_result.final_answer

            return Result()

        def close_resources(self) -> None:
            return None

    class FakeBootstrap:
        def __init__(self, **kwargs) -> None:
            self.project_root = kwargs["project_root"]

        def boot(self, _goal: str) -> FakeKernel:
            return FakeKernel(self.project_root)

    result = EvaluationRunner(
        output_root=tmp_path / "out",
        run_id="real_loop",
        bootstrap_cls=FakeBootstrap,
    ).run(manifest)

    assert calls == ["chat"]
    assert result["tasks"][0]["success"] is True
    assert result["tasks"][0]["agent_completed"] is True
    assert "agent_loop_ref" not in result["tasks"][0]


def test_evaluation_prepare_failure_returns_structured_result(tmp_path: Path) -> None:
    py = json.dumps(sys.executable)
    manifest = EvaluationTaskSet.from_dict(
        {
            "schema_version": EVALUATION_TASK_SET_SCHEMA_VERSION,
            "tasks": [
                {
                    "task_id": "fake.prepare_failure",
                    "workspace": {"type": "fixture", "files": {"README.md": "fixture\n"}},
                    "prepare_commands": [f"{py} -c \"raise SystemExit(3)\""],
                    "user_task": "Write done.txt.",
                    "allowed_paths": ["done.txt"],
                    "verification_command": f"{py} -c \"print('unused')\"",
                    "success": {"type": "verification_exit_code", "exit_code": 0},
                }
            ],
        },
        base_dir=tmp_path,
    )

    result = EvaluationRunner(
        output_root=tmp_path / "out",
        run_id="run_prepare_failed",
        bootstrap_cls=object,
    ).run(manifest)

    task = result["tasks"][0]
    assert task["success"] is False
    assert task["verification"]["exit_code"] == 3
    assert task["patch"]["applicable"] is False
    assert task["checks"]["hidden"]["status"] == "failed"
    assert "prepare failed" in task["error_summary"]


def test_evaluation_applies_patch_in_clean_verification_workspace(tmp_path: Path) -> None:
    py = json.dumps(sys.executable)
    manifest = EvaluationTaskSet.from_dict(
        {
            "schema_version": EVALUATION_TASK_SET_SCHEMA_VERSION,
            "tasks": [
                {
                    "task_id": "fake.clean_apply",
                    "workspace": {"type": "fixture", "files": {"solution.py": "def answer():\n    return 0\n"}},
                    "user_task": "Fix solution.answer.",
                    "allowed_paths": ["solution.py"],
                    "verification_prepare_commands": [
                        f"{py} -c \"from pathlib import Path; Path('tests').mkdir(exist_ok=True); Path('tests/test_solution.py').write_text('from solution import answer\\n\\ndef test_answer():\\n    assert answer() == 42\\n', encoding='utf-8')\""
                    ],
                    "verification_command": f"{py} -m pytest tests/test_solution.py",
                    "success": {"type": "verification_exit_code", "exit_code": 0},
                }
            ],
        },
        base_dir=tmp_path,
    )

    class FakeTraceStore:
        run_dir = tmp_path / "trace"

    class FakeTrace:
        store = FakeTraceStore()

    class FakeGraph:
        trace = FakeTrace()

    class FakeResult:
        status = RunStatus.COMPLETED
        final_report = FinalReport(
            run_id="run_1",
            session_id="session_1",
            task_id="task_1",
            kernel_status="finalized",
            shutdown_reason="normal",
            diagnostics_count=0,
            cleanup_status="completed",
            recovered_previous_run=False,
            uncertain_transactions=[],
            workspace_lock_status="released",
            trace_summary={"tool_calls": 1, "model_usage_summary": {"input_tokens": 10}},
        )

    class FakeKernel:
        graph = FakeGraph()

        def __init__(self, project_root: Path) -> None:
            self.project_root = project_root

        def run_task(self, _goal: str) -> FakeResult:
            (self.project_root / "solution.py").write_text("def answer():\n    return 42\n", encoding="utf-8")
            return FakeResult()

        def close_resources(self) -> None:
            return None

    class FakeBootstrap:
        def __init__(self, **kwargs) -> None:
            self.project_root = kwargs["project_root"]

        def boot(self, _goal: str) -> FakeKernel:
            return FakeKernel(self.project_root)

    result = EvaluationRunner(
        output_root=tmp_path / "out",
        run_id="run_apply",
        bootstrap_cls=FakeBootstrap,
    ).run(manifest)

    task = result["tasks"][0]
    assert task["success"] is True
    assert task["workspace"] != task["verification_workspace"]
    assert Path(task["verification_workspace"], "tests", "test_solution.py").exists()
    assert task["patch"]["applicable"] is True
    assert task["checks"]["hidden"]["passed"] is True
    assert task["checks"]["public"]["passed"] is True


def test_evaluation_marks_model_transport_blocker_without_running_verification(tmp_path: Path) -> None:
    py = json.dumps(sys.executable)
    manifest = EvaluationTaskSet.from_dict(
        {
            "schema_version": EVALUATION_TASK_SET_SCHEMA_VERSION,
            "tasks": [
                {
                    "task_id": "fake.model_blocked",
                    "workspace": {"type": "fixture", "files": {"README.md": "fixture\n"}},
                    "user_task": "Write done.txt with ok.",
                    "allowed_paths": ["done.txt"],
                    "verification_command": f"{py} -c \"raise SystemExit(99)\"",
                    "success": {"type": "verification_exit_code", "exit_code": 0},
                }
            ],
        },
        base_dir=tmp_path,
    )

    class FakeTraceStore:
        run_dir = tmp_path / "trace"

    class FakeTrace:
        store = FakeTraceStore()

    class FakeGraph:
        trace = FakeTrace()

    class FakeResult:
        status = RunStatus.FAILED
        final_answer = "[WinError 10013] socket access denied"
        final_report = FinalReport(
            run_id="run_1",
            session_id="session_1",
            task_id="task_1",
            kernel_status="finalized",
            shutdown_reason="error",
            diagnostics_count=1,
            cleanup_status="completed",
            recovered_previous_run=False,
            uncertain_transactions=[],
            workspace_lock_status="released",
            trace_summary={"model_usage_summary": {"input_tokens": 0}},
        )

    class FakeKernel:
        graph = FakeGraph()

        def run_task(self, _goal: str) -> FakeResult:
            return FakeResult()

        def close_resources(self) -> None:
            return None

    class FakeBootstrap:
        def __init__(self, **kwargs) -> None:
            _ = kwargs

        def boot(self, _goal: str) -> FakeKernel:
            return FakeKernel()

    result = EvaluationRunner(
        output_root=tmp_path / "out",
        run_id="run_blocked",
        bootstrap_cls=FakeBootstrap,
    ).run(manifest)

    assert result["summary"]["infrastructure_blocked_count"] == 1
    assert result["summary"]["scored_task_count"] == 0
    assert result["summary"]["score_status"] == "infrastructure_blocked"
    assert result["summary"]["task_completion_rate"] == 0.0
    assert result["summary"]["test_pass_rate"] == 0.0
    task = result["tasks"][0]
    assert task["infrastructure_blocked"] is True
    assert task["verification"] is None
    assert "infrastructure blocked" in task["error_summary"]


def test_evaluation_completion_gate_counts_false_completed_report(tmp_path: Path) -> None:
    py = json.dumps(sys.executable)
    manifest = EvaluationTaskSet.from_dict(
        {
            "schema_version": EVALUATION_TASK_SET_SCHEMA_VERSION,
            "tasks": [
                {
                    "task_id": "fake.false_completed",
                    "task_type": "completion_gate",
                    "workspace": {"type": "fixture", "files": {"status.py": "VALUE = 'draft'\n"}},
                    "user_task": "Change status.py so VALUE is ready and verify it.",
                    "allowed_paths": ["status.py"],
                    "expected_file_changes": ["status.py"],
                    "verification_command": f"{py} -c \"from status import VALUE; assert VALUE == 'ready'\"",
                    "completion_standard": "Completion requires the expected file change and a passing verification command.",
                    "risk_tags": ["completion-gate", "missing-evidence-risk"],
                    "success": {"type": "verification_exit_code", "exit_code": 0},
                }
            ],
        },
        base_dir=tmp_path,
    )

    class FakeTraceStore:
        run_dir = tmp_path / "trace"

    class FakeTrace:
        store = FakeTraceStore()

    class FakeGraph:
        trace = FakeTrace()

    class FakeResult:
        status = RunStatus.COMPLETED
        final_report = FinalReport(
            run_id="run_1",
            session_id="session_1",
            task_id="task_1",
            kernel_status="finalized",
            shutdown_reason="normal",
            diagnostics_count=0,
            cleanup_status="completed",
            recovered_previous_run=False,
            uncertain_transactions=[],
            workspace_lock_status="released",
            trace_summary={"tool_calls": 0, "model_usage_summary": {"requests": 1, "input_tokens": 10}},
            planner_summary={"status": "completed"},
        )

    class FakeKernel:
        graph = FakeGraph()

        def run_task(self, _goal: str) -> FakeResult:
            return FakeResult()

        def close_resources(self) -> None:
            return None

    class FakeBootstrap:
        def __init__(self, **kwargs) -> None:
            _ = kwargs

        def boot(self, _goal: str) -> FakeKernel:
            return FakeKernel()

    result = EvaluationRunner(
        output_root=tmp_path / "out",
        run_id="false_completed",
        bootstrap_cls=FakeBootstrap,
    ).run(manifest)

    task = result["tasks"][0]
    assert task["success"] is False
    assert task["completed"] is True
    assert task["miscompletion_count"] == 1
    assert result["summary"]["miscompletion_count"] == 1
    assert task["patch_applicable"] is False
    assert task["public_verification_passed"] is False
    assert task["hidden_verification_passed"] is False
    assert task["contract_satisfaction"]["status"] == "unsatisfied"
    assert any(
        check["name"] == "expected_file_changes" and check["passed"] is False
        for check in task["contract_satisfaction"]["checks"]
    )
    assert task["failure_category"] == "command_failed"


def test_failure_case_replay_runner_extracts_evaluation_failure_record(tmp_path: Path) -> None:
    trace_dir = tmp_path / "trace"
    trace_dir.mkdir()
    (trace_dir / "events.jsonl").write_text(
        "\n".join(
            [
                json.dumps(
                    {
                        "event_type": "action.proposed",
                        "summary": "write_file is not allowed in phase running_verification.",
                        "payload": {
                            "phase": "running_verification",
                            "reason": "write_file is not allowed in phase running_verification.",
                        },
                    }
                ),
                json.dumps(
                    {
                        "event_type": "final_report.completed",
                        "summary": "Final report completed: blocked.",
                        "payload": {
                            "final_report": {
                                "outcome": "blocked",
                                "blocked_reasons": [
                                    "write_file is not allowed in phase running_verification."
                                ],
                            }
                        },
                    }
                ),
            ]
        )
        + "\n",
        encoding="utf-8",
    )
    report_path = tmp_path / "report.json"
    report_path.write_text(
        json.dumps(
            {
                "tasks": [
                    {
                        "task_id": "benchmark.regression.multi_file_reasoning",
                        "status": "verification_failed",
                        "success": False,
                        "failure_category": "verification_failed",
                        "miscompletion_count": 1,
                        "public_verification_passed": False,
                        "hidden_verification_passed": False,
                        "policy_blocks": 1,
                        "files_changed": ["policy.py"],
                        "final_report_status": "finalized",
                        "repair_attempt_count": 0,
                        "repair_execution_count": 0,
                        "blocked_reason": "agent status: blocked",
                        "trace": str(trace_dir),
                        "trace_artifact_refs": ["artifact_prompt"],
                        "contract_satisfaction": {"status": "unsatisfied"},
                        "verification_result": {"status": "failed"},
                        "reproducible_environment": {
                            "expected_file_changes": ["cart.py", "policy.py"]
                        },
                    }
                ]
            },
            ensure_ascii=False,
        ),
        encoding="utf-8",
    )

    output_path = tmp_path / "failure_cases.json"
    records = FailureCaseReplayRunner(report_path=report_path).write(output_path)

    assert len(records) == 1
    record = records[0].to_dict()
    assert record["schema_version"] == FAILURE_CASE_RECORD_SCHEMA_VERSION
    assert record["task_id"] == "benchmark.regression.multi_file_reasoning"
    assert record["expected_file_changes"] == ["cart.py", "policy.py"]
    assert record["files_changed"] == ["policy.py"]
    assert record["repair_attempt_count"] == 0
    assert record["trace_summary"]["final_report_outcome"] == "blocked"
    assert record["trace_summary"]["phase_policy_blocks"][0]["phase"] == "running_verification"
    saved = json.loads(output_path.read_text(encoding="utf-8"))
    assert saved["runner_mode"] == "post_run_failure_extraction"
    assert saved["targeted_replay_runner"] == "TargetedFailureReplayRunner"
    assert saved["failure_count"] == 1
    assert "entered_agent_loop" not in saved
    assert "targeted_replay_result" not in saved
    assert "phase_history" not in saved["records"][0]


def test_evaluation_runs_hidden_verification_prepare_after_agent(tmp_path: Path) -> None:
    py = json.dumps(sys.executable)
    hidden_test = "from solution import answer\n\n\ndef test_answer():\n    assert answer() == 42\n"
    hidden_source = tmp_path / "hidden_test_source.py"
    hidden_source.write_text(hidden_test, encoding="utf-8")
    hidden_code = (
        "from pathlib import Path; "
        "Path('tests').mkdir(exist_ok=True); "
        f"Path('tests/test_hidden.py').write_text(Path({str(hidden_source)!r}).read_text(encoding='utf-8'), encoding='utf-8')"
    )
    manifest = EvaluationTaskSet.from_dict(
        {
            "schema_version": EVALUATION_TASK_SET_SCHEMA_VERSION,
            "tasks": [
                {
                    "task_id": "fake.hidden_verification",
                    "workspace": {"type": "fixture", "files": {"README.md": "fixture\n"}},
                    "user_task": "Create solution.py with answer().",
                    "allowed_paths": ["solution.py"],
                    "verification_prepare_commands": [f"{py} -c {json.dumps(hidden_code)}"],
                    "verification_command": f"{py} -m pytest tests/test_hidden.py",
                    "success": {"type": "verification_exit_code", "exit_code": 0},
                }
            ],
        },
        base_dir=tmp_path,
    )
    seen_goals: list[str] = []

    class FakeTraceStore:
        run_dir = tmp_path / "trace"

    class FakeTrace:
        store = FakeTraceStore()

    class FakeGraph:
        trace = FakeTrace()

    class FakeResult:
        status = RunStatus.COMPLETED
        final_report = FinalReport(
            run_id="run_1",
            session_id="session_1",
            task_id="task_1",
            kernel_status="finalized",
            shutdown_reason="normal",
            diagnostics_count=0,
            cleanup_status="completed",
            recovered_previous_run=False,
            uncertain_transactions=[],
            workspace_lock_status="released",
            trace_summary={"tool_calls": 1, "model_usage_summary": {"input_tokens": 10}},
        )

    class FakeKernel:
        graph = FakeGraph()

        def __init__(self, project_root: Path) -> None:
            self.project_root = project_root

        def run_task(self, goal: str) -> FakeResult:
            seen_goals.append(goal)
            (self.project_root / "solution.py").write_text("def answer():\n    return 42\n", encoding="utf-8")
            return FakeResult()

        def close_resources(self) -> None:
            return None

    class FakeBootstrap:
        def __init__(self, **kwargs) -> None:
            self.project_root = kwargs["project_root"]

        def boot(self, _goal: str) -> FakeKernel:
            return FakeKernel(self.project_root)

    result = EvaluationRunner(
        output_root=tmp_path / "out",
        run_id="run_hidden",
        bootstrap_cls=FakeBootstrap,
    ).run(manifest)

    assert result["summary"]["success_count"] == 1
    task = result["tasks"][0]
    assert task["success"] is True
    assert task["files_changed"] == ["solution.py"]
    assert "tests/test_hidden.py" not in seen_goals[0]


