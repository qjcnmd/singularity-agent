from __future__ import annotations

from miniharness.memory.injector import MemoryInjector
from miniharness.memory.models import (
    Confidence,
    ConflictStatus,
    MemoryEntry,
    MemoryEvidenceRef,
    MemoryQuery,
    MemoryScope,
    MemorySource,
    MemoryStatus,
    MemoryType,
    Provenance,
)
from miniharness.memory.retrieval import MemoryRetrieval


def entry(entry_id: str, title: str, body: str, **kwargs) -> MemoryEntry:
    return MemoryEntry(
        id=entry_id,
        scope=kwargs.get("scope", MemoryScope.PROJECT),
        type=kwargs.get("type", MemoryType.LESSON),
        source=kwargs.get("source", MemorySource.VERIFICATION),
        title=title,
        body=body,
        confidence=kwargs.get("confidence", Confidence.MEDIUM),
        provenance=Provenance(
            evidence=[
                MemoryEvidenceRef(
                    source=MemorySource.VERIFICATION,
                    ref_id=f"{entry_id}_check",
                    summary="verified",
                )
            ]
        ),
        paths=kwargs.get("paths", []),
        tools=kwargs.get("tools", []),
        modules=kwargs.get("modules", []),
        error_types=kwargs.get("error_types", []),
        status=kwargs.get("status", MemoryStatus.ACTIVE),
        conflict_status=kwargs.get("conflict_status", ConflictStatus.NONE),
        last_verified_at=kwargs.get("last_verified_at", "2026-06-19T00:00:00+00:00"),
    )


def test_retrieval_ranks_by_goal_path_tool_error_and_module() -> None:
    relevant = entry(
        "mem_pytest",
        "Use pytest command",
        "Use python -m pytest tests --basetemp work/pytest-tmp for memory runtime.",
        paths=["tests/memory/test_store.py"],
        tools=["pytest"],
        modules=["memory"],
        error_types=["unit_test_failure"],
        confidence=Confidence.HIGH,
    )
    unrelated = entry("mem_docs", "Docs lesson", "Update docs after architecture changes.")

    results = MemoryRetrieval([unrelated, relevant]).search(
        MemoryQuery(
            goal="fix memory runtime unit test failure",
            paths=["tests/memory/test_store.py"],
            tools=["pytest"],
            error_types=["unit_test_failure"],
            modules=["memory"],
        )
    )

    assert [result.entry.id for result in results][:1] == ["mem_pytest"]
    assert results[0].provenance
    assert results[0].confidence == Confidence.HIGH
    assert results[0].last_verified_at == "2026-06-19T00:00:00+00:00"


def test_retrieval_filters_tombstones_expired_and_conflicted_entries() -> None:
    results = MemoryRetrieval(
        [
            entry("mem_deleted", "Deleted", "Deleted", status=MemoryStatus.TOMBSTONED),
            entry(
                "mem_conflict",
                "Conflict",
                "Conflict",
                conflict_status=ConflictStatus.MANUAL_REVIEW_REQUIRED,
            ),
            entry("mem_active", "Active pytest", "pytest"),
        ]
    ).search(MemoryQuery(goal="pytest"))

    assert [result.entry.id for result in results] == ["mem_active"]


def test_injector_budgets_items_and_marks_pollution_risk() -> None:
    results = MemoryRetrieval(
        [
            entry("mem_high", "High confidence", "Verified pytest command.", confidence=Confidence.HIGH),
            entry(
                "mem_low",
                "Low confidence caution",
                "Caution from review.",
                confidence=Confidence.LOW,
                type=MemoryType.CAUTION,
            ),
        ]
    ).search(MemoryQuery(goal="pytest review caution"))

    block = MemoryInjector(max_items=2, token_budget=40).build_block(results)

    assert block.runtime == "memory"
    assert len(block.items) == 2
    assert block.token_count <= 40
    assert {item["pollution_risk"] for item in block.items} >= {"low", "medium"}
    assert all("source" in item and "confidence" in item for item in block.items)
