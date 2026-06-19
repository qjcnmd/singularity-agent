from __future__ import annotations

import json
from pathlib import Path

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
