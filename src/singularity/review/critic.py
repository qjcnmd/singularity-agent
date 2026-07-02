from __future__ import annotations

import json
from dataclasses import dataclass, field
from typing import Any

from singularity.review.evidence import to_bounded_plain
from singularity.review.models import ReviewCategory, ReviewFinding, ReviewFindings, ReviewReport, ReviewSeverity
from singularity.review.structured_output import BusinessRuleViolation, call_review_output


@dataclass(frozen=True)
class ModelCriticOutcome:
    status: str
    findings: list[ReviewFinding] = field(default_factory=list)
    error: str | None = None
    metadata: dict[str, Any] = field(default_factory=dict)


class ModelCritic:
    def __init__(self, model_runner: Any | None) -> None:
        self.model_runner = model_runner

    def review(
        self,
        report: ReviewReport,
        *,
        bundle: dict[str, Any],
        request_context: dict[str, Any] | None = None,
    ) -> ModelCriticOutcome:
        if self.model_runner is None:
            return ModelCriticOutcome(
                status="model_critic_unavailable",
                findings=[_fallback_finding("Model critic unavailable", "No model runner was configured.")],
                error="model_runner_missing",
                metadata=_metadata(output_mode="rule_only", fallback_reason="model_runner_missing"),
            )
        try:
            from singularity.model.models import ModelPurpose

            ids = dict(request_context or {})
            result = call_review_output(
                model_runner=self.model_runner,
                request_base={
                    "request_id": str(ids.get("request_id") or f"critic_{report.review_id}"),
                    "run_id": str(ids.get("run_id") or report.review_id),
                    "session_id": str(ids.get("session_id") or report.review_id),
                    "task_id": str(ids.get("task_id") or report.target.task_id or report.review_id),
                    "phase_id": str(ids.get("phase_id") or report.target.stage.value),
                    "action_id": str(
                        ids.get("action_id")
                        or report.target.verification_id
                        or report.target.patch_id
                        or report.review_id
                    ),
                    "purpose": ModelPurpose.CLASSIFY_ERROR,
                    "context_metadata": {
                        "review_id": report.review_id,
                        "review_stage": report.target.stage.value,
                    },
                    "max_output_tokens": 1200,
                },
                prompt=_critic_prompt(report, bundle),
                output_model=ReviewFindings,
                schema_name="review_findings",
                tool_name="submit_review_findings",
                tool_description="Submit model-assisted review findings that conform to the ReviewFindings JSON Schema.",
                business_validator=_validate_review_findings_business_rules,
            )
            if result.status != "ok":
                invalid_reasons = {"schema_validation_failed", "business_rule_validation_failed"}
                reason = str(result.metadata.get("fallback_reason") or "")
                status = "model_critic_invalid" if reason in invalid_reasons else "model_critic_unavailable"
                finding = (
                    _invalid_finding(result.error or "invalid model critic output")
                    if status == "model_critic_invalid"
                    else _fallback_finding(
                        "Model critic unavailable",
                        result.error or "Review output boundary used the rule-only fallback path.",
                    )
                )
                return ModelCriticOutcome(
                    status=status,
                    findings=[finding],
                    error=result.error,
                    metadata=result.metadata,
                )
            findings = [
                ReviewFinding.model_validate({**finding, "source": "model_critic"})
                for finding in result.payload.get("findings") or []
            ]
            return ModelCriticOutcome(status="ok", findings=findings, metadata=result.metadata)
        except Exception as exc:
            return ModelCriticOutcome(
                status="model_critic_unavailable",
                findings=[_fallback_finding("Model critic unavailable", str(exc))],
                error=str(exc),
                metadata=_metadata(output_mode="rule_only", fallback_reason="provider_error"),
            )

    def _parse_findings(self, text: str) -> list[ReviewFinding]:
        payload = _load_json_payload(text)
        result = ReviewFindings.model_validate(payload)
        return [
            ReviewFinding.model_validate({**finding.model_dump(mode="json"), "source": "model_critic"})
            for finding in result.findings
        ]


class InvalidCriticOutput(ValueError):
    pass


def _critic_prompt(report: ReviewReport, bundle: dict[str, Any]) -> str:
    payload = {
        "instruction": (
            "Review this local Singularity change bundle. Use the output boundary requested by the caller. "
            "Return a ReviewFindings JSON object with title, severity, category, location, evidence, "
            "recommendation, blocking, confidence. Return {\"findings\": []} when there are no findings."
        ),
        "terminology": {
            "standard_terms": [
                "Structured Outputs",
                "tool calling",
                "tool choice",
                "JSON Schema",
                "schema validation",
                "bounded retry",
                "fallback path",
                "graceful degradation",
                "fail-closed",
                "deterministic gate",
                "hard gate",
                "model-assisted review",
            ],
            "internal_object_names": [
                "ReviewPipeline",
                "ModelCritic",
                "ReviewDecisionEngine",
                "FinalReviewer",
                "CompletionGate",
                "EvidenceLedger",
                "ReviewFinding",
                "ReviewReport",
            ],
        },
        "allowed_severities": ["info", "warning", "error", "critical"],
        "allowed_categories": [
            "goal_mismatch",
            "over_editing",
            "bug_risk",
            "test_gap",
            "architecture_regression",
            "security_risk",
            "maintainability",
            "style",
            "verification_gap",
            "policy_risk",
        ],
        "rule_report": report.model_dump(mode="json"),
        "bundle": to_bounded_plain(bundle, max_chars=2400),
    }
    return json.dumps(payload, ensure_ascii=False, sort_keys=True, default=str)


def _load_json_payload(text: str) -> dict[str, Any]:
    stripped = text.strip()
    if not stripped:
        raise InvalidCriticOutput("empty critic response")
    try:
        payload = json.loads(stripped)
    except json.JSONDecodeError as original:
        decoder = json.JSONDecoder()
        for index, char in enumerate(stripped):
            if char not in "[{":
                continue
            try:
                payload, _ = decoder.raw_decode(stripped[index:])
                break
            except json.JSONDecodeError:
                continue
        else:
            raise InvalidCriticOutput(str(original)) from original
    if isinstance(payload, list):
        return {"findings": payload}
    if isinstance(payload, dict) and isinstance(payload.get("report"), dict):
        report = payload["report"]
        if isinstance(report.get("findings"), list):
            return {"findings": report["findings"]}
    if not isinstance(payload, dict):
        raise InvalidCriticOutput("critic output must be a JSON object")
    return payload


def _validate_review_findings_business_rules(payload: dict[str, Any]) -> None:
    for finding in payload.get("findings") or []:
        if not isinstance(finding, dict):
            continue
        source = str(finding.get("source") or "model_critic")
        evidence_text = " ".join(str(item).lower() for item in finding.get("evidence") or [])
        if source not in {"model_critic", "rules", ""} or "evaluator-only" in evidence_text:
            raise BusinessRuleViolation("model-assisted review referenced evaluator-only or unauthorized evidence")


def _metadata(
    *,
    output_mode: str,
    fallback_reason: str,
    schema_validation_passed: bool = False,
    retry_count: int = 0,
    retry_reason: str = "none",
) -> dict[str, Any]:
    return {
        "output_mode": output_mode,
        "schema_validation_passed": schema_validation_passed,
        "retry_count": retry_count,
        "retry_reason": retry_reason,
        "fallback_reason": fallback_reason,
    }


def _fallback_finding(title: str, detail: str) -> ReviewFinding:
    return ReviewFinding(
        title=title,
        severity=ReviewSeverity.INFO,
        category=ReviewCategory.VERIFICATION_GAP,
        evidence=[detail],
        recommendation="Continue with deterministic rule review; the model-assisted review used the rule-only fallback path.",
        blocking=False,
        confidence=0.4,
        source="model_critic",
    )


def _invalid_finding(detail: str) -> ReviewFinding:
    return ReviewFinding(
        title="Model critic returned invalid output",
        severity=ReviewSeverity.INFO,
        category=ReviewCategory.VERIFICATION_GAP,
        evidence=[detail],
        recommendation="Ignore model critic output and continue with deterministic rule review.",
        blocking=False,
        confidence=0.4,
        source="model_critic",
    )
