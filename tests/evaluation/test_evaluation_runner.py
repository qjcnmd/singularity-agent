from __future__ import annotations

import json
import sys
from pathlib import Path
from typing import Any, ClassVar

import pytest

import singularity.evaluation.runner as evaluation_runner
from singularity.evaluation.failure_case_replay import FailureCaseReplayRunner
from singularity.evaluation.models import FAILURE_CASE_RECORD_SCHEMA_VERSION
from singularity.evaluation.runner import (
    EVALUATION_RESULT_SCHEMA_VERSION,
    EVALUATION_TASK_SET_SCHEMA_VERSION,
    CommandEvalResult,
    EvaluationRunner,
    EvaluationTask,
    EvaluationTaskResult,
    EvaluationTaskSet,
    EvaluationWorkspace,
    SingularityPrivateBenchmarkAdapter,
    _apply_benchmark_constraints,
    _apply_test_patch,
    _build_capability_summary,
    _command_failure_category,
    _expected_file_changes_satisfied,
    _failure_category,
    _provider_time_seconds,
    _result_status,
    _task_goal,
    evaluation_report_markdown,
    load_evaluation_task_set,
    summarize_evaluation_results,
)
from singularity.kernel.finalization import FinalReport
from singularity.kernel.models import RunStatus
from tests.agent_loop_helpers import make_agent_session


def test_load_evaluation_task_set_rejects_unsupported_schema(tmp_path: Path) -> None:
    task_set = tmp_path / "old-entry-task-set.json"
    old_schema = "evaluation." + "live" + "_agent_task_set/v1"
    task_set.write_text(
        json.dumps(
            {
                "schema_version": old_schema,
                "tasks": [
                    {
                        "task_id": "old.entry",
                        "workspace": {"type": "fixture", "files": {"README.md": "fixture\n"}},
                        "user_task": "Say done.",
                        "allowed_paths": ["."],
                        "verification_command": f"{json.dumps(sys.executable)} -c \"print('ok')\"",
                        "success": {"type": "verification_exit_code", "exit_code": 0},
                    }
                ],
            }
        ),
        encoding="utf-8",
    )

    with pytest.raises(ValueError, match="Unsupported evaluation schema_version"):
        load_evaluation_task_set(task_set)


def test_load_public_representative_task_manifest_is_public_swe_bench() -> None:
    manifest = load_evaluation_task_set(Path("docs/evaluation/public-representative-task.json"))

    assert manifest.schema_version == EVALUATION_TASK_SET_SCHEMA_VERSION
    assert len(manifest.tasks) == 1
    task = manifest.tasks[0]
    assert task.task_id == "sqlfluff__sqlfluff-2419"
    assert task.task_type == "public_representative"
    assert task.workspace.kind == "repo"
    assert task.workspace.path == "https://github.com/sqlfluff/sqlfluff.git"
    assert task.workspace.start_commit == "f1dba0e1dd764ae72d67c3d5e1471cf14d3db030"
    assert task.fixture_metadata["adapter"] == "swe_bench"
    assert task.fixture_metadata["instance_id"] == "sqlfluff__sqlfluff-2419"
    assert task.fixture_metadata["fail_to_pass"] == [
        "test/rules/std_L060_test.py::test__rules__std_L060_raised"
    ]
    assert task.allowed_paths == ["src/sqlfluff/rules/L060.py"]
    assert task.expected_file_changes == ["src/sqlfluff/rules/L060.py"]
    assert "edit_apply" in task.allowed_tools
    assert "workspace_replace_text" not in task.allowed_tools
    assert "test__rules__std_L060_raised" in task.test_patch
    assert "gold_patch" not in task.fixture_metadata
    assert task.hidden_test_patch["source"] == "swe_bench_lite.dev"
    assert task.hidden_test_patch["fixture_owner"] == "evaluator"
    assert task.prepare_commands
    assert "PYTHONPATH" in task.public_verification_command
    assert "os.path.abspath('.')" in task.public_verification_command
    assert task.hidden_verification_command == task.public_verification_command
    assert "src/sqlfluff/rules/L060.py" in task.model_visible_verification_command
    assert "std_L060_test" not in task.model_visible_verification_command
    assert "test_patch" not in task.model_visible_verification_command
    assert "FAIL_TO_PASS" not in task.model_visible_verification_command
    assert ".eval-venv" not in task.model_visible_verification_command
    goal = _task_goal(task)
    assert "SQLFluff" in goal
    assert "src/sqlfluff/rules/L060.py" in goal
    assert "std_L060_test" not in goal
    assert "FAIL_TO_PASS" not in goal
    assert "hidden_test_patch" not in goal
    assert "test_patch" not in goal
    assert "Use 'COALESCE' instead of 'IFNULL'." in goal
    assert "std_L060_test" not in goal
    assert "gold" not in goal.lower()
    assert ".eval-venv" not in goal
    assert "pip install" not in goal

    class FakePlanner:
        def __init__(self) -> None:
            self.constraints: dict[str, Any] = {}

        def apply_benchmark_constraints(self, payload: dict[str, Any]) -> None:
            self.constraints = payload

    class FakeGraph:
        def __init__(self) -> None:
            self.planner = FakePlanner()

    class FakeKernel:
        def __init__(self) -> None:
            self.graph = FakeGraph()

    kernel = FakeKernel()
    _apply_benchmark_constraints(kernel, task)
    constraints_json = json.dumps(kernel.graph.planner.constraints, ensure_ascii=False)
    assert "src/sqlfluff/rules/L060.py" in constraints_json
    assert "std_L060_test" not in constraints_json
    assert "test_patch" not in constraints_json
    assert "FAIL_TO_PASS" not in constraints_json
    assert ".eval-venv" not in constraints_json


def test_evaluation_hidden_fixture_metadata_never_enters_goal_or_constraints(tmp_path: Path) -> None:
    sentinel = "PHASE8_SENTINEL_DO_NOT_LEAK"
    manifest = EvaluationTaskSet.from_dict(
        {
            "schema_version": EVALUATION_TASK_SET_SCHEMA_VERSION,
            "tasks": [
                {
                    "task_id": "fake.hidden_fixture",
                    "workspace": {"type": "fixture", "files": {"README.md": "fixture\n"}},
                    "user_task": "Update README.md.",
                    "allowed_paths": ["README.md"],
                    "verification_command": f"{json.dumps(sys.executable)} -c \"print('ok')\"",
                    "success": {"type": "verification_exit_code", "exit_code": 0},
                    "fixture_metadata": {
                        "patch": sentinel,
                        "test_patch": sentinel,
                        "gold_patch": sentinel,
                        "FAIL_TO_PASS": sentinel,
                        "PASS_TO_PASS": sentinel,
                    },
                    "hidden_test_patch": {
                        "content": sentinel,
                        "sha256": sentinel,
                    },
                    "test_patch": sentinel,
                }
            ],
        },
        base_dir=tmp_path,
    )
    task = manifest.tasks[0]

    class FakePlanner:
        def __init__(self) -> None:
            self.constraints: dict[str, Any] = {}

        def apply_benchmark_constraints(self, payload: dict[str, Any]) -> None:
            self.constraints = payload

    class FakeGraph:
        def __init__(self) -> None:
            self.planner = FakePlanner()

    class FakeKernel:
        def __init__(self) -> None:
            self.graph = FakeGraph()

    kernel = FakeKernel()
    goal = _task_goal(task)
    _apply_benchmark_constraints(kernel, task)
    constraints = kernel.graph.planner.constraints

    assert sentinel not in goal
    assert sentinel not in json.dumps(constraints, ensure_ascii=False)
    assert constraints["verification_command"] == ""
    assert "fixture_metadata" not in constraints
    assert "hidden_test_patch" not in constraints
    assert "test_patch" not in constraints


def test_public_task_baseline_already_passing_is_invalid_and_skips_agent(
    tmp_path: Path,
) -> None:
    py = json.dumps(sys.executable)
    manifest = EvaluationTaskSet.from_dict(
        {
            "schema_version": EVALUATION_TASK_SET_SCHEMA_VERSION,
            "tasks": [
                {
                    "task_id": "fake.public_already_passing",
                    "task_type": "public_representative",
                    "workspace": {"type": "fixture", "files": {"solution.py": "value = 1\n"}},
                    "user_task": "Change solution.py.",
                    "allowed_paths": ["solution.py"],
                    "expected_file_changes": ["solution.py"],
                    "verification_command": f"{py} -c \"from solution import value; assert value == 1\"",
                    "test_patch": "",
                    "fixture_metadata": {"fail_to_pass": ["fake::test"]},
                    "success": {"type": "verification_exit_code", "exit_code": 0},
                }
            ],
        },
        base_dir=tmp_path,
    )

    class FailingBootstrap:
        def __init__(self, **kwargs: Any) -> None:
            _ = kwargs
            raise AssertionError("AgentLoop must not start for invalid public baseline")

    result = EvaluationRunner(
        output_root=tmp_path / "out",
        run_id="baseline_already_passing",
        bootstrap_cls=FailingBootstrap,
    ).run(manifest)

    task = result["tasks"][0]
    assert task["status"] == "invalid_public_task"
    assert task["failure_category"] == "baseline_already_passing"
    assert task["baseline_failed"] is False
    assert task["baseline_checks"]["public"]["passed"] is True
    assert task["baseline_checks"]["hidden"]["passed"] is True
    assert task["evaluation_passed"] is False
    assert task["tests_passed"] is False
    assert result["summary"]["failure_reasons"] == {"baseline_already_passing": 1}


def test_public_task_baseline_verification_misconfiguration_skips_agent(
    tmp_path: Path,
) -> None:
    manifest = EvaluationTaskSet.from_dict(
        {
            "schema_version": EVALUATION_TASK_SET_SCHEMA_VERSION,
            "tasks": [
                {
                    "task_id": "fake.public_misconfigured",
                    "task_type": "public_representative",
                    "workspace": {"type": "fixture", "files": {"README.md": "fixture\n"}},
                    "user_task": "Change README.md.",
                    "allowed_paths": ["README.md"],
                    "expected_file_changes": ["README.md"],
                    "verification_command": "missing-python -m pytest fake::test",
                    "fixture_metadata": {"fail_to_pass": ["fake::test"]},
                    "success": {"type": "verification_exit_code", "exit_code": 0},
                }
            ],
        },
        base_dir=tmp_path,
    )

    class FailingBootstrap:
        def __init__(self, **kwargs: Any) -> None:
            _ = kwargs
            raise AssertionError("AgentLoop must not start for misconfigured baseline")

    result = EvaluationRunner(
        output_root=tmp_path / "out",
        run_id="baseline_misconfigured",
        bootstrap_cls=FailingBootstrap,
    ).run(manifest)

    task = result["tasks"][0]
    assert task["status"] == "verification_misconfigured"
    assert task["failure_category"] == "verification_misconfigured"
    assert task["baseline_failed"] is False
    assert task["verification_misconfiguration_reason"]
    assert task["evaluation_passed"] is False


def test_expected_file_changes_accepts_directory_targets() -> None:
    assert _expected_file_changes_satisfied(
        ["src/sqlfluff"],
        files_changed=["src/sqlfluff/rules/L060.py"],
    )
    assert not _expected_file_changes_satisfied(
        ["src/sqlfluff"],
        files_changed=["test/rules/std_L060_test.py"],
    )


def test_evaluator_test_patch_applies_inside_non_git_workspace(tmp_path: Path) -> None:
    workspace = tmp_path / "nested" / "workspace"
    workspace.mkdir(parents=True)
    task = EvaluationTask(
        task_id="fake.patch",
        workspace=EvaluationWorkspace(kind="fixture", files={}),
        user_task="Patch workspace.",
        allowed_paths=["test"],
        verification_command=f"{json.dumps(sys.executable)} -c \"print('ok')\"",
        success={"type": "verification_exit_code", "exit_code": 0},
        test_patch=(
            "diff --git a/test/generated_test.py b/test/generated_test.py\n"
            "new file mode 100644\n"
            "--- /dev/null\n"
            "+++ b/test/generated_test.py\n"
            "@@ -0,0 +1,2 @@\n"
            "+def test_generated():\n"
            "+    assert True\n"
        ),
    )

    result = _apply_test_patch(task, workspace=workspace, redactor=evaluation_runner.TraceRedactor())

    assert result is not None
    assert result.passed
    assert (workspace / "test" / "generated_test.py").exists()


def test_public_task_requires_baseline_fail_before_agent_patch_can_pass(
    tmp_path: Path,
) -> None:
    py = json.dumps(sys.executable)
    manifest = EvaluationTaskSet.from_dict(
        {
            "schema_version": EVALUATION_TASK_SET_SCHEMA_VERSION,
            "tasks": [
                {
                    "task_id": "fake.public_fail_to_pass",
                    "task_type": "public_representative",
                    "workspace": {"type": "fixture", "files": {"solution.py": "value = 0\n"}},
                    "user_task": "Make value equal 1.",
                    "allowed_paths": ["solution.py"],
                    "expected_file_changes": ["solution.py"],
                    "verification_command": f"{py} -c \"from solution import value; assert value == 1\"",
                    "fixture_metadata": {"fail_to_pass": ["fake::test"]},
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
            (self.project_root / "solution.py").write_text("value = 1\n", encoding="utf-8")
            return FakeResult()

        def close_resources(self) -> None:
            return None

    class FakeBootstrap:
        def __init__(self, **kwargs: Any) -> None:
            self.project_root = kwargs["project_root"]

        def boot(self, _goal: str) -> FakeKernel:
            return FakeKernel(self.project_root)

    result = EvaluationRunner(
        output_root=tmp_path / "out",
        run_id="baseline_fail_agent_pass",
        bootstrap_cls=FakeBootstrap,
    ).run(manifest)

    task = result["tasks"][0]
    assert task["baseline_failed"] is True
    assert task["baseline_checks"]["public"]["passed"] is False
    assert task["patch_applied"] is True
    assert task["patch_applicable"] is True
    assert task["fail_to_pass_satisfied"] is True
    assert task["agent_completed"] is True
    assert task["tests_passed"] is True
    assert task["evaluation_passed"] is True
    metrics = task["evaluation_metrics"]
    assert metrics["schema_version"] == "evaluation.metrics/v1"
    assert metrics["resolved"] == {
        "value": True,
        "resolved_rate_contribution": 1.0,
        "reason": "",
    }
    assert metrics["swe_bench"]["fail_to_pass"]["satisfied"] is True
    assert metrics["swe_bench"]["fail_to_pass"]["baseline_failed"] is True
    baseline_metrics = metrics["swe_bench"]["fail_to_pass"]["baseline_checks"]
    assert baseline_metrics["public"]["passed"] is False
    assert "test_patch" not in json.dumps(metrics["swe_bench"], sort_keys=True)
    assert metrics["swe_bench"]["pass_to_pass"]["status"] == "not_configured"
    assert metrics["swe_bench"]["pass_to_pass"]["reason"] == "manifest has no PASS_TO_PASS checks"
    manifest.tasks[0].fixture_metadata["PASS_TO_PASS"] = ["tests/test_existing.py::test_keeps_passing"]
    pass_to_pass = evaluation_runner._swe_bench_metrics(
        task=manifest.tasks[0],
        baseline_failed=True,
        baseline_checks={},
        fail_to_pass_satisfied=True,
    )["pass_to_pass"]
    assert pass_to_pass["status"] == "not_implemented"
    assert pass_to_pass["satisfied"] is None
    assert pass_to_pass["checks"] == ["tests/test_existing.py::test_keeps_passing"]


def test_evaluation_metrics_patch_tool_context_and_cost_helpers(tmp_path: Path) -> None:
    trace_dir = tmp_path / "trace"
    trace_dir.mkdir()
    events = [
        {"event_type": "tool_protocol.call_started", "payload": {"tool_name": "read_file"}},
        {"event_type": "tool_protocol.call_completed", "payload": {"tool_name": "read_file", "ok": True}},
        {"event_type": "tool_protocol.call_started", "payload": {"tool_name": "edit_apply"}},
        {"event_type": "tool_protocol.call_completed", "payload": {"tool_name": "edit_apply", "ok": False}},
        {"event_type": "tool_protocol.call_started", "payload": {"tool_name": "search_text"}},
        {"event_type": "model.response.received", "payload": {"usage": {"cost_estimate": 0.1234}}},
    ]
    (trace_dir / "events.jsonl").write_text(
        "\n".join(json.dumps(event) for event in events) + "\n",
        encoding="utf-8",
    )
    patch = {
        "diff": (
            "diff --git a/app.py b/app.py\n"
            "--- a/app.py\n"
            "+++ b/app.py\n"
            "@@ -1 +1,2 @@\n"
            "-old\n"
            "+new\n"
            "+extra\n"
            "diff --git a/tests/test_app.py b/tests/test_app.py\n"
            "--- a/tests/test_app.py\n"
            "+++ b/tests/test_app.py\n"
            "@@ -0,0 +1 @@\n"
            "+def test_app(): pass\n"
        ),
        "applicable": True,
    }

    assert evaluation_runner._tool_metrics_from_trace_events(events) == {
        "tool_call_count": 3,
        "tool_result_count": 2,
        "tool_success_count": 1,
        "tool_failure_count": 1,
        "tool_unknown_count": 1,
        "tool_success_rate": None,
        "distinct_tool_names": ["edit_apply", "read_file", "search_text"],
    }
    assert evaluation_runner._tool_metrics_from_trace_events(
        [
            {
                "event_type": "model.tool_call.proposed",
                "payload": {"tool_call_id": "call_1", "function": "read_file"},
            },
            {
                "event_type": "tool_protocol.call_started",
                "payload": {"tool_call_id": "call_1", "tool_name": "read_file"},
            },
            {
                "event_type": "tool_protocol.call_completed",
                "payload": {"tool_call_id": "call_1", "tool_name": "read_file", "ok": True},
            },
        ]
    ) == {
        "tool_call_count": 1,
        "tool_result_count": 1,
        "tool_success_count": 1,
        "tool_failure_count": 0,
        "tool_unknown_count": 0,
        "tool_success_rate": 1.0,
        "distinct_tool_names": ["read_file"],
    }
    assert evaluation_runner._patch_metrics(
        patch=patch,
        files_changed=["app.py", "tests/test_app.py"],
        expected_file_changes=["app.py"],
        allowed_paths=["app.py"],
        patch_applicable=True,
        allowed_scope_passed=False,
        patch_applied=True,
    ) == {
        "patch_applied": True,
        "patch_applicable": True,
        "allowed_scope_passed": False,
        "files_changed_count": 2,
        "expected_files_changed": True,
        "test_files_modified": True,
        "out_of_scope_files": ["tests/test_app.py"],
        "diff_added_lines": 3,
        "diff_deleted_lines": 1,
        "reason": "",
    }
    context = evaluation_runner._context_metrics(
        trace_events=events,
        capability_summary={
            "retrieval_calls": 0,
            "context_package_rebuild_count": 0,
            "context_compaction": {
                "requested": 0,
                "completed": 0,
                "failed": 0,
                "skipped": True,
                "reason": "context usage below compaction threshold",
            },
        },
        expected_file_changes=["app.py"],
        allowed_paths=["app.py"],
        cache_usage={"request_cache_hit_rate": 0.25, "run_cache_hit_rate": 0.5},
    )
    assert context["target_file_retrieval_hit"] is None
    assert context["target_file_retrieval_reason"] == "no retrieval evidence recorded"
    assert context["request_cache_hit_rate"] == 0.25
    assert context["run_cache_hit_rate"] == 0.5

    assert evaluation_runner._cost_metrics(
        trace_events=events,
        token_usage={
            "input_tokens": 1_000_000,
            "cached_input_tokens": 100_000,
            "output_tokens": 500_000,
        },
        model_profile={"model": "mimo-v2.5", "base_url": "https://token-plan-cn.xiaomimimo.com/v1"},
    )["cost_source"] == "provider_usage"

    priced = evaluation_runner._cost_metrics(
        trace_events=[],
        token_usage={
            "input_tokens": 1_000_000,
            "cached_input_tokens": 100_000,
            "output_tokens": 500_000,
        },
        model_profile={"model": "mimo-v2.5", "base_url": "https://token-plan-cn.xiaomimimo.com/v1"},
    )
    assert priced == {
        "cost_estimate": 0.26628,
        "currency": "USD",
        "cost_source": "pricing_table",
        "pricing_status": "priced",
        "pricing_source_url": "https://platform.xiaomimimo.com/docs/pricing",
        "retrieved_at": "2026-07-01",
        "pricing_unit": "1M tokens",
        "matched_model": "mimo-v2.5",
    }
    assert evaluation_runner._cost_metrics(
        trace_events=[],
        token_usage={"input_tokens": 10, "output_tokens": 5},
        model_profile={"model": "unknown-model", "base_url": "https://token-plan-cn.xiaomimimo.com/v1"},
    )["pricing_status"] == "unknown_model_or_unpriced"


def test_capability_summary_counts_trace_events_and_explains_skipped_compaction(tmp_path: Path) -> None:
    trace_dir = tmp_path / "trace"
    trace_dir.mkdir()
    (trace_dir / "events.jsonl").write_text(
        "\n".join(
            json.dumps(event)
            for event in [
                {"event_type": "model.request.created", "payload": {}},
                {"event_type": "model.response.received", "payload": {"latency_ms": 1200}},
                {"event_type": "model.request.failed", "payload": {"latency_ms": 300}},
                {"event_type": "model.tool_call.proposed", "payload": {}},
                {"event_type": "tool_protocol.call_started", "payload": {}},
                {"event_type": "tool_protocol.call_completed", "payload": {}},
                {"event_type": "context.tool_observation_added", "payload": {}},
                {"event_type": "retrieval.query.completed", "payload": {}},
                {"event_type": "context.bundle_built", "payload": {}},
                {"event_type": "context.rendered_for_model", "payload": {}},
                {"event_type": "command.completed", "payload": {"duration_ms": 250, "backend": "local_process"}},
            ]
        )
        + "\n",
        encoding="utf-8",
    )
    verification = CommandEvalResult(
        command=f"{json.dumps(sys.executable)} -m pytest tests/test_app.py",
        exit_code=0,
        duration_seconds=1.25,
    )
    summary = _build_capability_summary(
        trace=trace_dir,
        trace_summary={"model_usage_summary": {"requests": 1}},
        checks={"public": {"status": "passed"}, "hidden": {"status": "passed"}},
        verification=verification,
        public_verification=verification,
        hidden_verification=verification,
        final_report_status="completed",
        agent_status="completed",
        wall_time_seconds=2.5,
    )

    assert summary["model_turn_request_count"] == 1
    assert summary["model_turn_result_count"] == 2
    assert summary["tool_call_envelope_count"] == 2
    assert summary["tool_result_count"] == 1
    assert summary["tool_observation_count"] == 1
    assert summary["retrieval_calls"] == 1
    assert summary["context_package_rebuild_count"] == 2
    assert summary["context_compaction"]["requested"] == 0
    assert summary["context_compaction"]["skipped"] is True
    assert summary["context_compaction"]["reason"]
    assert summary["sandbox_backend"] == "local_process"
    assert summary["local_process_fallback_count"] == 1
    assert summary["verification_checks"] == ["public", "hidden"]
    assert summary["final_report_status"] == "completed"
    assert summary["agent_loop_result_status"] == "completed"
    assert summary["timing"]["wall_time_seconds"] == 2.5
    assert summary["timing"]["provider_time_seconds"] == 1.5
    assert summary["timing"]["sandbox_time_seconds"] == 0.25
    assert summary["timing"]["context_retrieval_compaction_time_seconds"] == 0.0
    assert summary["timing"]["pytest_time_seconds"] == 1.25


def test_provider_time_falls_back_to_request_response_monotonic_ms() -> None:
    events = [
        {
            "event_type": "model.request.created",
            "monotonic_ms": 100,
            "payload": {"request_id": "req_1"},
        },
        {
            "event_type": "model.response.received",
            "monotonic_ms": 2600,
            "payload": {"request_id": "req_1"},
        },
        {
            "event_type": "model.request.created",
            "monotonic_ms": 3000,
            "payload": {"request_id": "req_2"},
        },
        {
            "event_type": "model.request.failed",
            "monotonic_ms": 3500,
            "payload": {"request_id": "req_2"},
        },
    ]

    assert _provider_time_seconds(events, {}) == 3.0


def test_evaluation_sanitized_result_shape_is_safe_and_current() -> None:
    payload: dict[str, Any] = {
        "schema_version": EVALUATION_RESULT_SCHEMA_VERSION,
        "summary": {
            "task_count": 1,
            "agent_completed_count": 1,
            "evaluation_passed_count": 1,
        },
        "tasks": [
            {
                "task_id": "fake.sanitized_result",
                "agent_completed": True,
                "evaluation_passed": True,
                "patch_applicable": True,
                "allowed_scope_passed": True,
                "public_verification_passed": True,
                "hidden_verification_passed": True,
                "contract_satisfaction": {"status": "satisfied"},
                "miscompletion_count": 0,
                "repair_attempt_count": 0,
                "repair_execution_count": 0,
                "turn_count": 2,
                "tool_calls": 1,
                "blocked_reason": "",
                "failure_category": "none",
                "final_report_status": "completed",
                "reproducible_environment": {
                    "schema_version": "evaluation.environment/v1",
                    "policy": {
                        "permission_profile": "workspace-write",
                        "approval_policy": "never",
                        "network_access": "denied",
                    },
                },
            }
        ],
    }
    text = json.dumps(payload, ensure_ascii=False, sort_keys=True)

    assert payload["schema_version"] == EVALUATION_RESULT_SCHEMA_VERSION
    assert "SINGULARITY_API_KEY" not in text
    assert "api_key" not in text.lower()
    assert "sk-" not in text
    task = payload["tasks"][0]
    assert task["reproducible_environment"]["schema_version"] == "evaluation.environment/v1"
    policy = task["reproducible_environment"]["policy"]
    assert policy["permission_profile"] == "workspace-write"
    assert policy["approval_policy"] == "never"
    assert policy["network_access"] == "denied"
    assert "approval_mode" not in policy
    assert "security_mode" not in policy
    assert "sandbox_strategy" not in policy
    for field in [
        "agent_completed",
        "evaluation_passed",
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
        "success",
        "completed",
        "success_count",
        "completed_count",
        "tool_call_count",
        "failure_" + "repair_count",
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
                        "strategy": {
                            "tool_policy": "read_write",
                            "permission_profile": "workspace-write",
                            "approval_policy": "never",
                            "network_access": "denied",
                        },
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
    assert manifest.tasks[0].strategy["permission_profile"] == "workspace-write"
    assert manifest.tasks[0].strategy["approval_policy"] == "never"
    assert manifest.tasks[0].verification_command == f"{json.dumps(sys.executable)} -m pytest tests/test_math.py"
    assert manifest.tasks[1].task_id == "private.repo_issue"
    assert manifest.tasks[1].workspace.kind == "repo"
    assert manifest.tasks[1].workspace.path == str(tmp_path / "repo")
    assert manifest.tasks[1].workspace.start_commit == "abc123"


def test_summarize_evaluation_results_reports_cache_and_rates(tmp_path: Path) -> None:
    first = EvaluationTaskResult(
        task_id="one",
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
        evaluation_passed=True,
        final_report_status="completed",
        evaluation_metrics={
            "resolved": {"value": True, "resolved_rate_contribution": 1.0, "reason": ""},
            "swe_bench": {
                "fail_to_pass": {"satisfied": True},
                "pass_to_pass": {"satisfied": None, "status": "not_configured"},
            },
            "tools": {"tool_success_rate": 1.0},
            "cost": {"cost_estimate": 0.4, "cost_source": "pricing_table"},
        },
    )
    second = EvaluationTaskResult(
        task_id="two",
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
        evaluation_passed=False,
        final_report_status="completed",
        evaluation_metrics={
            "resolved": {"value": False, "resolved_rate_contribution": 0.0, "reason": "verification_failed"},
            "swe_bench": {
                "fail_to_pass": {"satisfied": False},
                "pass_to_pass": {"satisfied": True, "status": "satisfied"},
            },
            "tools": {"tool_success_rate": 0.5},
            "cost": {"cost_estimate": 0.6, "cost_source": "provider_usage"},
        },
    )
    blocked = EvaluationTaskResult(
        task_id="blocked",
        tests_passed=False,
        infrastructure_blocked=True,
        prompt_tokens=10,
        cached_tokens=0,
        request_cache_hit_rate=0.0,
        run_cache_hit_rate=0.0,
        tool_calls=0,
        files_changed=[],
        duration_seconds=0.5,
        error_summary="infrastructure blocked",
        workspace=str(tmp_path),
        trace=str(tmp_path / "trace3"),
        status="environment_blocker",
        turn_count=0,
        failure_category="environment_blocker",
        evaluation_metrics={
            "resolved": {"value": False, "resolved_rate_contribution": 0.0, "reason": "environment_blocker"},
            "swe_bench": {
                "fail_to_pass": {"satisfied": False},
                "pass_to_pass": {"satisfied": None, "status": "not_configured"},
            },
            "tools": {"tool_success_rate": None},
            "cost": {"cost_estimate": 9.0, "cost_source": "pricing_table"},
        },
    )

    summary = summarize_evaluation_results([first, second, blocked])

    assert summary == {
        "task_count": 3,
        "scored_task_count": 2,
        "infrastructure_blocked_count": 1,
        "score_status": "scored",
        "task_completion_rate": 1.0,
        "tests_passed_count": 2,
        "test_pass_rate": 1.0,
        "prompt_tokens": 210,
        "cached_tokens": 100,
        "request_cache_hit_rate": 0.5,
        "run_cache_hit_rate": 0.4762,
        "tool_calls": 5,
        "resolved_count": 1,
        "resolved_rate": 0.5,
        "fail_to_pass_satisfied_count": 1,
        "pass_to_pass_satisfied_count": 1,
        "pass_to_pass_not_configured_count": 2,
        "average_tool_success_rate": 0.75,
        "total_cost_estimate": 10.0,
        "cost_per_resolved": 1.0,
        "evaluation_passed_rate": 0.5,
        "verification_pass_rate": 1.0,
        "average_turns": 2.5,
        "average_tool_calls": 2.5,
        "agent_completed_count": 2,
        "evaluation_passed_count": 1,
        "repair_attempt_count": 0,
        "repair_execution_count": 0,
        "policy_blocks": 0,
        "miscompletion_count": 1,
        "failure_reasons": {"environment_blocker": 1, "verification_failed": 1},
    }


def test_summarize_evaluation_results_preserves_unknown_tool_success_rate(tmp_path: Path) -> None:
    result = EvaluationTaskResult(
        task_id="unknown-tools",
        tests_passed=False,
        infrastructure_blocked=False,
        prompt_tokens=10,
        cached_tokens=0,
        request_cache_hit_rate=0.0,
        run_cache_hit_rate=0.0,
        tool_calls=1,
        files_changed=[],
        duration_seconds=1.0,
        error_summary="blocked",
        workspace=str(tmp_path),
        trace=str(tmp_path / "trace"),
        status="blocked",
        turn_count=1,
        evaluation_metrics={
            "resolved": {"value": False, "resolved_rate_contribution": 0.0, "reason": "blocked"},
            "swe_bench": {
                "fail_to_pass": {"satisfied": False},
                "pass_to_pass": {"satisfied": None, "status": "not_configured"},
            },
            "tools": {"tool_success_rate": None},
            "cost": {"cost_estimate": None},
        },
    )

    summary = summarize_evaluation_results([result])

    assert summary["average_tool_success_rate"] is None


def test_summarize_evaluation_results_uses_agent_completed_only(tmp_path: Path) -> None:
    finalized_only = EvaluationTaskResult(
        task_id="kernel-finalized-blocked",
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
    )
    false_completed = EvaluationTaskResult(
        task_id="completed-but-verification-failed",
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
        agent_completed=True,
    )

    summary = summarize_evaluation_results([finalized_only, false_completed])

    assert "completed_count" not in summary
    assert summary["agent_completed_count"] == 1
    assert summary["miscompletion_count"] == 1
    assert summary["failure_reasons"] == {"blocked": 1, "verification_failed": 1}


def test_evaluation_report_markdown_includes_metrics_scorecard(tmp_path: Path) -> None:
    task_result = EvaluationTaskResult(
        task_id="scorecard-task",
        tests_passed=True,
        infrastructure_blocked=False,
        prompt_tokens=10,
        cached_tokens=2,
        request_cache_hit_rate=0.2,
        run_cache_hit_rate=0.2,
        tool_calls=1,
        files_changed=["app.py"],
        duration_seconds=1.0,
        error_summary="",
        workspace=str(tmp_path),
        trace=str(tmp_path / "trace"),
        status="success",
        turn_count=1,
        agent_completed=True,
        evaluation_passed=True,
        evaluation_metrics={
            "schema_version": "evaluation.metrics/v1",
            "resolved": {"value": True, "resolved_rate_contribution": 1.0, "reason": ""},
            "swe_bench": {
                "fail_to_pass": {"satisfied": True},
                "pass_to_pass": {"satisfied": None, "status": "not_configured"},
            },
            "verification": {"tests_passed": True},
            "patch": {"files_changed_count": 1, "out_of_scope_files": []},
            "trajectory": {"turn_count": 1},
            "tools": {"tool_call_count": 1, "tool_success_rate": 1.0},
            "context": {"compaction": {"reason": "context usage below compaction threshold"}},
            "efficiency": {"wall_time_seconds": 1.0},
            "cost": {"cost_estimate": 0.1, "cost_source": "pricing_table"},
            "safety": {"policy_blocks": 0},
        },
    )
    payload = {
        "run_id": "scorecard-run",
        "summary": summarize_evaluation_results([task_result]),
        "tasks": [task_result.to_dict()],
    }

    markdown = evaluation_report_markdown(payload)

    assert "## Metrics / Scorecard" in markdown
    assert "- resolved: 1 / 1 (1.0000)" in markdown
    assert "`scorecard-task` | True | True | not_configured" in markdown
    assert "0.100000 (pricing_table)" in markdown


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
    assert result["summary"]["evaluation_passed_count"] == 1
    task = result["tasks"][0]
    assert task["task_id"] == "fake.write_file"
    assert "success" not in task
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
    assert "completed" not in task
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
    scorecard = task["evaluation_metrics"]
    assert scorecard["reproducibility"]["reproducible_environment"]["workspace"]["type"] == "fixture"
    rendered_scorecard = json.dumps(scorecard, sort_keys=True)
    assert "hidden_verification_command" not in rendered_scorecard
    assert "verification_prepare_commands" not in rendered_scorecard
    assert "test_patch" not in rendered_scorecard
    for removed in [
        "tool_call_count",
        "failure_" + "repair_count",
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

    assert first["summary"]["evaluation_passed_count"] == 1
    assert second["summary"]["evaluation_passed_count"] == 0
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
    assert task["evaluation_passed"] is True
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
    assert task["evaluation_passed"] is True
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


def test_evaluation_uses_task_strategy_max_turns(tmp_path: Path) -> None:
    py = json.dumps(sys.executable)
    manifest = EvaluationTaskSet.from_dict(
        {
            "schema_version": EVALUATION_TASK_SET_SCHEMA_VERSION,
            "tasks": [
                {
                    "task_id": "fake.strategy_max_turns",
                    "workspace": {"type": "fixture", "files": {"README.md": "fixture\n"}},
                    "user_task": "Write done.txt.",
                    "allowed_paths": ["done.txt"],
                    "verification_command": f"{py} -c \"from pathlib import Path; assert Path('done.txt').exists()\"",
                    "strategy": {"max_turns": 24},
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
            trace_summary={"tool_calls": 1, "model_usage_summary": {"requests": 1}},
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
            assert self.config.max_turns == 24
            return FakeKernel(self.project_root)

    result = EvaluationRunner(
        output_root=tmp_path / "out",
        run_id="strategy_max_turns",
        bootstrap_cls=FakeBootstrap,
    ).run(manifest)

    task = result["tasks"][0]
    assert task["reproducible_environment"]["model_profile"]["max_turns"] == 24
    assert task["evaluation_passed"] is True


def test_evaluation_runner_max_turns_overrides_task_strategy(tmp_path: Path) -> None:
    py = json.dumps(sys.executable)
    manifest = EvaluationTaskSet.from_dict(
        {
            "schema_version": EVALUATION_TASK_SET_SCHEMA_VERSION,
            "tasks": [
                {
                    "task_id": "fake.runner_max_turns",
                    "workspace": {"type": "fixture", "files": {"README.md": "fixture\n"}},
                    "user_task": "Write done.txt.",
                    "allowed_paths": ["done.txt"],
                    "verification_command": f"{py} -c \"from pathlib import Path; assert Path('done.txt').exists()\"",
                    "strategy": {"max_turns": 24},
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
            trace_summary={"tool_calls": 1, "model_usage_summary": {"requests": 1}},
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
            assert self.config.max_turns == 7
            return FakeKernel(self.project_root)

    result = EvaluationRunner(
        output_root=tmp_path / "out",
        run_id="runner_max_turns",
        max_turns=7,
        bootstrap_cls=FakeBootstrap,
    ).run(manifest)

    assert result["tasks"][0]["reproducible_environment"]["model_profile"]["max_turns"] == 7


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
    assert result["tasks"][0]["evaluation_passed"] is True
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
    assert task["evaluation_passed"] is False
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
    assert task["evaluation_passed"] is True
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
    assert result["summary"]["score_status"] == "environment_blocker"
    assert result["summary"]["failure_reasons"] == {"environment_blocker": 1}
    assert result["summary"]["task_completion_rate"] == 0.0
    assert result["summary"]["test_pass_rate"] == 0.0
    task = result["tasks"][0]
    assert task["infrastructure_blocked"] is True
    assert task["status"] == "environment_blocker"
    assert task["failure_category"] == "environment_blocker"
    assert task["verification"] is None
    assert "environment blocker" in task["error_summary"]


def test_evaluation_classifies_observed_sandbox_unavailability_as_environment_blocker(
    tmp_path: Path,
) -> None:
    py = json.dumps(sys.executable)
    manifest = EvaluationTaskSet.from_dict(
        {
            "schema_version": EVALUATION_TASK_SET_SCHEMA_VERSION,
            "tasks": [
                {
                    "task_id": "fake.sandbox_environment_blocker",
                    "workspace": {"type": "fixture", "files": {"README.md": "fixture\n"}},
                    "user_task": "Write done.txt with ok and verify it.",
                    "allowed_paths": ["done.txt"],
                    "expected_file_changes": ["done.txt"],
                    "verification_command": f'{py} -c "raise SystemExit(99)"',
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

    class FakeEvidence:
        sandbox_observations: ClassVar[list[dict[str, Any]]] = [
            {
                "source": "verification",
                "backend": "windows",
                "status": "backend_unavailable",
                "enforcement_status": "backend_unavailable",
                "sandbox_id": "sandbox_1",
            }
        ]

    class FakePlannerState:
        blocked_reasons: ClassVar[list[str]] = [
            "sandbox backend unavailable: run elevated sandbox setup before verification can proceed"
        ]

    class FakePlanner:
        evidence = FakeEvidence()
        state = FakePlannerState()

    class FakeGraph:
        trace = FakeTrace()
        planner = FakePlanner()

    class FakeResult:
        status = RunStatus.BLOCKED
        final_answer = "sandbox backend unavailable"
        final_report = FinalReport(
            run_id="run_1",
            session_id="session_1",
            task_id="task_1",
            kernel_status="finalized",
            shutdown_reason="blocked",
            diagnostics_count=1,
            cleanup_status="completed",
            recovered_previous_run=False,
            uncertain_transactions=[],
            workspace_lock_status="released",
            planner_summary={
                "failure_repair_summary": {
                    "latest_failure_category": "sandbox_limitation",
                    "latest_blocked_reason": (
                        "sandbox backend unavailable: run elevated sandbox setup before verification"
                    ),
                }
            },
            trace_summary={
                "tool_calls": 2,
                "key_artifacts": ["artifact_sandbox"],
                "model_usage_summary": {"input_tokens": 10, "requests": 2},
            },
        )

    class FakeKernel:
        graph = FakeGraph()

        def __init__(self, project_root: Path) -> None:
            self.project_root = project_root

        def run_task(self, _goal: str) -> FakeResult:
            (self.project_root / "done.txt").write_text("ok\n", encoding="utf-8")
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
        run_id="sandbox_environment_blocker",
        bootstrap_cls=FakeBootstrap,
    ).run(manifest)

    task = result["tasks"][0]
    assert task["status"] == "environment_blocker"
    assert task["failure_category"] == "environment_blocker"
    assert task["infrastructure_blocked"] is True
    assert task["verification"] is None
    assert task["checks"]["public"]["status"] == "not_run"
    assert task["checks"]["hidden"]["status"] == "not_run"
    assert task["tests_passed"] is False
    assert task["files_changed"] == ["done.txt"]
    assert task["patch"]["changed_files"] == ["done.txt"]
    assert task["tool_calls"] == 2
    assert task["turn_count"] == 2
    assert task["trace"] == str(tmp_path / "trace")
    assert task["trace_artifact_refs"] == ["artifact_sandbox"]
    assert "sandbox backend unavailable" in task["blocked_reason"]
    assert result["summary"]["scored_task_count"] == 0
    assert result["summary"]["score_status"] == "environment_blocker"
    assert result["summary"]["failure_reasons"] == {"environment_blocker": 1}


def test_evaluation_short_circuits_python_ssl_environment_error_before_post_verification(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    py = json.dumps(sys.executable)
    manifest = EvaluationTaskSet.from_dict(
        {
            "schema_version": EVALUATION_TASK_SET_SCHEMA_VERSION,
            "tasks": [
                {
                    "task_id": "fake.python_ssl_environment_blocker",
                    "workspace": {"type": "fixture", "files": {"README.md": "fixture\n"}},
                    "user_task": "Write done.txt with ok and verify it.",
                    "allowed_paths": ["done.txt"],
                    "expected_file_changes": ["done.txt"],
                    "verification_command": f'{py} -c "raise SystemExit(99)"',
                    "success": {"type": "verification_exit_code", "exit_code": 0},
                }
            ],
        },
        base_dir=tmp_path,
    )

    def fail_if_post_verification_runs(*args: Any, **kwargs: Any) -> None:
        raise AssertionError("post-agent verification must not run after Python SSL environment blocker")

    monkeypatch.setattr(evaluation_runner, "_run_shell", fail_if_post_verification_runs)

    class FakeTraceStore:
        run_dir = tmp_path / "trace"

    class FakeTrace:
        store = FakeTraceStore()

    class FakeEvidence:
        sandbox_observations: ClassVar[list[dict[str, Any]]] = []

    class FakePlanner:
        evidence = FakeEvidence()

    class FakeGraph:
        trace = FakeTrace()
        planner = FakePlanner()

    class FakeResult:
        status = RunStatus.BLOCKED
        final_answer = "python_runtime_environment_blocker"
        final_report = FinalReport(
            run_id="run_1",
            session_id="session_1",
            task_id="task_1",
            kernel_status="finalized",
            shutdown_reason="blocked",
            diagnostics_count=1,
            cleanup_status="completed",
            recovered_previous_run=False,
            uncertain_transactions=[],
            workspace_lock_status="released",
            planner_summary={
                "failure_repair_summary": {
                    "latest_failure_category": "environment_error",
                    "latest_blocked_reason": (
                        "python_runtime_environment_blocker: "
                        "ssl_low_integrity_runtime_initialization_failed"
                    ),
                },
                "sandbox_isolation_summary": {
                    "selected_backends": ["windows"],
                    "local_process_backend_count": 0,
                },
            },
            trace_summary={
                "tool_calls": 2,
                "key_artifacts": ["artifact_python_ssl"],
                "model_usage_summary": {"input_tokens": 10, "requests": 2},
            },
        )

    class FakeKernel:
        graph = FakeGraph()

        def __init__(self, project_root: Path) -> None:
            self.project_root = project_root

        def run_task(self, _goal: str) -> FakeResult:
            (self.project_root / "done.txt").write_text("ok\n", encoding="utf-8")
            return FakeResult()

        def close_resources(self) -> None:
            return None

    class FakeBootstrap:
        def __init__(self, **kwargs: Any) -> None:
            self.project_root = kwargs["project_root"]

        def boot(self, _goal: str) -> FakeKernel:
            return FakeKernel(self.project_root)

    result = EvaluationRunner(
        output_root=tmp_path / "out",
        run_id="python_ssl_environment_blocker",
        bootstrap_cls=FakeBootstrap,
    ).run(manifest)

    task = result["tasks"][0]
    assert task["status"] == "environment_blocker"
    assert task["failure_category"] == "environment_blocker"
    assert task["infrastructure_blocked"] is True
    assert task["verification"] is None
    assert task["checks"]["public"]["status"] == "not_run"
    assert task["checks"]["hidden"]["status"] == "not_run"
    assert "ssl_low_integrity_runtime_initialization_failed" in task["blocked_reason"]
    assert result["summary"]["score_status"] == "environment_blocker"


def test_evaluation_does_not_treat_plain_agent_block_as_environment_blocker(tmp_path: Path) -> None:
    py = json.dumps(sys.executable)
    manifest = EvaluationTaskSet.from_dict(
        {
            "schema_version": EVALUATION_TASK_SET_SCHEMA_VERSION,
            "tasks": [
                {
                    "task_id": "fake.agent_blocked",
                    "workspace": {"type": "fixture", "files": {"README.md": "fixture\n"}},
                    "user_task": "Stop without changing files.",
                    "allowed_paths": ["README.md"],
                    "expected_file_changes": ["README.md"],
                    "verification_command": f'{py} -c "print(\'ok\')"',
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

    class FakeEvidence:
        sandbox_observations: ClassVar[list[dict[str, Any]]] = []

    class FakePlanner:
        evidence = FakeEvidence()

    class FakeGraph:
        trace = FakeTrace()
        planner = FakePlanner()

    class FakeResult:
        status = RunStatus.BLOCKED
        final_answer = "agent requires clarification"
        final_report = FinalReport(
            run_id="run_1",
            session_id="session_1",
            task_id="task_1",
            kernel_status="finalized",
            shutdown_reason="blocked",
            diagnostics_count=1,
            cleanup_status="completed",
            recovered_previous_run=False,
            uncertain_transactions=[],
            workspace_lock_status="released",
            trace_summary={"tool_calls": 1, "model_usage_summary": {"input_tokens": 10}},
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
        run_id="agent_blocked",
        bootstrap_cls=FakeBootstrap,
    ).run(manifest)

    task = result["tasks"][0]
    assert task["status"] == "blocked"
    assert task["failure_category"] == "blocked"
    assert task["infrastructure_blocked"] is False
    assert task["verification"]["passed"] is True
    assert task["tests_passed"] is True
    assert task["evaluation_passed"] is False
    assert task["files_changed"] == []
    assert task["patch_applicable"] is False
    assert result["summary"]["tests_passed_count"] == 1
    assert result["summary"]["evaluation_passed_count"] == 0
    assert result["summary"]["failure_reasons"] == {"blocked": 1}


def test_result_status_preserves_agent_blocked_when_verification_fails() -> None:
    verification = CommandEvalResult(command="pytest", exit_code=1, duration_seconds=0.1)

    status = _result_status(
        success=False,
        tests_passed=False,
        infrastructure_blocked=False,
        agent_status="blocked",
        verification=verification,
        policy_blocks=0,
        errors=["agent status: blocked", "verification failed"],
    )

    assert status == "blocked"
    assert (
        _failure_category(
            {},
            status=status,
            verification=verification,
            infrastructure_blocked=False,
            policy_blocks=0,
            errors=["agent status: blocked", "verification failed"],
        )
        == "blocked"
    )


def test_evaluation_reduces_python_ssl_environment_error_to_environment_blocker() -> None:
    payload = {
        "planner_summary": {
            "failure_repair_summary": {
                "latest_failure_category": "environment_error",
                "latest_blocked_reason": (
                    "python_runtime_environment_blocker: "
                    "ssl_low_integrity_runtime_initialization_failed"
                ),
            },
            "sandbox_isolation_summary": {
                "selected_backends": ["windows"],
                "local_process_backend_count": 0,
            },
        }
    }

    status = _result_status(
        success=False,
        tests_passed=False,
        infrastructure_blocked=True,
        agent_status="blocked",
        verification=None,
        policy_blocks=0,
        errors=["environment blocker: python ssl runtime"],
    )
    category = _failure_category(
        payload,
        status=status,
        verification=None,
        infrastructure_blocked=True,
        policy_blocks=0,
        errors=["environment blocker: python ssl runtime"],
    )

    assert status == "environment_blocker"
    assert category == "environment_blocker"
    assert payload["planner_summary"]["sandbox_isolation_summary"]["local_process_backend_count"] == 0


@pytest.mark.parametrize(
    "error_summary",
    [
        "ImportError: DLL load failed while importing _ssl: libssl-3-x64.dll was not found",
        "OpenSSL provider missing: Library\\lib\\ossl-modules was not found",
        "OpenSSL config missing: Library\\ssl\\openssl.cnf was not found",
        "ssl_low_integrity_runtime_initialization_failed: DLL initialization routine failed",
        "certificate path unreadable from ssl.get_default_verify_paths()",
    ],
)
def test_evaluation_shell_python_ssl_runtime_failure_is_environment_error(error_summary: str) -> None:
    category = _command_failure_category(
        [sys.executable, "-m", "pytest", "tests/test_app.py"],
        exit_code=1,
        error_summary=error_summary,
    )

    assert category == "environment_error"


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
    assert task["evaluation_passed"] is False
    assert task["agent_completed"] is True
    assert "success" not in task
    assert "completed" not in task
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
                        "task_id": "fake.failure_replay_contract",
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
    assert record["task_id"] == "fake.failure_replay_contract"
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

    assert result["summary"]["evaluation_passed_count"] == 1
    task = result["tasks"][0]
    assert task["evaluation_passed"] is True
    assert task["files_changed"] == ["solution.py"]
    assert "tests/test_hidden.py" not in seen_goals[0]


