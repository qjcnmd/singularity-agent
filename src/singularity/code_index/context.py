from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any

from singularity.code_index.models import ContextCandidate, IndexSummary, RelevantFileCandidate, TrustLevel


@dataclass(frozen=True)
class ProjectIndexObservation:
    index_id: str
    summary: dict[str, Any]
    relevant_files: list[dict[str, Any]] = field(default_factory=list)
    context_candidates: list[dict[str, Any]] = field(default_factory=list)
    impact: dict[str, Any] | None = None
    test_impact: dict[str, Any] | None = None
    warnings: list[str] = field(default_factory=list)
    trust_level: str = TrustLevel.WORKSPACE_UNTRUSTED.value
    truncated: bool = False

    def to_dict(self) -> dict[str, Any]:
        return {
            "index_id": self.index_id,
            "summary": self.summary,
            "relevant_files": self.relevant_files,
            "context_candidates": self.context_candidates,
            "impact": self.impact,
            "test_impact": self.test_impact,
            "warnings": self.warnings,
            "trust_level": self.trust_level,
            "truncated": self.truncated,
        }


def build_project_index_observation(
    *,
    index_id: str,
    summary: IndexSummary,
    relevant_files: list[RelevantFileCandidate] | None = None,
    context_candidates: list[ContextCandidate] | None = None,
    impact: Any | None = None,
    test_impact: Any | None = None,
    max_items: int = 20,
) -> ProjectIndexObservation:
    relevant = [item.to_dict() for item in (relevant_files or [])[:max_items]]
    context = [item.to_dict() for item in (context_candidates or [])[:max_items]]
    truncated = len(relevant_files or []) > max_items or len(context_candidates or []) > max_items
    warnings = list(summary.limitations)
    if summary.freshness.value != "fresh":
        warnings.append(f"Index freshness is {summary.freshness.value}; consumers must treat results as stale.")
    return ProjectIndexObservation(
        index_id=index_id,
        summary=summary.to_dict(),
        relevant_files=relevant,
        context_candidates=context,
        impact=impact.to_dict() if hasattr(impact, "to_dict") else impact,
        test_impact=test_impact.to_dict() if hasattr(test_impact, "to_dict") else test_impact,
        warnings=warnings,
        truncated=truncated,
    )
