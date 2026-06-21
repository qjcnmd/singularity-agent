from __future__ import annotations

import json
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from singularity.evaluation.models import (
    EvaluationProfile,
    ScoringResult,
    TraceReplayResult,
    canonical_json,
    now_iso,
)


@dataclass(frozen=True)
class TaskEvaluationResult:
    task_id: str
    profile: EvaluationProfile
    scoring: ScoringResult
    replay: TraceReplayResult | None = None
    patch_quality: dict[str, Any] = field(default_factory=dict)
    runtime_overrides: dict[str, Any] = field(default_factory=dict)
    execution_evidence: dict[str, Any] = field(default_factory=dict)
    latency_ms: int = 0
    cost: float = 0.0
    tool_calls: int = 0
    intervention_count: int = 0

    def to_dict(self) -> dict[str, Any]:
        return {
            "task_id": self.task_id,
            "profile": self.profile.to_dict(),
            "scoring": self.scoring.to_dict(),
            "replay": self.replay.to_dict() if self.replay else None,
            "patch_quality": self.patch_quality,
            "runtime_overrides": self.runtime_overrides,
            "execution_evidence": self.execution_evidence,
            "latency_ms": self.latency_ms,
            "cost": self.cost,
            "tool_calls": self.tool_calls,
            "intervention_count": self.intervention_count,
        }


@dataclass(frozen=True)
class ProfileEvaluationReport:
    profile: EvaluationProfile
    task_results: list[TaskEvaluationResult]
    metrics: dict[str, Any]

    def to_dict(self) -> dict[str, Any]:
        return {
            "profile": self.profile.to_dict(),
            "task_results": [item.to_dict() for item in self.task_results],
            "metrics": self.metrics,
        }


@dataclass(frozen=True)
class EvaluationReport:
    run_id: str
    generated_at: str
    profile_reports: list[ProfileEvaluationReport]
    metrics: dict[str, Any]
    output_dir: Path | None = None

    def to_dict(self) -> dict[str, Any]:
        return {
            "schema_version": "evaluation.report/v1",
            "run_id": self.run_id,
            "generated_at": self.generated_at,
            "profile_reports": [item.to_dict() for item in self.profile_reports],
            "metrics": self.metrics,
            "output_dir": str(self.output_dir) if self.output_dir else None,
            "report_hash": self.report_hash(),
        }

    def to_json(self) -> str:
        return json.dumps(self.to_dict(), ensure_ascii=False, indent=2, sort_keys=True) + "\n"

    def to_markdown(self) -> str:
        lines = [
            f"# Evaluation Report `{self.run_id}`",
            "",
            f"- generated_at: `{self.generated_at}`",
            f"- success rate: {self.metrics.get('success_rate', 0):.2f}",
            f"- average score: {self.metrics.get('average_score', 0):.2f}",
            f"- cost: {self.metrics.get('cost', 0):.4f}",
            f"- latency_ms: {self.metrics.get('latency_ms', 0)}",
            f"- tool calls: {self.metrics.get('tool_calls', 0)}",
            f"- intervention rate: {self.metrics.get('intervention_rate', 0):.2f}",
            "",
            "## Profiles",
        ]
        for profile_report in self.profile_reports:
            metrics = profile_report.metrics
            lines.extend(
                [
                    "",
                    f"### {profile_report.profile.name}",
                    "",
                    f"- model: `{profile_report.profile.model}`",
                    f"- prompt profile: `{profile_report.profile.prompt_profile}`",
                    f"- memory enabled: `{str(profile_report.profile.memory_enabled).lower()}`",
                    f"- tool policy: `{profile_report.profile.tool_policy}`",
                    f"- success rate: {metrics.get('success_rate', 0):.2f}",
                    f"- average score: {metrics.get('average_score', 0):.2f}",
                    f"- cost: {metrics.get('cost', 0):.4f}",
                    f"- latency_ms: {metrics.get('latency_ms', 0)}",
                    f"- tool calls: {metrics.get('tool_calls', 0)}",
                    f"- intervention rate: {metrics.get('intervention_rate', 0):.2f}",
                    "",
                    "| task | status | score | failures |",
                    "| --- | --- | ---: | --- |",
                ]
            )
            for item in profile_report.task_results:
                failures = ", ".join(item.scoring.failure_reasons) or "-"
                lines.append(
                    f"| `{item.task_id}` | {item.scoring.status} | {item.scoring.score:.2f} | {failures} |"
                )
        return "\n".join(lines) + "\n"

    def write(self, output_dir: Path | str) -> None:
        output_dir = Path(output_dir)
        output_dir.mkdir(parents=True, exist_ok=True)
        (output_dir / "report.json").write_text(self.to_json(), encoding="utf-8")
        (output_dir / "report.md").write_text(self.to_markdown(), encoding="utf-8")

    def report_hash(self) -> str:
        import hashlib

        payload = {
            "profile_reports": [item.to_dict() for item in self.profile_reports],
            "metrics": self.metrics,
        }
        return hashlib.sha256(canonical_json(payload).encode("utf-8")).hexdigest()


@dataclass(frozen=True)
class RegressionReport:
    baseline_profile: str
    candidate_profile: str
    threshold: float
    blocking: bool
    regressions: list[dict[str, Any]]
    summary: dict[str, Any]
    task_diffs: list[dict[str, Any]] = field(default_factory=list)

    def to_dict(self) -> dict[str, Any]:
        return {
            "schema_version": "evaluation.regression_report/v1",
            "baseline_profile": self.baseline_profile,
            "candidate_profile": self.candidate_profile,
            "threshold": self.threshold,
            "blocking": self.blocking,
            "regressions": self.regressions,
            "summary": self.summary,
            "task_diffs": self.task_diffs,
        }

    def to_json(self) -> str:
        return json.dumps(self.to_dict(), ensure_ascii=False, indent=2, sort_keys=True) + "\n"

    def to_markdown(self) -> str:
        lines = [
            f"# Regression Report `{self.baseline_profile}` vs `{self.candidate_profile}`",
            "",
            f"- threshold: {self.threshold:.2f}",
            f"- blocking: {str(self.blocking).lower()}",
            f"- regressions: {len(self.regressions)}",
            "",
            "| metric | baseline | candidate | delta |",
            "| --- | ---: | ---: | ---: |",
        ]
        for item in self.regressions:
            lines.append(
                f"| {item['metric']} | {item['baseline']} | {item['candidate']} | {item['delta']} |"
            )
        if self.task_diffs:
            lines.extend(
                [
                    "",
                    "## Per-Task Diff",
                    "",
                    "| task | baseline | candidate | score delta |",
                    "| --- | --- | --- | ---: |",
                ]
            )
            for item in self.task_diffs:
                lines.append(
                    "| "
                    f"`{item['task_id']}` | "
                    f"{item['baseline_status']} ({item['baseline_score']}) | "
                    f"{item['candidate_status']} ({item['candidate_score']}) | "
                    f"{item['score_delta']} |"
                )
        return "\n".join(lines) + "\n"

    def write(self, output_dir: Path | str) -> None:
        output_dir = Path(output_dir)
        output_dir.mkdir(parents=True, exist_ok=True)
        (output_dir / "regression.json").write_text(self.to_json(), encoding="utf-8")
        (output_dir / "regression.md").write_text(self.to_markdown(), encoding="utf-8")


def metrics_for_results(results: list[TaskEvaluationResult]) -> dict[str, Any]:
    total = len(results)
    successes = len([item for item in results if item.scoring.status == "success"])
    score_total = sum(item.scoring.score for item in results)
    interventions = sum(item.intervention_count for item in results)
    return {
        "task_count": total,
        "success_count": successes,
        "failure_count": total - successes,
        "success_rate": round(successes / total, 4) if total else 0.0,
        "average_score": round(score_total / total, 4) if total else 0.0,
        "cost": round(sum(item.cost for item in results), 6),
        "latency_ms": sum(item.latency_ms for item in results),
        "tool_calls": sum(item.tool_calls for item in results),
        "intervention_count": interventions,
        "intervention_rate": round(interventions / total, 4) if total else 0.0,
    }


def new_report(
    *,
    run_id: str,
    profile_reports: list[ProfileEvaluationReport],
    output_dir: Path | None = None,
) -> EvaluationReport:
    all_results = [
        result
        for profile_report in profile_reports
        for result in profile_report.task_results
    ]
    return EvaluationReport(
        run_id=run_id,
        generated_at=now_iso(),
        profile_reports=profile_reports,
        metrics=metrics_for_results(all_results),
        output_dir=output_dir,
    )
