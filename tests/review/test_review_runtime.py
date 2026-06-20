from __future__ import annotations

import json
from pathlib import Path

from miniharness.model import ModelMessage, ModelTurnResult, ModelTurnStatus
from miniharness.observability import TraceRuntime
from miniharness.review import ReviewDecisionAction, ReviewRuntime


def test_pre_edit_review_emits_trace_and_blocks_validation_review(tmp_path: Path) -> None:
    trace = TraceRuntime.create(tmp_path, trace_dir=tmp_path / "traces")
    runtime = ReviewRuntime(tmp_path, trace=trace, enable_model_critic=False)

    report = runtime.pre_edit_review(
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
    assert all(event["runtime"] == "review" for event in events)


def test_post_verification_review_repairs_failed_checks(tmp_path: Path) -> None:
    runtime = ReviewRuntime(tmp_path, enable_model_critic=False)

    report = runtime.post_verification_review(
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


def test_review_runtime_sends_structured_reports_to_memory(tmp_path: Path) -> None:
    calls = []

    class FakeMemoryRuntime:
        def ingest_review_report(self, report):
            calls.append(report)

    runtime = ReviewRuntime(tmp_path, enable_model_critic=False)
    runtime.memory_runtime = FakeMemoryRuntime()

    report = runtime.post_verification_review(
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


def test_review_runtime_memory_ingest_failure_is_fail_open(tmp_path: Path) -> None:
    class BrokenMemoryRuntime:
        def ingest_review_report(self, report):
            raise RuntimeError("memory unavailable")

    runtime = ReviewRuntime(tmp_path, enable_model_critic=False)
    runtime.memory_runtime = BrokenMemoryRuntime()

    report = runtime.post_verification_review(
        verification={
            "completion_assessment": {"status": "failed", "remaining_risks": ["tests failed"]},
            "failed_checks": [{"check_id": "check_1", "status": "failed"}],
        }
    )

    assert report.decision.action == ReviewDecisionAction.REPAIR
    assert any(item.source == "memory_ingest" for item in report.evidence)


def test_review_runtime_model_critic_request_inherits_run_identifiers(tmp_path: Path) -> None:
    requests = []

    class FakeModelRuntime:
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

    trace = TraceRuntime.create(tmp_path, trace_dir=tmp_path / "traces")
    runtime = ReviewRuntime(
        tmp_path,
        trace=trace,
        planner=Planner(),
        model_runtime=FakeModelRuntime(),
        enable_model_critic=True,
    )

    report = runtime.post_verification_review(
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


def test_review_runtime_model_critic_bad_json_is_non_blocking(tmp_path: Path) -> None:
    class BadJsonModelRuntime:
        def run_turn(self, request):
            return ModelTurnResult(
                request_id=request.request_id,
                response_id="resp_bad",
                status=ModelTurnStatus.SUCCESS,
                assistant_message=ModelMessage.assistant_text("not json"),
            )

    runtime = ReviewRuntime(tmp_path, model_runtime=BadJsonModelRuntime(), enable_model_critic=True)

    report = runtime.post_verification_review(
        verification={"completion_assessment": {"status": "ready"}}
    )

    assert report.model_critic_status == "model_critic_invalid"
    assert any(finding.source == "model_critic" for finding in report.findings)


def test_review_runtime_model_critic_exception_is_non_blocking(tmp_path: Path) -> None:
    class RaisingModelRuntime:
        def run_turn(self, request):
            raise RuntimeError("critic down")

    runtime = ReviewRuntime(tmp_path, model_runtime=RaisingModelRuntime(), enable_model_critic=True)

    report = runtime.post_verification_review(
        verification={"completion_assessment": {"status": "ready"}}
    )

    assert report.model_critic_status == "model_critic_unavailable"
    assert report.model_critic_error == "critic down"
