from __future__ import annotations

from typing import Any

from singularity.evaluation.models import PatchQualityResult

TEST_PASS_STATUSES = {"ready", "passed", "success", "ready_with_warnings"}
LARGE_DIFF_LINE_THRESHOLD = 80
VERY_LARGE_DIFF_LINE_THRESHOLD = 160
WIDE_DIFF_FILE_THRESHOLD = 8
COMPLEX_DIFF_THRESHOLD = 20
MINIMAL_CHANGE_FILE_THRESHOLD = 5
LARGE_DIFF_PENALTY = 0.25
VERY_LARGE_DIFF_PENALTY = 0.15
WIDE_DIFF_PENALTY = 0.15
COMPLEX_DIFF_PENALTY = 0.15
REDUNDANT_CODE_PENALTY = 0.2
TEST_FAILURE_PENALTY = 0.25
NON_MINIMAL_CHANGE_PENALTY = 0.05


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
        tests_passed = str(verification.get("status", "")).lower() in TEST_PASS_STATUSES
        warnings: list[str] = []
        score = 1.0
        if total_changed > LARGE_DIFF_LINE_THRESHOLD:
            score -= LARGE_DIFF_PENALTY
            warnings.append("large_diff")
        if total_changed > VERY_LARGE_DIFF_LINE_THRESHOLD:
            score -= VERY_LARGE_DIFF_PENALTY
        if files_changed > WIDE_DIFF_FILE_THRESHOLD:
            score -= WIDE_DIFF_PENALTY
            warnings.append("wide_diff")
        if complexity > COMPLEX_DIFF_THRESHOLD:
            score -= COMPLEX_DIFF_PENALTY
            warnings.append("complex_diff")
        if redundant_code:
            score -= REDUNDANT_CODE_PENALTY
            warnings.append("redundant_code")
        if not tests_passed:
            score -= TEST_FAILURE_PENALTY
            warnings.append("tests_not_passed")
        minimal_change = (
            total_changed <= LARGE_DIFF_LINE_THRESHOLD
            and files_changed <= MINIMAL_CHANGE_FILE_THRESHOLD
            and not redundant_code
        )
        if not minimal_change:
            score -= NON_MINIMAL_CHANGE_PENALTY
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
