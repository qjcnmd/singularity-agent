from __future__ import annotations

from pathlib import Path

import pytest

from singularity.command import CommandDecision, CommandPolicyResult, CommandRisk, SemanticStatus
from singularity.evaluation import (
    BenchmarkTask,
    EvaluationHarness,
    EvaluationHook,
    EvaluationProfile,
    ExpectedOutcome,
    ExpectedOutcomeKind,
    PatchQualityEvaluator,
    RegressionDetector,
    ScoringEngine,
    TraceReplayHarness,
    WorkspaceSnapshot,
    WorkspaceSnapshotKind,
)
from singularity.observability import TraceEventType, TraceRecorder


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


def _trace(root: Path, run_id: str = "run_eval") -> TraceRecorder:
    trace = TraceRecorder.create(root, run_id=run_id, session_id=f"{run_id}_session")
    trace.emit(
        TraceEventType.MODEL_RESPONSE_RECEIVED,
        component="model",
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
        component="tool",
        summary="read_file completed.",
        ids={"task_id": "task_eval", "action_id": "tool_1"},
        payload={"tool_name": "read_file", "ok": True},
    )
    trace.emit(
        TraceEventType.VERIFICATION_CHECK_COMPLETED,
        component="verification",
        summary="unit tests passed",
        ids={"task_id": "task_eval", "verification_id": "check_1"},
        payload={"status": "passed"},
    )
    trace.emit(
        TraceEventType.TOOL_DISPATCH_COMPLETED,
        component="tool",
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
    replay = TraceReplayHarness(project_root=tmp_path)

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
    replay = TraceReplayHarness(project_root=tmp_path)

    first = replay.replay(first_trace.store.run_dir, profile=profile)
    second = replay.replay(second_trace.store.run_dir, profile=profile)

    assert first.trace_input_digest != second.trace_input_digest
    assert first.result_hash != second.result_hash


def test_trace_replay_simulates_side_effect_events_by_default(tmp_path: Path) -> None:
    trace = _trace(tmp_path, run_id="run_side_effect")
    trace.emit(
        TraceEventType.COMMAND_COMPLETED,
        component="command",
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

    result = TraceReplayHarness(project_root=tmp_path).replay(trace.store.run_dir, profile=profile)

    assert result.replay_classification == "simulated_side_effects"
    assert result.side_effects_simulated == 1
    assert result.metrics["side_effect_events"] == 1


def test_evaluation_harness_runs_ab_and_regression_reports(tmp_path: Path) -> None:
    trace = _trace(tmp_path, run_id="run_ab")
    component = EvaluationHarness(project_root=tmp_path, output_root=tmp_path / "evals")
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

    report = component.run_suite(
        tasks=[_task()],
        profiles=[baseline, candidate],
        trace_run_dir=trace.store.run_dir,
    )
    regression = RegressionDetector().compare(report.profile_reports[0], report.profile_reports[1])

    assert report.metrics["success_rate"] == 0.5
    assert {item.profile.name for item in report.profile_reports} == {"baseline", "candidate"}
    assert report.profile_reports[1].profile.memory_enabled is False
    assert report.profile_reports[1].profile.allowed_tools == ["read_file"]
    assert report.profile_reports[1].task_results[0].agent_config_overrides["tool_policy"] == "read_only"
    assert report.profile_reports[1].task_results[0].replay is not None
    assert report.profile_reports[1].task_results[0].replay.metrics["profile_policy_violations"] == 1
    assert "success rate" in report.to_markdown().lower()
    assert regression.baseline_profile == "baseline"
    assert regression.candidate_profile == "candidate"


def test_evaluation_report_hash_ignores_run_id_and_generated_at(tmp_path: Path) -> None:
    trace = _trace(tmp_path, run_id="run_report_hash")
    component = EvaluationHarness(project_root=tmp_path, output_root=tmp_path / "evals")
    profile = EvaluationProfile(name="baseline", model="gpt-a")

    first = component.run_suite(
        tasks=[_task()],
        profiles=[profile],
        trace_run_dir=trace.store.run_dir,
        run_id="run_one",
    )
    second = component.run_suite(
        tasks=[_task()],
        profiles=[profile],
        trace_run_dir=trace.store.run_dir,
        run_id="run_two",
    )

    assert first.run_id != second.run_id
    assert first.report_hash() == second.report_hash()


def test_evaluation_report_includes_failure_taxonomy_and_previous_comparison(tmp_path: Path) -> None:
    component = EvaluationHarness(project_root=tmp_path, output_root=tmp_path / "evals")
    profile = EvaluationProfile(name="baseline", model="gpt-a")
    first = component.run_suite(
        tasks=[_task("task.first")],
        profiles=[profile],
        trace_run_dir=_trace(tmp_path, run_id="previous_trace").store.run_dir,
        run_id="previous",
        write_report=True,
    )
    second = component.run_suite(
        tasks=[_task("task.second")],
        profiles=[profile],
        run_id="current",
        write_report=True,
    )

    assert first.metrics["success_rate"] == 1.0
    assert second.metrics["failure_taxonomy"]
    assert second.metrics["previous_comparison"] == {
        "previous_run_id": "previous",
        "success_rate_delta": -1.0,
        "average_score_delta": -0.625,
        "cost_delta": -0.05,
        "latency_ms_delta": -1200,
        "tool_calls_delta": -1,
    }
    assert "previous comparison" in second.to_markdown().lower()


def test_evaluation_harness_blocks_archive_snapshot_execution_without_direct_unpack(tmp_path: Path) -> None:
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
    report = EvaluationHarness(project_root=tmp_path).run_suite(
        tasks=[task],
        profiles=[EvaluationProfile(name="baseline", model="gpt-a")],
        execute=True,
    )
    result = report.profile_reports[0].task_results[0]

    assert result.scoring.status == "failure"
    assert "archive_snapshot_requires_controlled_restore" in result.scoring.failure_reasons
    assert not (tmp_path / "snapshot").exists()


class _StubCommandExecutor:
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


class _TimeoutRecordingCommandExecutor:
    def __init__(self) -> None:
        self.requests = []

    def run(self, request):
        self.requests.append(request)

        class Result:
            semantic_status = SemanticStatus.EXECUTION_FAILED
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
    offline = EvaluationHarness(project_root=tmp_path).run_suite(
        tasks=[task],
        profiles=[profile],
        execute=False,
    )
    executed = EvaluationHarness(
        project_root=tmp_path,
        command_executor=_StubCommandExecutor(),
    ).run_suite(
        tasks=[task],
        profiles=[profile],
        execute=True,
    )

    assert offline.profile_reports[0].task_results[0].scoring.score == 0.4
    assert executed.profile_reports[0].task_results[0].scoring.score == 0.65


def test_evaluation_hook_args_and_timeout_are_used_for_execution(tmp_path: Path) -> None:
    command = _TimeoutRecordingCommandExecutor()
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

    result = EvaluationHarness(
        project_root=tmp_path,
        command_executor=command,
    ).run_suite(
        tasks=[task],
        profiles=[EvaluationProfile(name="baseline", model="gpt-a")],
        execute=True,
    ).profile_reports[0].task_results[0]

    assert command.requests[0].timeout_seconds == 3
    assert result.execution_evidence["hook_results"][0]["args"] == {"score_delta": 0.25}
    assert "timeout" in result.scoring.failure_reasons


def test_evaluation_hook_args_are_executed_as_stable_argv(tmp_path: Path) -> None:
    command = _StubCommandExecutor()
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

    EvaluationHarness(
        project_root=tmp_path,
        command_executor=command,
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


def test_evaluation_profiles_do_not_share_agent_config_overrides(tmp_path: Path) -> None:
    task = _task()
    baseline = EvaluationProfile(name="baseline", model="gpt-a", tool_policy="read_write")
    candidate = EvaluationProfile(
        name="candidate",
        model="gpt-b",
        memory_enabled=False,
        allowed_tools=["read_file"],
        tool_policy="read_only",
    )

    report = EvaluationHarness(project_root=tmp_path).run_suite(
        tasks=[task],
        profiles=[baseline, candidate],
        execute=False,
    )

    baseline_overrides = report.profile_reports[0].task_results[0].agent_config_overrides
    candidate_overrides = report.profile_reports[1].task_results[0].agent_config_overrides
    assert baseline_overrides == baseline.to_agent_config_overrides()
    assert candidate_overrides == candidate.to_agent_config_overrides()
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


def _contract_task(task_id: str = "benchmark.contract.create_file_smoke_verify") -> BenchmarkTask:
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
    report = EvaluationHarness(project_root=tmp_path).run_suite(
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
    assert "benchmark.contract.create_file_smoke_verify" in markdown
    assert "quicksort.py" in markdown
    assert "python -m pytest tests/test_quicksort.py" in markdown


@pytest.mark.parametrize(
    "assertion",
    [
        "file_exists:../outside.txt",
        "file_contains:../outside.txt:secret",
        "json:../outside.json:key:secret",
        "json:config.json:missing:value",
        "file_contains:missing_parts",
    ],
)
def test_evaluation_assertions_fail_closed(tmp_path: Path, assertion: str) -> None:
    project = tmp_path / "project"
    project.mkdir()
    outside = tmp_path / "outside.txt"
    outside.write_text("secret\n", encoding="utf-8")
    (tmp_path / "outside.json").write_text('{"key": "secret"}\n', encoding="utf-8")
    (project / "config.json").write_text('{"key": "value"}\n', encoding="utf-8")
    task = BenchmarkTask(
        task_id="task.outside_assertion",
        version="v1",
        title="Reject outside assertion path",
        input_prompt="Check assertion path containment.",
        workspace_snapshot=WorkspaceSnapshot(kind=WorkspaceSnapshotKind.GIT_REF, git_ref="HEAD"),
        expected_outcomes=[
            ExpectedOutcome(
                kind=ExpectedOutcomeKind.ASSERTION,
                weight=1.0,
                assertion=assertion,
            )
        ],
        tags=["easy"],
    )

    report = EvaluationHarness(project_root=project).run_suite(
        tasks=[task],
        profiles=[EvaluationProfile(name="baseline", model="gpt-a")],
        execute=False,
    )

    assertions = report.profile_reports[0].task_results[0].execution_evidence["assertions"]
    assert assertions["failed"] == 1
    assert assertions["results"][0]["passed"] is False


def test_regression_report_binds_each_regression_to_trace_artifact_ref(tmp_path: Path) -> None:
    task = _contract_task("benchmark.contract.modify_bug_test_pass")
    baseline = EvaluationHarness(project_root=tmp_path).run_suite(
        tasks=[task],
        profiles=[EvaluationProfile(name="baseline", model="gpt-a")],
        trace_run_dir=_trace(tmp_path, run_id="baseline_trace").store.run_dir,
    )
    candidate = EvaluationHarness(project_root=tmp_path).run_suite(
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
        if item.get("task_id") == "benchmark.contract.modify_bug_test_pass"
    ]
    assert task_regressions
    for item in task_regressions:
        assert item["trace_artifact_ref"].startswith("regression:")
    assert "trace artifact" in regression.to_markdown().lower()


def test_write_regression_report_persists_trace_artifact_per_regression(tmp_path: Path) -> None:
    task = _contract_task("benchmark.contract.trace_artifact_regression")
    baseline = EvaluationHarness(project_root=tmp_path).run_suite(
        tasks=[task],
        profiles=[EvaluationProfile(name="baseline", model="gpt-a")],
        trace_run_dir=_trace(tmp_path, run_id="trace_artifact_baseline").store.run_dir,
    )
    candidate = EvaluationHarness(project_root=tmp_path).run_suite(
        tasks=[task],
        profiles=[EvaluationProfile(name="candidate", model="gpt-b")],
        execute=False,
    )
    regression = RegressionDetector().compare(
        baseline.profile_reports[0],
        candidate.profile_reports[0],
        threshold=0.0,
    )
    trace = TraceRecorder.create(tmp_path, run_id="regression_artifacts")
    component = EvaluationHarness(
        project_root=tmp_path,
        output_root=tmp_path / "evals",
        trace_recorder=trace,
    )

    component.write_regression_report(run_id="regression_artifacts", regression=regression)

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
    task = _contract_task("benchmark.contract.offline_fast")

    def fail_if_scanned(_root: Path):
        raise AssertionError("offline evaluation should not scan the workspace")

    monkeypatch.setattr(
        "singularity.evaluation.execution._capture_text_snapshot",
        fail_if_scanned,
    )

    report = EvaluationHarness(project_root=tmp_path).run_suite(
        tasks=[task],
        profiles=[EvaluationProfile(name="baseline", model="gpt-a")],
        execute=False,
    )
    result = report.profile_reports[0].task_results[0]

    assert result.execution_evidence["diff"]["status"] == "blocked"
    assert "diff_requires_execution" in result.scoring.failure_reasons


def test_patch_payload_skips_env_file_content(tmp_path: Path) -> None:
    from singularity.evaluation.runner import _patch_payload

    workspace = tmp_path / "workspace"
    workspace.mkdir()
    (workspace / "app.py").write_text("print('hello')\n", encoding="utf-8")
    (workspace / ".env").write_text(
        "OPENAI_API_KEY=sk-secret-123456789\nDATABASE_URL=postgres://user:pass@host/db\n",
        encoding="utf-8",
    )

    before_snapshot = {"app.py": "", ".env": ""}
    payload = _patch_payload(before_snapshot, workspace)

    diff_text = payload["diff"]
    assert "sk-secret" not in diff_text
    assert "DATABASE_URL" not in diff_text
    assert "postgres://user:pass" not in diff_text
    assert "<sensitive_path>" in payload["changed_files"]
    assert "app.py" in payload["changed_files"]


def test_patch_payload_skips_pem_and_key_files(tmp_path: Path) -> None:
    from singularity.evaluation.runner import _patch_payload

    workspace = tmp_path / "workspace"
    workspace.mkdir()
    (workspace / "server.pem").write_text(
        "-----BEGIN PRIVATE KEY-----\nMIIBVwIBADANBgkqhkiG9w0BAQEFAASCAUEwggE9AgEAAkEA\n-----END PRIVATE KEY-----\n",
        encoding="utf-8",
    )
    (workspace / "private.key").write_text(
        "super_secret_key_material_here\n",
        encoding="utf-8",
    )
    (workspace / "config.py").write_text("DEBUG = True\n", encoding="utf-8")

    before_snapshot = {"server.pem": "", "private.key": "", "config.py": ""}
    payload = _patch_payload(before_snapshot, workspace)

    diff_text = payload["diff"]
    assert "BEGIN PRIVATE KEY" not in diff_text
    assert "super_secret_key_material" not in diff_text
    assert "<sensitive_path>" in payload["changed_files"]
    assert "config.py" in payload["changed_files"]


def test_patch_payload_redacts_secrets_in_non_sensitive_files(tmp_path: Path) -> None:
    from singularity.evaluation.runner import _patch_payload

    workspace = tmp_path / "workspace"
    workspace.mkdir()
    (workspace / "settings.py").write_text(
        'API_KEY = "sk-leaked-abcdefghij"\nTOKEN = "ghp_abcdefghijklmnop"\n',
        encoding="utf-8",
    )

    before_snapshot = {"settings.py": ""}
    payload = _patch_payload(before_snapshot, workspace)

    diff_text = payload["diff"]
    assert "sk-leaked" not in diff_text
    assert "ghp_abcdefghijklmnop" not in diff_text
    assert "settings.py" in payload["changed_files"]


