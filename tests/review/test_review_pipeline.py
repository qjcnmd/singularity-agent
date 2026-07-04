from __future__ import annotations

import inspect
import json
from pathlib import Path

from singularity.model import ModelMessage, ModelTurnResult, ModelTurnStatus
from singularity.observability import TraceRecorder
from singularity.review import ReviewDecisionAction, ReviewPipeline


def test_pre_edit_review_emits_trace_and_blocks_validation_review(tmp_path: Path) -> None:
    trace = TraceRecorder.create(tmp_path, trace_dir=tmp_path / "traces")
    component = ReviewPipeline(tmp_path, trace=trace, enable_model_critic=False)

    report = component.pre_edit_review(
        intent={"summary": "large rewrite"},
        validation={
            "ok": False,
            "requires_review": True,
            "failure_category": "over_modification",
            "issues": [{"code": "diff_budget", "message": "Too many changed lines."}],
        },
        patch={"id": "patch_1", "touched_paths": ["app.py"]},
    )

    assert report.decision.action == ReviewDecisionAction.REPLAN
    assert report.findings[0].blocking is True
    events = [json.loads(line) for line in trace.path.read_text(encoding="utf-8").splitlines()]
    event_types = {event["event_type"] for event in events}
    assert {"review.started", "review.finding", "review.decision", "review.completed"} <= event_types
    assert all(event["component"] == "review" for event in events)
    completed = next(event for event in events if event["event_type"] == "review.completed")
    assert completed["payload"]["duration_ms"] >= 0
    assert completed["payload"]["critic_duration_ms"] == 0


def test_review_stage_methods_delegate_common_review_execution() -> None:
    assert hasattr(ReviewPipeline, "_run_stage_review")

    pipeline_source = inspect.getsource(ReviewPipeline)
    assert "def pre_edit_review" in pipeline_source
    assert "def post_patch_review" in pipeline_source
    assert "def post_verification_review" in pipeline_source
    assert "def final_review" in pipeline_source


def test_post_verification_review_repairs_failed_checks(tmp_path: Path) -> None:
    component = ReviewPipeline(tmp_path, enable_model_critic=False)

    report = component.post_verification_review(
        verification={
            "plan": {"verification_plan_id": "vplan_1"},
            "check_status": [{"check_id": "check_1", "kind": "unit_test", "status": "failed"}],
            "failed_checks": [
                {
                    "check_id": "check_1",
                    "kind": "unit_test",
                    "status": "failed",
                    "failure_type": "unit_test_failure",
                }
            ],
            "completion_assessment": {"status": "failed", "remaining_risks": ["tests failed"]},
        }
    )

    assert report.decision.action == ReviewDecisionAction.REPAIR
    assert report.decision.repair_targets == ["check_1"]


def test_review_pipeline_sends_structured_reports_to_memory(tmp_path: Path) -> None:
    calls = []

    class FakeMemoryLearningPipeline:
        def ingest_review_report(self, report):
            calls.append(report)

    component = ReviewPipeline(tmp_path, enable_model_critic=False)
    component.memory_pipeline = FakeMemoryLearningPipeline()

    report = component.post_verification_review(
        verification={
            "plan": {"verification_plan_id": "vplan_1"},
            "check_status": [{"check_id": "check_1", "kind": "unit_test", "status": "failed"}],
            "failed_checks": [
                {
                    "check_id": "check_1",
                    "kind": "unit_test",
                    "status": "failed",
                    "failure_type": "unit_test_failure",
                }
            ],
            "completion_assessment": {"status": "failed", "remaining_risks": ["tests failed"]},
        }
    )

    assert calls == [report]


def test_review_pipeline_memory_ingest_failure_is_fail_open(tmp_path: Path) -> None:
    class BrokenMemoryLearningPipeline:
        def ingest_review_report(self, report):
            raise RuntimeError("memory unavailable")

    component = ReviewPipeline(tmp_path, enable_model_critic=False)
    component.memory_pipeline = BrokenMemoryLearningPipeline()

    report = component.post_verification_review(
        verification={
            "completion_assessment": {"status": "failed", "remaining_risks": ["tests failed"]},
            "failed_checks": [{"check_id": "check_1", "status": "failed"}],
        }
    )

    assert report.decision.action == ReviewDecisionAction.REPAIR
    assert any(item.source == "memory_ingest" for item in report.evidence)


def test_review_pipeline_model_critic_request_inherits_run_identifiers(tmp_path: Path) -> None:
    requests = []

    class FakeModelRunner:
        def run_turn(self, request):
            requests.append(request)
            return ModelTurnResult(
                request_id=request.request_id,
                response_id="resp_critic",
                status=ModelTurnStatus.SUCCESS,
                assistant_message=ModelMessage.assistant_text('{"findings": []}'),
            )

    class Planner:
        session_id = "session_critic"
        task_id = "task_critic"
        state = type("State", (), {"current_phase": "review_phase"})()

    trace = TraceRecorder.create(tmp_path, trace_dir=tmp_path / "traces")
    component = ReviewPipeline(
        tmp_path,
        trace=trace,
        planner=Planner(),
        model_runner=FakeModelRunner(),
        enable_model_critic=True,
    )

    report = component.post_verification_review(
        verification={
            "plan": {"verification_plan_id": "verify_1"},
            "completion_assessment": {"status": "ready"},
        }
    )

    assert report.model_critic_status == "ok"
    assert requests
    request = requests[0]
    assert request.run_id == trace.run_id
    assert request.session_id == "session_critic"
    assert request.task_id == "task_critic"
    assert request.phase_id == "review_phase"
    assert request.action_id == "verify_1"


def test_final_review_completed_trace_uses_safe_action_id(tmp_path: Path) -> None:
    trace = TraceRecorder.create(tmp_path, trace_dir=tmp_path / "traces")
    component = ReviewPipeline(tmp_path, trace=trace, enable_model_critic=False)

    report = component.final_review(
        task_state={"task_id": "task_final"},
        task_plan={"plan_id": "plan_final"},
    )

    assert report.target.stage.value == "final"
    events = [json.loads(line) for line in trace.path.read_text(encoding="utf-8").splitlines()]
    completed = [
        event
        for event in events
        if event["event_type"] == "review.completed" and event["payload"]["review_stage"] == "final"
    ]
    assert completed[-1]["action_id"] == "plan_final"


def test_final_review_model_critic_request_uses_safe_action_id(tmp_path: Path) -> None:
    requests = []

    class FakeModelRunner:
        def run_turn(self, request):
            requests.append(request)
            return ModelTurnResult(
                request_id=request.request_id,
                response_id="resp_final_critic",
                status=ModelTurnStatus.SUCCESS,
                assistant_message=ModelMessage.assistant_text('{"findings": []}'),
            )

    trace = TraceRecorder.create(tmp_path, trace_dir=tmp_path / "traces")
    component = ReviewPipeline(
        tmp_path,
        trace=trace,
        model_runner=FakeModelRunner(),
        enable_model_critic=True,
    )

    report = component.final_review(
        task_state={"task_id": "task_final"},
        task_plan={"plan_id": "plan_final"},
    )

    assert report.model_critic_status == "ok"
    assert requests
    assert requests[0].action_id == "plan_final"


def test_review_pipeline_model_critic_bad_json_is_non_blocking(tmp_path: Path) -> None:
    class BadJsonModelRunner:
        def run_turn(self, request):
            return ModelTurnResult(
                request_id=request.request_id,
                response_id="resp_bad",
                status=ModelTurnStatus.SUCCESS,
                assistant_message=ModelMessage.assistant_text("not json"),
            )

    component = ReviewPipeline(tmp_path, model_runner=BadJsonModelRunner(), enable_model_critic=True)

    report = component.post_verification_review(
        verification={"completion_assessment": {"status": "ready"}}
    )

    assert report.model_critic_status == "model_critic_invalid"
    assert any(finding.source == "model_critic" for finding in report.findings)


def test_review_pipeline_model_critic_exception_is_non_blocking(tmp_path: Path) -> None:
    class RaisingModelRunner:
        def run_turn(self, request):
            raise RuntimeError("critic down")

    component = ReviewPipeline(tmp_path, model_runner=RaisingModelRunner(), enable_model_critic=True)

    report = component.post_verification_review(
        verification={"completion_assessment": {"status": "ready"}}
    )

    assert report.model_critic_status == "model_critic_unavailable"
    assert report.model_critic_error == "critic down"


def test_post_patch_reuses_pre_edit_critic_when_evidence_is_unchanged(tmp_path: Path) -> None:
    requests = []

    class FakeModelRunner:
        def run_turn(self, request):
            requests.append(request)
            return ModelTurnResult(
                request_id=request.request_id,
                response_id=f"resp_{len(requests)}",
                status=ModelTurnStatus.SUCCESS,
                assistant_message=ModelMessage.assistant_text('{"findings": []}'),
            )

    trace = TraceRecorder.create(tmp_path, trace_dir=tmp_path / "traces")
    component = ReviewPipeline(
        tmp_path,
        trace=trace,
        model_runner=FakeModelRunner(),
        enable_model_critic=True,
    )
    validation = {
        "ok": True,
        "requires_review": False,
        "changed_files": ["app.py"],
        "issues": [],
        "diff_summary": {"files_changed": 1},
    }
    patch = {"id": "patch_1", "digest": "digest_1", "touched_paths": ["app.py"]}

    pre = component.pre_edit_review(
        intent={"id": "intent_1", "summary": "rename"},
        plan={"id": "plan_1"},
        patch=patch,
        validation=validation,
        code_impact={"risk_level": "low"},
        test_impact={"likely_tests": ["tests/test_app.py"]},
    )
    post = component.post_patch_review(
        edit_result={
            "ok": True,
            "status": "applied",
            "intent_id": "intent_1",
            "patch_candidate_id": "patch_1",
            "patch_digest": "digest_1",
            "changed_files": ["app.py"],
            "changeset_id": "changeset_1",
            "transaction_id": "tx_1",
        },
        mutation_result={
            "ok": True,
            "status": "applied",
            "affected_files": ["app.py"],
            "changeset_id": "changeset_1",
            "transaction_id": "tx_1",
        },
        verification_plan={"verification_plan_id": "verify_1", "changed_files": ["app.py"]},
        code_impact={"risk_level": "low"},
        test_impact={"likely_tests": ["tests/test_app.py"]},
    )

    assert pre.model_critic_status == "ok"
    assert post.model_critic_status == "reused"
    assert post.metadata["critic_reused"] is True
    assert post.metadata["critic_source_status"] == "ok"
    assert post.metadata["critic_skipped_reason"] == "pre_edit_evidence_unchanged"
    assert len(requests) == 1
    events = [json.loads(line) for line in trace.path.read_text(encoding="utf-8").splitlines()]
    completed = [event for event in events if event["event_type"] == "review.completed"]
    assert completed[-1]["payload"]["critic_reused"] is True
    assert completed[-1]["payload"]["critic_source_status"] == "ok"
    assert completed[-1]["payload"]["critic_skipped_reason"] == "pre_edit_evidence_unchanged"
    assert completed[-1]["payload"]["critic_reuse_skip_reason"] == ""


def test_final_review_reuses_ready_post_verification_critic(tmp_path: Path) -> None:
    requests = []

    class FakeModelRunner:
        def run_turn(self, request):
            requests.append(request)
            return ModelTurnResult(
                request_id=request.request_id,
                response_id=f"resp_{len(requests)}",
                status=ModelTurnStatus.SUCCESS,
                assistant_message=ModelMessage.assistant_text('{"findings": []}'),
            )

    trace = TraceRecorder.create(tmp_path, trace_dir=tmp_path / "traces")
    component = ReviewPipeline(
        tmp_path,
        trace=trace,
        model_runner=FakeModelRunner(),
        enable_model_critic=True,
    )
    verification = {
        "plan": {"verification_plan_id": "verify_1", "changeset_id": "changeset_1"},
        "check_status": [{"check_id": "check_1", "kind": "unit_test", "status": "passed"}],
        "failed_checks": [],
        "completion_assessment": {"status": "ready", "warnings": [], "remaining_risks": []},
    }

    post = component.post_verification_review(verification=verification)
    final = component.final_review(
        task_state={"task_id": "task_final", "final_assessment": {"status": "ready"}},
        task_plan={"plan_id": "plan_final"},
        evidence_ledger={
            "verification_results": [verification],
            "review_results": [post.model_dump(mode="json")],
        },
    )

    assert post.model_critic_status == "ok"
    assert final.model_critic_status == "reused"
    assert final.metadata["critic_reused"] is True
    assert final.metadata["critic_skipped_reason"] == "post_verification_evidence_unchanged"
    assert final.metadata["critic_source_status"] == "ok"
    assert len(requests) == 1
    events = [json.loads(line) for line in trace.path.read_text(encoding="utf-8").splitlines()]
    completed = [
        event
        for event in events
        if event["event_type"] == "review.completed" and event["payload"]["review_stage"] == "final"
    ]
    assert completed[-1]["payload"]["critic_reused"] is True
    assert completed[-1]["payload"]["critic_skipped_reason"] == "post_verification_evidence_unchanged"
    assert "raw_prompt" not in completed[-1]["payload"]
    assert "raw_response" not in completed[-1]["payload"]


def test_final_review_does_not_reuse_failed_post_verification_critic(tmp_path: Path) -> None:
    requests = []

    class FakeModelRunner:
        def run_turn(self, request):
            requests.append(request)
            return ModelTurnResult(
                request_id=request.request_id,
                response_id=f"resp_{len(requests)}",
                status=ModelTurnStatus.SUCCESS,
                assistant_message=ModelMessage.assistant_text('{"findings": []}'),
            )

    component = ReviewPipeline(tmp_path, model_runner=FakeModelRunner(), enable_model_critic=True)
    verification = {
        "plan": {"verification_plan_id": "verify_1"},
        "check_status": [{"check_id": "check_1", "kind": "unit_test", "status": "failed"}],
        "failed_checks": [
            {
                "check_id": "check_1",
                "kind": "unit_test",
                "status": "failed",
                "failure_type": "unit_test_failure",
            }
        ],
        "completion_assessment": {"status": "failed", "remaining_risks": ["tests failed"]},
    }

    post = component.post_verification_review(verification=verification)
    final = component.final_review(
        task_state={"task_id": "task_final", "final_assessment": {"status": "failed"}},
        task_plan={"plan_id": "plan_final"},
        evidence_ledger={
            "verification_results": [verification],
            "review_results": [post.model_dump(mode="json")],
        },
    )

    assert post.decision.action == ReviewDecisionAction.REPAIR
    assert final.metadata["critic_reused"] is False
    assert final.metadata["critic_reuse_skip_reason"] == "final_rule_decision_not_accept"
    assert len(requests) == 2


def test_final_review_does_not_reuse_when_final_rules_reject(tmp_path: Path) -> None:
    requests = []

    class FakeModelRunner:
        def run_turn(self, request):
            requests.append(request)
            return ModelTurnResult(
                request_id=request.request_id,
                response_id=f"resp_{len(requests)}",
                status=ModelTurnStatus.SUCCESS,
                assistant_message=ModelMessage.assistant_text('{"findings": []}'),
            )

    component = ReviewPipeline(tmp_path, model_runner=FakeModelRunner(), enable_model_critic=True)
    verification = {
        "plan": {"verification_plan_id": "verify_1"},
        "check_status": [{"check_id": "check_1", "kind": "unit_test", "status": "passed"}],
        "failed_checks": [],
        "completion_assessment": {"status": "ready", "warnings": [], "remaining_risks": []},
    }

    post = component.post_verification_review(verification=verification)
    stale_verification = {
        "plan": {"verification_plan_id": "verify_1"},
        "check_status": [{"check_id": "check_1", "kind": "unit_test", "status": "passed"}],
        "failed_checks": [],
        "completion_assessment": {"status": "not_run", "warnings": [], "remaining_risks": []},
    }
    final = component.final_review(
        task_state={"task_id": "task_final", "final_assessment": {"status": "ready"}},
        task_plan={"plan_id": "plan_final"},
        evidence_ledger={
            "verification_results": [stale_verification],
            "review_results": [post.model_dump(mode="json")],
        },
    )

    assert final.decision.action != ReviewDecisionAction.ACCEPT
    assert final.metadata["critic_reused"] is False
    assert final.metadata["critic_reuse_skip_reason"] == "final_rule_decision_not_accept"
    assert len(requests) == 2


def test_final_review_does_not_reuse_post_verification_from_another_task(tmp_path: Path) -> None:
    requests = []

    class FakeModelRunner:
        def run_turn(self, request):
            requests.append(request)
            return ModelTurnResult(
                request_id=request.request_id,
                response_id=f"resp_{len(requests)}",
                status=ModelTurnStatus.SUCCESS,
                assistant_message=ModelMessage.assistant_text('{"findings": []}'),
            )

    class Planner:
        session_id = "session_critic"
        task_id = "task_1"
        state = type("State", (), {"current_phase": "review_phase"})()

    planner = Planner()
    component = ReviewPipeline(
        tmp_path,
        planner=planner,
        model_runner=FakeModelRunner(),
        enable_model_critic=True,
    )
    verification = {
        "plan": {"verification_plan_id": "verify_1"},
        "check_status": [{"check_id": "check_1", "kind": "unit_test", "status": "passed"}],
        "failed_checks": [],
        "completion_assessment": {"status": "ready", "warnings": [], "remaining_risks": []},
    }

    post = component.post_verification_review(verification=verification)
    planner.task_id = "task_2"
    final = component.final_review(
        task_state={"task_id": "task_2", "final_assessment": {"status": "ready"}},
        task_plan={"plan_id": "plan_final"},
        evidence_ledger={
            "verification_results": [verification],
            "review_results": [post.model_dump(mode="json")],
        },
    )

    assert final.metadata["critic_reused"] is False
    assert final.metadata["critic_reuse_skip_reason"] == "post_verification_task_changed"
    assert len(requests) == 2


def test_non_reusable_post_verification_clears_final_reuse_reference(tmp_path: Path) -> None:
    requests = []

    class FakeModelRunner:
        def run_turn(self, request):
            requests.append(request)
            return ModelTurnResult(
                request_id=request.request_id,
                response_id=f"resp_{len(requests)}",
                status=ModelTurnStatus.SUCCESS,
                assistant_message=ModelMessage.assistant_text('{"findings": []}'),
            )

    component = ReviewPipeline(tmp_path, model_runner=FakeModelRunner(), enable_model_critic=True)
    ready_verification = {
        "plan": {"verification_plan_id": "verify_1"},
        "check_status": [{"check_id": "check_1", "kind": "unit_test", "status": "passed"}],
        "failed_checks": [],
        "completion_assessment": {"status": "ready", "warnings": [], "remaining_risks": []},
    }
    failed_verification = {
        "plan": {"verification_plan_id": "verify_2"},
        "check_status": [{"check_id": "check_2", "kind": "unit_test", "status": "failed"}],
        "failed_checks": [
            {
                "check_id": "check_2",
                "kind": "unit_test",
                "status": "failed",
                "failure_type": "unit_test_failure",
            }
        ],
        "completion_assessment": {"status": "failed", "remaining_risks": ["tests failed"]},
    }

    component.post_verification_review(verification=ready_verification)
    component.post_verification_review(verification=failed_verification)
    final = component.final_review(
        task_state={"task_id": "task_final", "final_assessment": {"status": "ready"}},
        task_plan={"plan_id": "plan_final"},
        evidence_ledger={"verification_results": [ready_verification]},
    )

    assert final.metadata["critic_reused"] is False
    assert final.metadata["critic_reuse_skip_reason"] == "post_verification_reference_missing"
    assert len(requests) == 3


def test_post_patch_reuse_matches_pre_edit_patch_touched_paths(tmp_path: Path) -> None:
    requests = []

    class FakeModelRunner:
        def run_turn(self, request):
            requests.append(request)
            return ModelTurnResult(
                request_id=request.request_id,
                response_id=f"resp_{len(requests)}",
                status=ModelTurnStatus.SUCCESS,
                assistant_message=ModelMessage.assistant_text('{"findings": []}'),
            )

    component = ReviewPipeline(tmp_path, model_runner=FakeModelRunner(), enable_model_critic=True)
    patch = {"id": "patch_1", "digest": "digest_1", "touched_paths": ["app.py"]}

    component.pre_edit_review(
        intent={"id": "intent_1", "summary": "rename"},
        plan={"id": "plan_1"},
        patch=patch,
        validation={"ok": True, "requires_review": False, "issues": []},
        code_impact={"risk_level": "low"},
        test_impact={"likely_tests": ["tests/test_app.py"]},
    )
    post = component.post_patch_review(
        edit_result={
            "ok": True,
            "status": "applied",
            "intent_id": "intent_1",
            "patch_candidate_id": "patch_1",
            "patch_digest": "digest_1",
            "changed_files": ["app.py"],
        },
        mutation_result={"ok": True, "status": "applied", "affected_files": ["app.py"]},
        verification_plan={"verification_plan_id": "verify_1", "changed_files": ["app.py"]},
        code_impact={"risk_level": "low"},
        test_impact={"likely_tests": ["tests/test_app.py"]},
    )

    assert post.model_critic_status == "reused"
    assert post.metadata["critic_reused"] is True
    assert len(requests) == 1


def test_disabled_model_critic_is_not_reported_as_reused(tmp_path: Path) -> None:
    component = ReviewPipeline(tmp_path, enable_model_critic=False)
    validation = {
        "ok": True,
        "requires_review": False,
        "changed_files": ["app.py"],
        "issues": [],
    }
    patch = {"id": "patch_1", "digest": "digest_1", "touched_paths": ["app.py"]}

    pre = component.pre_edit_review(
        intent={"id": "intent_1", "summary": "rename"},
        plan={"id": "plan_1"},
        patch=patch,
        validation=validation,
        code_impact={"risk_level": "low"},
        test_impact={"likely_tests": ["tests/test_app.py"]},
    )
    post = component.post_patch_review(
        edit_result={
            "ok": True,
            "status": "applied",
            "intent_id": "intent_1",
            "patch_candidate_id": "patch_1",
            "patch_digest": "digest_1",
            "changed_files": ["app.py"],
        },
        mutation_result={"ok": True, "status": "applied", "affected_files": ["app.py"]},
        verification_plan={"verification_plan_id": "verify_1", "changed_files": ["app.py"]},
        code_impact={"risk_level": "low"},
        test_impact={"likely_tests": ["tests/test_app.py"]},
    )

    assert pre.model_critic_status == "disabled"
    assert post.model_critic_status == "disabled"
    assert post.metadata["critic_reused"] is False
    assert post.metadata["critic_skipped_reason"] == ""


def test_post_patch_does_not_reuse_pre_edit_critic_when_mutation_failed(tmp_path: Path) -> None:
    requests = []

    class FakeModelRunner:
        def run_turn(self, request):
            requests.append(request)
            return ModelTurnResult(
                request_id=request.request_id,
                response_id=f"resp_{len(requests)}",
                status=ModelTurnStatus.SUCCESS,
                assistant_message=ModelMessage.assistant_text('{"findings": []}'),
            )

    component = ReviewPipeline(tmp_path, model_runner=FakeModelRunner(), enable_model_critic=True)
    validation = {
        "ok": True,
        "requires_review": False,
        "changed_files": ["app.py"],
        "issues": [],
    }
    patch = {"id": "patch_1", "digest": "digest_1", "touched_paths": ["app.py"]}

    component.pre_edit_review(
        intent={"id": "intent_1", "summary": "rename"},
        plan={"id": "plan_1"},
        patch=patch,
        validation=validation,
        code_impact={"risk_level": "low"},
        test_impact={"likely_tests": ["tests/test_app.py"]},
    )
    post = component.post_patch_review(
        edit_result={
            "ok": False,
            "status": "failed",
            "intent_id": "intent_1",
            "patch_candidate_id": "patch_1",
            "patch_digest": "digest_1",
            "changed_files": ["app.py"],
        },
        mutation_result={"ok": False, "status": "failed", "affected_files": ["app.py"]},
        verification_plan={"verification_plan_id": "verify_1", "changed_files": ["app.py"]},
        code_impact={"risk_level": "low"},
        test_impact={"likely_tests": ["tests/test_app.py"]},
    )

    assert post.model_critic_status == "ok"
    assert post.metadata["critic_reused"] is False
    assert post.metadata["critic_reuse_skip_reason"] == "risk_or_result_requires_review"
    assert len(requests) == 2


def test_unavailable_model_critic_is_not_cached_or_reused(tmp_path: Path) -> None:
    requests = []

    class FakeModelRunner:
        def run_turn(self, request):
            requests.append(request)
            return ModelTurnResult(
                request_id=request.request_id,
                response_id=f"resp_{len(requests)}",
                status=ModelTurnStatus.FAILED,
                error="provider timeout",
            )

    component = ReviewPipeline(tmp_path, model_runner=FakeModelRunner(), enable_model_critic=True)
    validation = {
        "ok": True,
        "requires_review": False,
        "changed_files": ["app.py"],
        "issues": [],
    }
    patch = {"id": "patch_1", "digest": "digest_1", "touched_paths": ["app.py"]}

    pre = component.pre_edit_review(
        intent={"id": "intent_1", "summary": "rename"},
        plan={"id": "plan_1"},
        patch=patch,
        validation=validation,
        code_impact={"risk_level": "low"},
        test_impact={"likely_tests": ["tests/test_app.py"]},
    )
    post = component.post_patch_review(
        edit_result={
            "ok": True,
            "status": "applied",
            "intent_id": "intent_1",
            "patch_candidate_id": "patch_1",
            "patch_digest": "digest_1",
            "changed_files": ["app.py"],
        },
        mutation_result={"ok": True, "status": "applied", "affected_files": ["app.py"]},
        verification_plan={"verification_plan_id": "verify_1", "changed_files": ["app.py"]},
        code_impact={"risk_level": "low"},
        test_impact={"likely_tests": ["tests/test_app.py"]},
    )

    assert pre.model_critic_status == "model_critic_unavailable"
    assert post.model_critic_status == "model_critic_unavailable"
    assert post.metadata["critic_reused"] is False
    assert len(requests) == 2
