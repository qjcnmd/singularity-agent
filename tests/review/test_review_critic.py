from __future__ import annotations

import json

from singularity.model import (
    MockModelProvider,
    ModelCapabilities,
    ModelMessage,
    ModelToolCall,
    ModelToolParseStatus,
    ModelTurnResult,
    ModelTurnStatus,
)
from singularity.model.runner import ModelRunner
from singularity.review import ModelCritic, ReviewCategory, ReviewReport, ReviewStage, ReviewTarget
from singularity.tools.registry import ToolRegistry


class FakeModelRunner:
    def __init__(self, text: str | Exception, *, supported_output_modes: set[str] | None = None) -> None:
        self.text = text
        self.supported_output_modes = supported_output_modes
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

    def supports_review_output_mode(self, mode: str) -> bool:
        return self.supported_output_modes is None or mode in self.supported_output_modes


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

    outcome = ModelCritic(FakeModelRunner(text)).review(base_report(), bundle={"summary": "x"})

    assert outcome.status == "ok"
    assert outcome.findings[0].category == ReviewCategory.TEST_GAP
    assert outcome.findings[0].source == "model_critic"
    assert outcome.metadata["output_mode"] == "structured_output"
    assert outcome.metadata["schema_validation_passed"] is True


def test_model_critic_accepts_wrapped_json_without_lowering_schema() -> None:
    text = """Here is the JSON:
```json
{"findings": []}
```
"""

    outcome = ModelCritic(FakeModelRunner(text)).review(base_report(), bundle={"summary": "x"})

    assert outcome.status == "ok"
    assert outcome.findings == []


def test_model_critic_unavailable_and_invalid_do_not_block_rules() -> None:
    unavailable = ModelCritic(None).review(base_report(), bundle={})
    invalid = ModelCritic(FakeModelRunner("not json")).review(base_report(), bundle={})

    assert unavailable.status == "model_critic_unavailable"
    assert unavailable.findings[0].blocking is False
    assert invalid.status == "model_critic_invalid"
    assert invalid.findings[0].blocking is False


def test_model_critic_model_runner_failure_degrades_to_non_blocking_finding() -> None:
    outcome = ModelCritic(FakeModelRunner(RuntimeError("provider down"))).review(base_report(), bundle={})

    assert outcome.status == "model_critic_unavailable"
    assert outcome.findings[0].category == ReviewCategory.VERIFICATION_GAP


def test_model_critic_requests_structured_outputs_first() -> None:
    runner = FakeModelRunner('{"findings": []}', supported_output_modes={"structured_output"})

    outcome = ModelCritic(runner).review(base_report(), bundle={})

    assert outcome.status == "ok"
    assert runner.requests[0].model_preferences.structured_output_schema is not None
    assert runner.requests[0].model_preferences.json_mode is False


def test_model_critic_uses_forced_tool_calling_when_structured_outputs_are_unsupported() -> None:
    class ToolRunner(FakeModelRunner):
        def run_turn(self, request):
            self.requests.append(request)
            return ModelTurnResult(
                request_id=request.request_id,
                response_id="resp_tool",
                status=ModelTurnStatus.SUCCESS,
                assistant_message=ModelMessage.assistant_text(""),
                tool_calls=[
                    ModelToolCall(
                        tool_call_id="call_1",
                        tool_name="submit_review_findings",
                        arguments={"findings": []},
                        raw_arguments='{"findings":[]}',
                        parse_status=ModelToolParseStatus.VALID,
                    )
                ],
            )

    runner = ToolRunner("", supported_output_modes={"forced_tool_call"})

    outcome = ModelCritic(runner).review(base_report(), bundle={})

    assert outcome.status == "ok"
    assert outcome.metadata["output_mode"] == "forced_tool_call"
    assert runner.requests[0].tool_choice.tool_name == "submit_review_findings"
    assert runner.requests[0].tools[0].metadata["strict"] is True
    assert "Call exactly one submit_review_findings tool" in runner.requests[0].messages[0].text
    assert "Do not answer in natural language" in runner.requests[0].messages[0].text


def test_model_critic_falls_back_to_json_mode_when_structured_and_tools_are_unsupported() -> None:
    runner = FakeModelRunner('{"findings": []}', supported_output_modes={"json_mode"})

    outcome = ModelCritic(runner).review(base_report(), bundle={})

    assert outcome.status == "ok"
    assert outcome.metadata["output_mode"] == "json_mode"
    assert runner.requests[0].model_preferences.json_mode is True


def test_model_critic_uses_real_model_runner_capabilities_for_output_mode(tmp_path) -> None:
    provider = MockModelProvider(
        text="",
        capabilities=ModelCapabilities(
            supports_tools=True,
            supports_json_mode=True,
            supports_structured_outputs=False,
        ),
        tool_calls=[
            ModelToolCall(
                tool_call_id="call_1",
                tool_name="submit_review_findings",
                arguments={"findings": []},
                raw_arguments='{"findings":[]}',
                parse_status=ModelToolParseStatus.VALID,
            )
        ],
    )
    runner = ModelRunner.with_mock_provider(provider, tool_registry=ToolRegistry(tmp_path))

    outcome = ModelCritic(runner).review(base_report(), bundle={})

    assert outcome.status == "ok"
    assert outcome.metadata["output_mode"] == "forced_tool_call"
    assert provider.requests[0].preferences.structured_output_schema is None
    assert provider.requests[0].tool_choice.tool_name == "submit_review_findings"


def test_model_critic_validation_retry_is_bounded() -> None:
    class RetryRunner(FakeModelRunner):
        def __init__(self) -> None:
            super().__init__("", supported_output_modes={"structured_output"})

        def run_turn(self, request):
            self.requests.append(request)
            text = "{}" if len(self.requests) == 1 else '{"findings": []}'
            return ModelTurnResult(
                request_id=request.request_id,
                response_id=f"resp_{len(self.requests)}",
                status=ModelTurnStatus.SUCCESS,
                assistant_message=ModelMessage.assistant_text(text),
            )

    runner = RetryRunner()

    outcome = ModelCritic(runner).review(base_report(), bundle={})

    assert outcome.status == "ok"
    assert outcome.metadata["retry_count"] == 1
    assert outcome.metadata["retry_reason"] == "schema_validation_error"
    assert len(runner.requests) == 2


def test_model_critic_forced_tool_call_parse_retry_records_reason() -> None:
    class ToolRetryRunner(FakeModelRunner):
        def __init__(self) -> None:
            super().__init__("", supported_output_modes={"forced_tool_call"})

        def run_turn(self, request):
            self.requests.append(request)
            parse_status = (
                ModelToolParseStatus.SCHEMA_MISMATCH
                if len(self.requests) == 1
                else ModelToolParseStatus.VALID
            )
            arguments = {} if len(self.requests) == 1 else {"findings": []}
            return ModelTurnResult(
                request_id=request.request_id,
                response_id=f"resp_tool_{len(self.requests)}",
                status=ModelTurnStatus.SUCCESS,
                assistant_message=ModelMessage.assistant_text(""),
                tool_calls=[
                    ModelToolCall(
                        tool_call_id=f"call_{len(self.requests)}",
                        tool_name="submit_review_findings",
                        arguments=arguments,
                        raw_arguments=json.dumps(arguments),
                        parse_status=parse_status,
                    )
                ],
            )

    runner = ToolRetryRunner()

    outcome = ModelCritic(runner).review(base_report(), bundle={})

    assert outcome.status == "ok"
    assert outcome.metadata["output_mode"] == "forced_tool_call"
    assert outcome.metadata["retry_count"] == 1
    assert outcome.metadata["retry_reason"] == "tool_call_parse_error"
    assert len(runner.requests) == 2


def test_model_critic_business_rule_failure_uses_rule_only_fallback() -> None:
    text = json.dumps(
        {
            "findings": [
                {
                    "title": "Leaks evaluator data",
                    "severity": "warning",
                    "category": "verification_gap",
                    "evidence": ["evaluator-only metadata was used"],
                    "recommendation": "Remove evaluator-only evidence.",
                    "source": "evaluator",
                }
            ]
        }
    )
    runner = FakeModelRunner(text, supported_output_modes={"structured_output"})

    outcome = ModelCritic(runner).review(base_report(), bundle={})

    assert outcome.status == "model_critic_invalid"
    assert outcome.metadata["output_mode"] == "rule_only"
    assert outcome.metadata["fallback_reason"] == "business_rule_validation_failed"
    assert outcome.metadata["retry_reason"] == "business_rule_validation_failed"
    assert len(runner.requests) == 1
