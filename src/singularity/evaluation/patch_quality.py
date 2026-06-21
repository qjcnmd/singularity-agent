from __future__ import annotations

from typing import Any

from singularity.evaluation.models import PatchQualityResult


class PatchQualityEvaluator:
    def evaluate(
        self,
        *,
        diff_summary: list[dict[str, Any]] | None = None,
        verification: dict[str, Any] | None = None,
    ) -> PatchQualityResult:
        diff_summary = diff_summary or []
        verification = verification or {}
        added = sum(int(item.get("added_lines", 0) or 0) for item in diff_summary)
        removed = sum(int(item.get("removed_lines", 0) or 0) for item in diff_summary)
        files_changed = len({str(item.get("path", "")) for item in diff_summary if item.get("path")})
        total_changed = added + removed
        redundant_code = any(bool(item.get("redundant_code")) for item in diff_summary)
        complexity = sum(float(item.get("complexity", 0) or 0) for item in diff_summary)
        tests_passed = str(verification.get("status", "")).lower() in {
            "ready",
            "passed",
            "success",
            "ready_with_warnings",
        }
        warnings: list[str] = []
        score = 1.0
        if total_changed > 80:
            score -= 0.25
            warnings.append("large_diff")
        if total_changed > 160:
            score -= 0.15
        if files_changed > 8:
            score -= 0.15
            warnings.append("wide_diff")
        if complexity > 20:
            score -= 0.15
            warnings.append("complex_diff")
        if redundant_code:
            score -= 0.2
            warnings.append("redundant_code")
        if not tests_passed:
            score -= 0.25
            warnings.append("tests_not_passed")
        minimal_change = total_changed <= 80 and files_changed <= 5 and not redundant_code
        if not minimal_change:
            score -= 0.05
        metrics = {
            "added_lines": added,
            "removed_lines": removed,
            "changed_lines": total_changed,
            "files_changed": files_changed,
            "complexity": complexity,
            "minimal_change": minimal_change,
            "redundant_code": redundant_code,
            "tests_passed": tests_passed,
        }
        return PatchQualityResult(
            score=round(max(0.0, min(1.0, score)), 4),
            metrics=metrics,
            warnings=warnings,
        )
