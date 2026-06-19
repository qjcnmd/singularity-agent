from __future__ import annotations

from datetime import UTC, datetime, timedelta

from miniharness.memory.models import (
    Confidence,
    ConflictStatus,
    MemoryEntry,
    MemoryEvidenceRef,
    MemoryScope,
    MemorySource,
    MemoryType,
    Provenance,
    TTL,
)


def test_memory_entry_round_trips_schema_versioned_payload() -> None:
    entry = MemoryEntry(
        id="mem_1",
        scope=MemoryScope.PROJECT,
        type=MemoryType.BUILD_COMMAND,
        source=MemorySource.VERIFICATION,
        title="Use pytest",
        body="Run python -m pytest tests --basetemp work/pytest-tmp.",
        confidence=Confidence.HIGH,
        provenance=Provenance(
            evidence=[
                MemoryEvidenceRef(
                    source=MemorySource.VERIFICATION,
                    ref_id="check_1",
                    summary="pytest passed",
                )
            ],
            created_by="test",
        ),
        paths=["tests/test_app.py"],
        tools=["pytest"],
        modules=["tests"],
        last_verified_at="2026-06-19T00:00:00+00:00",
    )

    payload = entry.to_dict()
    restored = MemoryEntry.from_dict(payload)

    assert payload["schema_version"] == 1
    assert restored == entry
    assert restored.provenance.evidence[0].source == MemorySource.VERIFICATION


def test_ttl_expiry_and_conflict_status_are_explicit() -> None:
    expired = TTL(expires_at=(datetime.now(UTC) - timedelta(days=1)).isoformat())
    active = MemoryEntry(
        id="mem_conflict",
        scope=MemoryScope.WORKSPACE,
        type=MemoryType.LESSON,
        source=MemorySource.FINAL_REPORT,
        title="Old note",
        body="Old note",
        provenance=Provenance(
            evidence=[
                MemoryEvidenceRef(
                    source=MemorySource.FINAL_REPORT,
                    ref_id="report_1",
                    summary="Final report",
                )
            ]
        ),
        ttl=expired,
        conflict_status=ConflictStatus.MANUAL_REVIEW_REQUIRED,
    )

    assert active.is_expired(now=datetime.now(UTC)) is True
    assert active.conflict_status == ConflictStatus.MANUAL_REVIEW_REQUIRED
