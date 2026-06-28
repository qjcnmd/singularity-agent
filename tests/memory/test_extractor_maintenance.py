from __future__ import annotations

from datetime import UTC, datetime, timedelta
from pathlib import Path

from singularity.memory.extractor import MemoryExtractor
from singularity.memory.maintenance import MemoryMaintenance
from singularity.memory.models import (
    TTL,
    ConflictStatus,
    MemoryEntry,
    MemoryEvidenceRef,
    MemoryScope,
    MemorySource,
    MemoryStatus,
    MemoryType,
    Provenance,
)
from singularity.memory.store import MemoryStore
from singularity.review import (
    ReviewCategory,
    ReviewFinding,
    ReviewReport,
    ReviewSeverity,
    ReviewStage,
    ReviewTarget,
)


def test_extractor_handles_trace_final_report_review_verification_and_rollback() -> None:
    extractor = MemoryExtractor()
    trace_candidates = extractor.from_trace_summary(
        {
            "stable_facts": [
                {
                    "title": "Build command",
                    "summary": "python -m pytest tests --basetemp work/pytest-tmp passed",
                    "tools": ["pytest"],
                }
            ]
        }
    )
    final_candidates = extractor.from_final_report(
        {
            "user_goal": "Implement memory",
            "verification_summary": {"status": "ready", "passed_checks": ["pytest"]},
            "files_changed": ["src/singularity/memory/pipeline.py"],
        }
    )
    review_candidates = extractor.from_review_report(
        ReviewReport(
            target=ReviewTarget(stage=ReviewStage.POST_VERIFICATION),
            input_summary="review",
            findings=[
                ReviewFinding(
                    title="Missing regression test",
                    severity=ReviewSeverity.WARNING,
                    category=ReviewCategory.TEST_GAP,
                    evidence=["check_1"],
                    recommendation="Add a focused test.",
                )
            ],
        )
    )
    verification_candidates = extractor.from_verification_result(
        {
            "check_id": "check_pytest",
            "kind": "unit_test",
            "status": "failed",
            "failure_type": "unit_test_failure",
            "evidence": {
                "command": "python -m pytest tests/memory",
                "output_excerpt": "FAILED tests/memory/test_store.py::test_store",
            },
        }
    )
    rollback_candidates = extractor.from_rollback(
        {
            "rollback_id": "rollback_1",
            "error_code": "patch_conflict",
            "message": "Rollback required because patch conflict touched src/singularity/memory/store.py",
            "conflicts": ["src/singularity/memory/store.py"],
        }
    )

    assert trace_candidates[0].source == MemorySource.TRACE
    assert final_candidates[0].source == MemorySource.FINAL_REPORT
    assert review_candidates[0].type == MemoryType.CAUTION
    assert verification_candidates[0].type == MemoryType.FAILURE_LESSON
    assert rollback_candidates[0].type == MemoryType.FAILURE_LESSON


def test_maintenance_expires_demotes_conflicts_and_reloads_human_edits(tmp_path: Path) -> None:
    store = MemoryStore(tmp_path)
    store.initialize()
    old = MemoryEntry(
        id="mem_old",
        scope=MemoryScope.PROJECT,
        type=MemoryType.LESSON,
        source=MemorySource.FINAL_REPORT,
        title="Old",
        body="Old",
        provenance=Provenance(
            evidence=[
                MemoryEvidenceRef(
                    source=MemorySource.FINAL_REPORT,
                    ref_id="report",
                    summary="report",
                )
            ]
        ),
        ttl=TTL(expires_at=(datetime.now(UTC) - timedelta(days=1)).isoformat()),
    )
    first = MemoryEntry(
        id="mem_first",
        scope=MemoryScope.PROJECT,
        type=MemoryType.LESSON,
        source=MemorySource.VERIFICATION,
        title="Test command",
        body="Use pytest.",
        provenance=old.provenance,
    )
    second = MemoryEntry(
        id="mem_second",
        scope=MemoryScope.PROJECT,
        type=MemoryType.LESSON,
        source=MemorySource.VERIFICATION,
        title="Test command",
        body="Use no tests.",
        provenance=old.provenance,
    )
    store.upsert_entry(old)
    store.upsert_entry(first)
    store.upsert_entry(second)

    maintenance = MemoryMaintenance(store)
    report = maintenance.run()
    lessons_path = store.root / "human" / "lessons.md"
    lessons_path.write_text(_protected_block("mem_first", "Test command", "Use pytest edited."), encoding="utf-8")
    reload_report = maintenance.reload_human_edits()

    assert store.get_entry("mem_old").status == MemoryStatus.EXPIRED
    assert report["conflicts"] >= 1
    assert store.get_entry("mem_first").conflict_status == ConflictStatus.MANUAL_REVIEW_REQUIRED
    assert reload_report["manual_review_required"] >= 1


def test_refresh_does_not_create_candidates_from_pristine_templates(tmp_path: Path) -> None:
    store = MemoryStore(tmp_path)
    store.initialize()

    report = MemoryMaintenance(store).reload_human_edits()

    assert report["created_candidates"] == 0
    assert store.load_candidates() == []


def test_refresh_preserves_visible_human_edit_for_manual_review(tmp_path: Path) -> None:
    store = MemoryStore(tmp_path)
    store.initialize()
    entry = MemoryEntry(
        id="mem_lesson",
        scope=MemoryScope.PROJECT,
        type=MemoryType.LESSON,
        source=MemorySource.VERIFICATION,
        title="Pytest",
        body="Use pytest.",
        provenance=Provenance(
            evidence=[
                MemoryEvidenceRef(
                    source=MemorySource.VERIFICATION,
                    ref_id="check",
                    summary="check",
                )
            ]
        ),
    )
    store.upsert_entry(entry)
    lessons_path = store.root / "human" / "lessons.md"
    lessons_path.write_text(_protected_block("mem_lesson", "Pytest", "Use pytest with --basetemp."), encoding="utf-8")

    report = MemoryMaintenance(store).refresh()

    refreshed = lessons_path.read_text(encoding="utf-8")
    assert report["reload"]["manual_review_required"] == 1
    assert "Use pytest with --basetemp." in refreshed
    assert store.get_entry("mem_lesson").conflict_status == ConflictStatus.MANUAL_REVIEW_REQUIRED


def _protected_block(entry_id: str, title: str, body: str) -> str:
    return "\n".join(
        [
            "# Lessons Memory",
            "",
            f"<!-- memory:id={entry_id} schema_version=1 content_hash=test -->",
            f"## {title}",
            "Scope: project",
            "Type: lesson",
            "Source: verification",
            "Confidence: medium",
            "Status: active",
            "Conflict: none",
            "Last verified: -",
            "",
            body,
            "",
        ]
    )
