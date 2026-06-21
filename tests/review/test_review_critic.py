from __future__ import annotations

import json

from singularity.model import ModelMessage, ModelTurnResult, ModelTurnStatus
from singularity.review import ModelCritic, ReviewCategory, ReviewReport, ReviewStage, ReviewTarget


class FakeModelRuntime:
    def __init__(self, text: str | Exception) -> None:
        self.text = text
        self.requests = []

    def run_turn(self, request):
        self.requests.append(request)
        if isinstance(self.text, Exception):
            raise self.text
        return ModelTurnResult(
            request_id=request.request_id,
            response_id="resp_1",
            status=ModelTurnStatus.SUCCESS,
            assistant_message=ModelMessage.assistant_text(self.text),
        )


def base_report() -> ReviewReport:
    return ReviewReport(
        target=ReviewTarget(stage=ReviewStage.POST_PATCH),
        input_summary="summary",
        evidence=[],
        findings=[],
    )


def test_model_critic_parses_machine_readable_findings() -> None:
    text = json.dumps(
        {
            "findings": [
                {
                    "title": "Missing tests",
                    "severity": "warning",
                    "category": "test_gap",
                    "evidence": ["No targeted tests mapped."],
                    "recommendation": "Add a targeted test.",
                    "blocking": False,
                }
            ]
        }
    )

    outcome = ModelCritic(FakeModelRuntime(text)).review(base_report(), bundle={"summary": "x"})

    assert outcome.status == "ok"
    assert outcome.findings[0].category == ReviewCategory.TEST_GAP


def test_model_critic_unavailable_and_invalid_do_not_block_rules() -> None:
    unavailable = ModelCritic(None).review(base_report(), bundle={})
    invalid = ModelCritic(FakeModelRuntime("not json")).review(base_report(), bundle={})

    assert unavailable.status == "model_critic_unavailable"
    assert unavailable.findings[0].blocking is False
    assert invalid.status == "model_critic_invalid"
    assert invalid.findings[0].blocking is False


def test_model_critic_runtime_failure_degrades_to_non_blocking_finding() -> None:
    outcome = ModelCritic(FakeModelRuntime(RuntimeError("provider down"))).review(base_report(), bundle={})

    assert outcome.status == "model_critic_unavailable"
    assert outcome.findings[0].category == ReviewCategory.VERIFICATION_GAP
