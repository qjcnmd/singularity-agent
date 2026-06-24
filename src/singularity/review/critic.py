from __future__ import annotations

import json
from dataclasses import dataclass, field
from typing import Any

from pydantic import ValidationError

from singularity.review.evidence import to_bounded_plain
from singularity.review.models import ReviewCategory, ReviewFinding, ReviewReport, ReviewSeverity


@dataclass(frozen=True)
class ModelCriticOutcome:
    status: str
    findings: list[ReviewFinding] = field(default_factory=list)
    error: str | None = None


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
                findings=[_degraded_finding("Model critic unavailable", "No model runner was configured.")],
                error="model_runner_missing",
            )
        try:
            from singularity.model.models import ModelPurpose, ModelTurnRequest, ModelTurnStatus

            ids = dict(request_context or {})
            request_id = str(ids.get("request_id") or f"critic_{report.review_id}")
            request = ModelTurnRequest(
                request_id=request_id,
                run_id=str(ids.get("run_id") or report.review_id),
                session_id=str(ids.get("session_id") or report.review_id),
                task_id=str(ids.get("task_id") or report.target.task_id or report.review_id),
                phase_id=str(ids.get("phase_id") or report.target.stage.value),
                action_id=str(ids.get("action_id") or report.target.verification_id or report.target.patch_id or report.review_id),
                purpose=ModelPurpose.CLASSIFY_ERROR,
                messages=[
                    {"role": "user", "content": _critic_prompt(report, bundle)}
                ],
                context_metadata={
                    "review_id": report.review_id,
                    "review_stage": report.target.stage.value,
                },
            )
            result = self.model_runner.run_turn(request)
            if getattr(result, "status", None) != ModelTurnStatus.SUCCESS:
                return ModelCriticOutcome(
                    status="model_critic_unavailable",
                    findings=[_degraded_finding("Model critic unavailable", f"Model status={getattr(result, 'status', None)}.")],
                    error=str(getattr(result, "error", None) or getattr(result, "status", None)),
                )
            text = getattr(getattr(result, "assistant_message", None), "text", "") or ""
            findings = self._parse_findings(text)
            return ModelCriticOutcome(status="ok", findings=findings)
        except InvalidCriticOutput as exc:
            return ModelCriticOutcome(
                status="model_critic_invalid",
                findings=[_invalid_finding(str(exc))],
                error=str(exc),
            )
        except Exception as exc:
            return ModelCriticOutcome(
                status="model_critic_unavailable",
                findings=[_degraded_finding("Model critic unavailable", str(exc))],
                error=str(exc),
            )

    def _parse_findings(self, text: str) -> list[ReviewFinding]:
        try:
            payload = json.loads(text)
        except json.JSONDecodeError as exc:
            raise InvalidCriticOutput(str(exc)) from exc
        if isinstance(payload, list):
            raw_findings = payload
        elif isinstance(payload, dict) and isinstance(payload.get("findings"), list):
            raw_findings = payload["findings"]
        elif isinstance(payload, dict) and isinstance(payload.get("report"), dict):
            raw_findings = payload["report"].get("findings") or []
        else:
            raise InvalidCriticOutput("critic output must contain findings")
        try:
            return [
                ReviewFinding.model_validate({**finding, "source": "model_critic"})
                for finding in raw_findings
            ]
        except ValidationError as exc:
            raise InvalidCriticOutput(str(exc)) from exc


class InvalidCriticOutput(ValueError):
    pass


def _critic_prompt(report: ReviewReport, bundle: dict[str, Any]) -> str:
    payload = {
        "instruction": (
            "Review this local Singularity change bundle. Return only JSON with a "
            "'findings' list using title, severity, category, location, evidence, "
            "recommendation, blocking, confidence. Do not return prose."
        ),
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


def _degraded_finding(title: str, detail: str) -> ReviewFinding:
    return ReviewFinding(
        title=title,
        severity=ReviewSeverity.INFO,
        category=ReviewCategory.VERIFICATION_GAP,
        evidence=[detail],
        recommendation="Continue with deterministic rule review; model critic evidence was not available.",
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
