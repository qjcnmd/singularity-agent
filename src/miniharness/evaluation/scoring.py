from __future__ import annotations

from typing import Any

from miniharness.evaluation.models import (
    BenchmarkTask,
    ExpectedOutcomeKind,
    ScoringResult,
)


class ScoringEngine:
    def score(
        self,
        *,
        task: BenchmarkTask,
        verification: dict[str, Any] | None = None,
        assertions: dict[str, Any] | None = None,
        diff: dict[str, Any] | None = None,
        heuristics: dict[str, float] | None = None,
        trace_metrics: dict[str, Any] | None = None,
    ) -> ScoringResult:
        verification = verification or {}
        assertions = assertions or {}
        diff = diff or {}
        heuristics = heuristics or {}
        trace_metrics = trace_metrics or {}
        by_kind: dict[ExpectedOutcomeKind, list[float]] = {}
        weights: dict[ExpectedOutcomeKind, float] = {}
        evidence: list[dict[str, Any]] = []

        for outcome in task.expected_outcomes:
            kind = outcome.kind
            weights[kind] = weights.get(kind, 0.0) + outcome.weight
            subscore = self._score_outcome(
                kind=kind,
                outcome_key=outcome.heuristic,
                verification=verification,
                assertions=assertions,
                diff=diff,
                heuristics=heuristics,
            )
            by_kind.setdefault(kind, []).append(subscore)
            evidence.append(
                {
                    "kind": kind.value,
                    "weight": outcome.weight,
                    "score": subscore,
                    "source": outcome.command or outcome.assertion or outcome.heuristic,
                }
            )

        subscores: dict[str, float] = {
            kind.value: round(sum(values) / max(1, len(values)), 4)
            for kind, values in by_kind.items()
        }
        weighted_total = 0.0
        total_weight = 0.0
        for kind, weight in weights.items():
            weighted_total += subscores[kind.value] * weight
            total_weight += weight
        raw_score = weighted_total / total_weight if total_weight else 0.0

        failure_reasons = self._failure_reasons(verification, trace_metrics)
        if trace_metrics.get("interventions", 0):
            raw_score -= min(0.2, 0.05 * float(trace_metrics.get("interventions", 0)))
        if trace_metrics.get("policy_denials", 0):
            raw_score -= 0.25
        score = round(max(0.0, min(1.0, raw_score)), 4)
        status = "success" if score >= 0.5 and not failure_reasons else "failure"
        return ScoringResult(
            task_id=task.task_id,
            status=status,
            score=score,
            subscores=subscores,
            evidence=evidence,
            failure_reasons=failure_reasons,
        )

    def _score_outcome(
        self,
        *,
        kind: ExpectedOutcomeKind,
        outcome_key: str | None,
        verification: dict[str, Any],
        assertions: dict[str, Any],
        diff: dict[str, Any],
        heuristics: dict[str, float],
    ) -> float:
        if kind == ExpectedOutcomeKind.TEST:
            return _verification_score(verification)
        if kind == ExpectedOutcomeKind.ASSERTION:
            return _ratio_score(assertions, default_key="passed")
        if kind == ExpectedOutcomeKind.DIFF:
            return _ratio_score(diff, default_key="matched")
        if kind == ExpectedOutcomeKind.HEURISTIC:
            if outcome_key and outcome_key in heuristics:
                return _clamp(float(heuristics[outcome_key]))
            if "patch_quality" in heuristics:
                return _clamp(float(heuristics["patch_quality"]))
            if heuristics:
                return _clamp(sum(float(value) for value in heuristics.values()) / len(heuristics))
            return 0.0
        return 0.0

    def _failure_reasons(
        self,
        verification: dict[str, Any],
        trace_metrics: dict[str, Any],
    ) -> list[str]:
        reasons: list[str] = []
        status = str(verification.get("status", "")).lower()
        if status in {"failed", "failure", "blocked"} or int(verification.get("failed", 0) or 0) > 0:
            reasons.append("verification_failed")
        if int(trace_metrics.get("policy_denials", 0) or 0) > 0:
            reasons.append("policy_denials")
        if int(trace_metrics.get("tool_failures", 0) or 0) > 0:
            reasons.append("tool_failures")
        return reasons


def _verification_score(verification: dict[str, Any]) -> float:
    status = str(verification.get("status", "")).lower()
    if status in {"ready", "passed", "success", "ready_with_warnings"}:
        return 1.0
    passed = int(verification.get("passed", 0) or 0)
    failed = int(verification.get("failed", 0) or 0)
    total = passed + failed
    if total:
        return _clamp(passed / total)
    return 0.0


def _ratio_score(payload: dict[str, Any], *, default_key: str) -> float:
    if default_key in payload:
        return 1.0 if bool(payload[default_key]) else 0.0
    passed = int(payload.get("passed", 0) or 0)
    failed = int(payload.get("failed", 0) or 0)
    total = passed + failed
    return _clamp(passed / total) if total else 0.0


def _clamp(value: float) -> float:
    return max(0.0, min(1.0, value))
