from __future__ import annotations

from miniharness.code_index.models import TestImpactAnalysis


def verification_scope_from_test_impact(impact: TestImpactAnalysis) -> dict[str, object]:
    return {
        "changed_files": impact.changed_files,
        "likely_tests": impact.likely_tests,
        "commands": impact.commands,
        "require_full_test": impact.require_full_test,
        "freshness": impact.freshness.value,
        "confidence": impact.confidence,
        "confidence_note": impact.confidence_note,
    }
