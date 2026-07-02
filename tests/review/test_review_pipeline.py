from __future__ import annotations

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
