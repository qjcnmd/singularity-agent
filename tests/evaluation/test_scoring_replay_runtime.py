from __future__ import annotations

from pathlib import Path

import pytest

from singularity.evaluation import (
    BenchmarkTask,
    EvaluationHook,
    EvaluationProfile,
    EvaluationRuntime,
    ExpectedOutcome,
    ExpectedOutcomeKind,
    PatchQualityEvaluator,
    RegressionDetector,
    ScoringEngine,
    TraceReplayRuntime,
    WorkspaceSnapshot,
    WorkspaceSnapshotKind,
)
from singularity.command import CommandDecision, CommandPolicyResult, CommandRisk, SemanticStatus
from singularity.observability import TraceEventType, TraceRuntime


def _task(task_id: str = "task.eval") -> BenchmarkTask:
    return BenchmarkTask(
        task_id=task_id,
        version="v1",
        title="Update behavior",
        input_prompt="Update behavior and run tests.",
        workspace_snapshot=WorkspaceSnapshot(
            kind=WorkspaceSnapshotKind.GIT_REF,
            git_ref="HEAD",
        ),
        expected_outcomes=[
            ExpectedOutcome(kind=ExpectedOutcomeKind.TEST, weight=0.5),
            ExpectedOutcome(kind=ExpectedOutcomeKind.HEURISTIC, weight=0.5),
        ],
        tags=["medium", "tool-heavy"],
    )


def _trace(root: Path, run_id: str = "run_eval") -> TraceRuntime:
    trace = TraceRuntime.create(root, run_id=run_id, session_id=f"{run_id}_session")
    trace.emit(
        TraceEventType.MODEL_RESPONSE_RECEIVED,
        runtime="model",
        summary="Model response received.",
        ids={"task_id": "task_eval"},
        payload={
            "usage": {
                "input_tokens": 100,
                "output_tokens": 20,
                "total_tokens": 120,
                "cost_estimate": 0.05,
            },
            "latency_ms": 1200,
        },
    )
    trace.emit(
        TraceEventType.TOOL_DISPATCH_COMPLETED,
        runtime="tool",
        summary="read_file completed.",
        ids={"task_id": "task_eval", "action_id": "tool_1"},
        payload={"tool_name": "read_file", "ok": True},
    )
    trace.emit(
        TraceEventType.VERIFICATION_CHECK_COMPLETED,
        runtime="verification",
        summary="unit tests passed",
        ids={"task_id": "task_eval", "verification_id": "check_1"},
        payload={"status": "passed"},
    )
    trace.emit(
        TraceEventType.TOOL_DISPATCH_COMPLETED,
        runtime="tool",
        summary="run_verification completed.",
        ids={"task_id": "task_eval", "action_id": "tool_2"},
        payload={"tool_name": "run_verification", "ok": True},
    )
    return trace


def test_scoring_engine_combines_test_and_heuristic_partial_scores() -> None:
    result = ScoringEngine().score(
        task=_task(),
        verification={"status": "ready", "passed": 1, "failed": 0},
        heuristics={"patch_quality": 0.6},
        trace_metrics={"policy_denials": 0, "interventions": 0},
    )

    assert result.status == "success"
    assert result.score == 0.8
    assert result.subscores["test"] == 1.0
    assert result.subscores["heuristic"] == 0.6


def test_patch_quality_evaluator_penalizes_large_and_redundant_diffs() -> None:
    small = PatchQualityEvaluator().evaluate(
        diff_summary=[{"path": "app.py", "added_lines": 3, "removed_lines": 1}],
        verification={"status": "ready"},
    )
    large = PatchQualityEvaluator().evaluate(
        diff_summary=[
            {
                "path": "app.py",
                "added_lines": 180,
                "removed_lines": 12,
                "redundant_code": True,
            }
        ],
        verification={"status": "failed"},
    )

    assert small.score > large.score
    assert small.metrics["tests_passed"] is True
    assert large.metrics["redundant_code"] is True
    assert "large_diff" in large.warnings


def test_trace_replay_is_deterministic_for_same_trace_and_profile(tmp_path: Path) -> None:
    trace = _trace(tmp_path)
    profile = EvaluationProfile(
        name="fixed",
        model="gpt-test",
        prompt_profile="baseline",
        memory_enabled=False,
        allowed_tools=["read_file"],
        tool_policy="read_only",
    )
    replay = TraceReplayRuntime(project_root=tmp_path)

    first = replay.replay(trace.store.run_dir, profile=profile)
    second = replay.replay(trace.store.run_dir, profile=profile)

    assert first.result_hash == second.result_hash
    assert first.deterministic is True
    assert first.config_fingerprint == second.config_fingerprint
    assert first.metrics["tool_calls"] == 2
    assert first.metrics["trace_input_digest"] == first.trace_input_digest


def test_trace_replay_hash_includes_trace_input_digest(tmp_path: Path) -> None:
    first_trace = _trace(tmp_path / "first", run_id="same_shape")
    second_trace = _trace(tmp_path / "second", run_id="same_shape")
    events_path = second_trace.store.run_dir / "events.jsonl"
    events_path.write_text(
        events_path.read_text(encoding="utf-8").replace("read_file completed", "list_files completed"),
        encoding="utf-8",
    )
    profile = EvaluationProfile(
        name="fixed",
        model="gpt-test",
        prompt_profile="baseline",
        memory_enabled=False,
        allowed_tools=["read_file"],
        tool_policy="read_only",
    )
    replay = TraceReplayRuntime(project_root=tmp_path)

    first = replay.replay(first_trace.store.run_dir, profile=profile)
    second = replay.replay(second_trace.store.run_dir, profile=profile)

    assert first.trace_input_digest != second.trace_input_digest
    assert first.result_hash != second.result_hash


def test_trace_replay_simulates_side_effect_events_by_default(tmp_path: Path) -> None:
    trace = _trace(tmp_path, run_id="run_side_effect")
    trace.emit(
        TraceEventType.COMMAND_COMPLETED,
        runtime="command",
        summary="command completed",
        ids={"task_id": "task_eval", "command_id": "cmd_1"},
        payload={"status": "success"},
    )
    profile = EvaluationProfile(
        name="fixed",
        model="gpt-test",
        prompt_profile="baseline",
        memory_enabled=False,
        allowed_tools=["read_file"],
        tool_policy="read_only",
    )

    result = TraceReplayRuntime(project_root=tmp_path).replay(trace.store.run_dir, profile=profile)

    assert result.replay_classification == "simulated_side_effects"
    assert result.side_effects_simulated == 1
    assert result.metrics["side_effect_events"] == 1


def test_evaluation_runtime_runs_ab_and_regression_reports(tmp_path: Path) -> None:
    trace = _trace(tmp_path, run_id="run_ab")
    runtime = EvaluationRuntime(project_root=tmp_path, output_root=tmp_path / "evals")
    baseline = EvaluationProfile(
        name="baseline",
        model="gpt-a",
        prompt_profile="default",
        memory_enabled=True,
        allowed_tools=["read_file", "run_verification"],
        tool_policy="read_write",
    )
    candidate = EvaluationProfile(
        name="candidate",
        model="gpt-b",
        prompt_profile="compact",
        memory_enabled=False,
        allowed_tools=["read_file"],
        tool_policy="read_only",
    )

    report = runtime.run_suite(
        tasks=[_task()],
        profiles=[baseline, candidate],
        trace_run_dir=trace.store.run_dir,
    )
    regression = RegressionDetector().compare(report.profile_reports[0], report.profile_reports[1])

    assert report.metrics["success_rate"] == 0.5
    assert {item.profile.name for item in report.profile_reports} == {"baseline", "candidate"}
    assert report.profile_reports[1].profile.memory_enabled is False
    assert report.profile_reports[1].profile.allowed_tools == ["read_file"]
    assert report.profile_reports[1].task_results[0].runtime_overrides["tool_policy"] == "read_only"
    assert report.profile_reports[1].task_results[0].replay is not None
    assert report.profile_reports[1].task_results[0].replay.metrics["profile_policy_violations"] == 1
    assert "success rate" in report.to_markdown().lower()
    assert regression.baseline_profile == "baseline"
    assert regression.candidate_profile == "candidate"


def test_evaluation_report_hash_ignores_run_id_and_generated_at(tmp_path: Path) -> None:
    trace = _trace(tmp_path, run_id="run_report_hash")
    runtime = EvaluationRuntime(project_root=tmp_path, output_root=tmp_path / "evals")
    profile = EvaluationProfile(name="baseline", model="gpt-a")

    first = runtime.run_suite(
        tasks=[_task()],
        profiles=[profile],
        trace_run_dir=trace.store.run_dir,
        run_id="run_one",
    )
    second = runtime.run_suite(
        tasks=[_task()],
        profiles=[profile],
        trace_run_dir=trace.store.run_dir,
        run_id="run_two",
    )

    assert first.run_id != second.run_id
    assert first.report_hash() == second.report_hash()


def test_runtime_blocks_archive_snapshot_execution_without_direct_unpack(tmp_path: Path) -> None:
    task = BenchmarkTask(
        task_id="task.archive",
        version="v1",
        title="Archive snapshot",
        input_prompt="Evaluate archive snapshot.",
        workspace_snapshot=WorkspaceSnapshot(
            kind=WorkspaceSnapshotKind.ARCHIVE_PATH,
            archive_path=tmp_path / "snapshot.zip",
        ),
        expected_outcomes=[ExpectedOutcome(kind=ExpectedOutcomeKind.HEURISTIC, weight=1.0)],
        tags=["easy"],
    )
    report = EvaluationRuntime(project_root=tmp_path).run_suite(
        tasks=[task],
        profiles=[EvaluationProfile(name="baseline", model="gpt-a")],
        execute=True,
    )
    result = report.profile_reports[0].task_results[0]

    assert result.scoring.status == "failure"
    assert "archive_snapshot_requires_controlled_restore" in result.scoring.failure_reasons
    assert not (tmp_path / "snapshot").exists()


class _StubCommandRuntime:
    def __init__(self) -> None:
        self.requests = []

    def run(self, request):
        self.requests.append(request)

        class Result:
            semantic_status = SemanticStatus.SUCCEEDED
            exit_code = 0
            error_code = None
            duration_ms = 1
            combined_output_preview = '{"score_delta": 0.25}'
            policy_decision = CommandPolicyResult(
                decision=CommandDecision.ALLOW,
                reasons=[],
                risk_tags=[CommandRisk.PROJECT_VERIFICATION],
            )

        return Result()


class _TimeoutRecordingCommandRuntime:
    def __init__(self) -> None:
        self.requests = []

    def run(self, request):
        self.requests.append(request)

        class Result:
            semantic_status = SemanticStatus.RUNTIME_FAILED
            exit_code = None
            error_code = "timeout"
            duration_ms = int((request.timeout_seconds or 0) * 1000)
            combined_output_preview = "timeout"
            policy_decision = None

        return Result()


def test_score_adjustment_hook_changes_final_score_only_when_executed(tmp_path: Path) -> None:
    task = BenchmarkTask(
        task_id="task.adjust",
        version="v1",
        title="Score adjustment",
        input_prompt="Evaluate adjustment.",
        workspace_snapshot=WorkspaceSnapshot(kind=WorkspaceSnapshotKind.GIT_REF, git_ref="HEAD"),
        expected_outcomes=[
            ExpectedOutcome(kind=ExpectedOutcomeKind.HEURISTIC, weight=1.0, heuristic="custom", metadata={"score": 0.4})
        ],
        evaluation_hooks=[
            EvaluationHook(name="adjust", stage="score_adjustment", command="adjust")
        ],
        tags=["easy"],
    )
    profile = EvaluationProfile(name="baseline", model="gpt-a")
    offline = EvaluationRuntime(project_root=tmp_path).run_suite(
        tasks=[task],
        profiles=[profile],
        execute=False,
    )
    executed = EvaluationRuntime(
        project_root=tmp_path,
        command_runtime=_StubCommandRuntime(),
    ).run_suite(
        tasks=[task],
        profiles=[profile],
        execute=True,
    )

    assert offline.profile_reports[0].task_results[0].scoring.score == 0.4
    assert executed.profile_reports[0].task_results[0].scoring.score == 0.65


def test_evaluation_hook_args_and_timeout_are_used_for_execution(tmp_path: Path) -> None:
    command = _TimeoutRecordingCommandRuntime()
    task = BenchmarkTask(
        task_id="task.hook_timeout",
        version="v1",
        title="Hook timeout",
        input_prompt="Evaluate hook timeout.",
        workspace_snapshot=WorkspaceSnapshot(kind=WorkspaceSnapshotKind.GIT_REF, git_ref="HEAD"),
        expected_outcomes=[
            ExpectedOutcome(kind=ExpectedOutcomeKind.HEURISTIC, weight=1.0, heuristic="custom", metadata={"score": 0.5})
        ],
        evaluation_hooks=[
            EvaluationHook(
                name="adjust",
                stage="score_adjustment",
                command="adjust",
                args={"score_delta": 0.25},
                timeout_seconds=3,
            )
        ],
        tags=["easy"],
    )

    result = EvaluationRuntime(
        project_root=tmp_path,
        command_runtime=command,
    ).run_suite(
        tasks=[task],
        profiles=[EvaluationProfile(name="baseline", model="gpt-a")],
        execute=True,
    ).profile_reports[0].task_results[0]

    assert command.requests[0].timeout_seconds == 3
    assert result.execution_evidence["hook_results"][0]["args"] == {"score_delta": 0.25}
    assert "timeout" in result.scoring.failure_reasons


def test_evaluation_hook_args_are_executed_as_stable_argv(tmp_path: Path) -> None:
    command = _StubCommandRuntime()
    task = BenchmarkTask(
        task_id="task.hook_args",
        version="v1",
        title="Hook args",
        input_prompt="Evaluate hook args.",
        workspace_snapshot=WorkspaceSnapshot(kind=WorkspaceSnapshotKind.GIT_REF, git_ref="HEAD"),
        expected_outcomes=[
            ExpectedOutcome(kind=ExpectedOutcomeKind.HEURISTIC, weight=1.0, heuristic="custom", metadata={"score": 0.5})
        ],
        evaluation_hooks=[
            EvaluationHook(
                name="adjust",
                stage="score_adjustment",
                module="hooks.adjust",
                args={"name": "two words", "retries": 2, "enabled": True, "skip": False},
                timeout_seconds=5,
            )
        ],
        tags=["easy"],
    )

    EvaluationRuntime(
        project_root=tmp_path,
        command_runtime=command,
    ).run_suite(
        tasks=[task],
        profiles=[EvaluationProfile(name="baseline", model="gpt-a")],
        execute=True,
    )

    assert command.requests[0].argv == [
        "python",
        "-m",
        "hooks.adjust",
        "--name",
        "two words",
        "--retries",
        "2",
        "--enabled",
    ]
    assert command.requests[0].shell is None
    assert command.requests[0].timeout_seconds == 5


def test_evaluation_profiles_do_not_share_runtime_overrides(tmp_path: Path) -> None:
    task = _task()
    baseline = EvaluationProfile(name="baseline", model="gpt-a", tool_policy="read_write")
    candidate = EvaluationProfile(
        name="candidate",
        model="gpt-b",
        memory_enabled=False,
        allowed_tools=["read_file"],
        tool_policy="read_only",
    )

    report = EvaluationRuntime(project_root=tmp_path).run_suite(
        tasks=[task],
        profiles=[baseline, candidate],
        execute=False,
    )

    baseline_overrides = report.profile_reports[0].task_results[0].runtime_overrides
    candidate_overrides = report.profile_reports[1].task_results[0].runtime_overrides
    assert baseline_overrides == baseline.to_runtime_overrides()
    assert candidate_overrides == candidate.to_runtime_overrides()
    assert baseline_overrides is not candidate_overrides


def test_scoring_marks_failed_policy_and_verification_as_failure() -> None:
    result = ScoringEngine().score(
        task=_task(),
        verification={"status": "failed", "passed": 0, "failed": 1},
        heuristics={"patch_quality": 0.4},
        trace_metrics={"policy_denials": 1, "interventions": 2},
    )

    assert result.status == "failure"
    assert result.score < 0.5
    assert "verification_failed" in result.failure_reasons
    assert "policy_denials" in result.failure_reasons


def _contract_task(task_id: str = "phase1j.create_file_smoke_verify") -> BenchmarkTask:
    payload = _task(task_id).to_dict()
    payload["golden_contract"] = {
        "scenario": "create_file_smoke_verify",
        "expected_files": ["quicksort.py", "tests/test_quicksort.py"],
        "expected_commands": ["python -m pytest tests/test_quicksort.py"],
        "expected_evidence": ["file_created", "verification_passed", "final_report_written"],
        "expected_report_sections": ["Goal", "Changes", "Verification", "Risks"],
        "required_trace_artifacts": ["diff", "verification", "report"],
    }
    payload["expected_outcomes"] = [
        {
            "kind": "assertion",
            "weight": 0.3,
            "assertion": "file_exists:quicksort.py",
        },
        {
            "kind": "diff",
            "weight": 0.2,
            "expected_diff": {"paths": ["quicksort.py"], "max_changed_lines": 100},
        },
        {
            "kind": "test",
            "weight": 0.3,
            "command": "python -m pytest tests/test_quicksort.py",
        },
        {
            "kind": "heuristic",
            "weight": 0.2,
            "heuristic": "patch_quality",
        },
    ]
    return BenchmarkTask.from_dict(payload)


def test_evaluation_report_includes_golden_contract_evidence(tmp_path: Path) -> None:
    task = _contract_task()
    report = EvaluationRuntime(project_root=tmp_path).run_suite(
        tasks=[task],
        profiles=[EvaluationProfile(name="baseline", model="gpt-a")],
        execute=False,
    )
    result = report.profile_reports[0].task_results[0]
    payload = report.to_dict()
    markdown = report.to_markdown()

    contract = result.execution_evidence["golden_contract"]
    assert contract["scenario"] == "create_file_smoke_verify"
    assert contract["expected_files"][0]["path"] == "quicksort.py"
    assert contract["expected_commands"][0]["command"] == "python -m pytest tests/test_quicksort.py"
    assert contract["expected_evidence"][0]["name"] == "file_created"
    assert contract["expected_report_sections"][0]["section"] == "Goal"
    assert contract["required_trace_artifacts"][0]["kind"] == "diff"
    assert payload["profile_reports"][0]["task_results"][0]["execution_evidence"]["golden_contract"] == contract
    assert "## Golden Task Evidence" in markdown
    assert "phase1j.create_file_smoke_verify" in markdown
    assert "quicksort.py" in markdown
    assert "python -m pytest tests/test_quicksort.py" in markdown


def test_regression_report_binds_each_regression_to_trace_artifact_ref(tmp_path: Path) -> None:
    task = _contract_task("phase1j.modify_bug_test_pass")
    baseline = EvaluationRuntime(project_root=tmp_path).run_suite(
        tasks=[task],
        profiles=[EvaluationProfile(name="baseline", model="gpt-a")],
        trace_run_dir=_trace(tmp_path, run_id="baseline_trace").store.run_dir,
    )
    candidate = EvaluationRuntime(project_root=tmp_path).run_suite(
        tasks=[task],
        profiles=[EvaluationProfile(name="candidate", model="gpt-b", allowed_tools=[])],
        execute=False,
    )

    regression = RegressionDetector().compare(
        baseline.profile_reports[0],
        candidate.profile_reports[0],
        threshold=0.0,
    )

    assert regression.regressions
    task_regressions = [
        item
        for item in regression.regressions
        if item.get("task_id") == "phase1j.modify_bug_test_pass"
    ]
    assert task_regressions
    for item in task_regressions:
        assert item["trace_artifact_ref"].startswith("regression:")
    assert "trace artifact" in regression.to_markdown().lower()


def test_write_regression_report_persists_trace_artifact_per_regression(tmp_path: Path) -> None:
    task = _contract_task("phase1j.trace_artifact_regression")
    baseline = EvaluationRuntime(project_root=tmp_path).run_suite(
        tasks=[task],
        profiles=[EvaluationProfile(name="baseline", model="gpt-a")],
        trace_run_dir=_trace(tmp_path, run_id="trace_artifact_baseline").store.run_dir,
    )
    candidate = EvaluationRuntime(project_root=tmp_path).run_suite(
        tasks=[task],
        profiles=[EvaluationProfile(name="candidate", model="gpt-b")],
        execute=False,
    )
    regression = RegressionDetector().compare(
        baseline.profile_reports[0],
        candidate.profile_reports[0],
        threshold=0.0,
    )
    trace = TraceRuntime.create(tmp_path, run_id="regression_artifacts")
    runtime = EvaluationRuntime(
        project_root=tmp_path,
        output_root=tmp_path / "evals",
        trace_runtime=trace,
    )

    runtime.write_regression_report(run_id="regression_artifacts", regression=regression)

    regression_refs = {
        artifact.metadata.get("trace_artifact_ref")
        for artifact in trace.store.artifacts()
        if artifact.metadata.get("artifact_type") == "evaluation_regression"
    }
    expected_refs = {item["trace_artifact_ref"] for item in regression.regressions}
    assert expected_refs
    assert expected_refs.issubset(regression_refs)


def test_offline_golden_suite_does_not_scan_workspace_for_diff_outcomes(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    task = _contract_task("phase1j.offline_fast")

    def fail_if_scanned(_root: Path):
        raise AssertionError("offline evaluation should not scan the workspace")

    monkeypatch.setattr(
        "singularity.evaluation.execution._capture_text_snapshot",
        fail_if_scanned,
    )

    report = EvaluationRuntime(project_root=tmp_path).run_suite(
        tasks=[task],
        profiles=[EvaluationProfile(name="baseline", model="gpt-a")],
        execute=False,
    )
    result = report.profile_reports[0].task_results[0]

    assert result.execution_evidence["diff"]["status"] == "blocked"
    assert "diff_requires_execution" in result.scoring.failure_reasons
