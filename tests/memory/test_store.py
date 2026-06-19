from __future__ import annotations

from pathlib import Path

from miniharness.memory.models import (
    Confidence,
    MemoryCandidate,
    MemoryEntry,
    MemoryEvidenceRef,
    MemoryScope,
    MemorySource,
    MemoryStatus,
    MemoryType,
    Provenance,
)
from miniharness.memory.store import MemoryStore


def make_entry(entry_id: str = "mem_1") -> MemoryEntry:
    return MemoryEntry(
        id=entry_id,
        scope=MemoryScope.PROJECT,
        type=MemoryType.LESSON,
        source=MemorySource.VERIFICATION,
        title="Verified lesson",
        body="Run pytest through python -m pytest.",
        confidence=Confidence.HIGH,
        provenance=Provenance(
            evidence=[
                MemoryEvidenceRef(
                    source=MemorySource.VERIFICATION,
                    ref_id="check_1",
                    summary="pytest passed",
                )
            ]
        ),
    )


def test_store_creates_runtime_local_memory_layout(tmp_path: Path) -> None:
    store = MemoryStore(tmp_path)
    store.initialize()

    assert (tmp_path / ".miniharness" / "memory" / "entries.jsonl").exists()
    assert (tmp_path / ".miniharness" / "memory" / "candidates.jsonl").exists()
    assert (tmp_path / ".miniharness" / "memory" / "index.json").exists()
    assert (tmp_path / ".miniharness" / "memory" / "human" / "project.md").exists()
    assert (tmp_path / ".miniharness" / "memory" / "human" / "user_preferences.md").exists()
    assert (tmp_path / ".miniharness" / "memory" / "human" / "lessons.md").exists()


def test_store_writes_jsonl_and_markdown_projection(tmp_path: Path) -> None:
    store = MemoryStore(tmp_path)
    store.initialize()
    entry = make_entry()

    store.upsert_entry(entry)
    loaded = store.load_entries()

    assert loaded[0].id == "mem_1"
    assert '"schema_version": 1' in (store.root / "entries.jsonl").read_text(encoding="utf-8")
    lessons = (store.root / "human" / "lessons.md").read_text(encoding="utf-8")
    assert "Verified lesson" in lessons
    assert "<!-- memory:id=mem_1" in lessons


def test_accept_reject_delete_and_tombstone_are_durable(tmp_path: Path) -> None:
    store = MemoryStore(tmp_path)
    store.initialize()
    candidate = MemoryCandidate.from_entry(make_entry("mem_candidate"))
    candidate.id = "cand_1"
    store.upsert_candidate(candidate)

    accepted = store.accept_candidate("cand_1")
    store.upsert_candidate(candidate.with_status(MemoryStatus.REJECTED, reason="bad"))
    tombstone = store.tombstone_entry(accepted.id, reason="user deleted")

    assert accepted.id.startswith("mem_")
    assert store.get_entry(accepted.id).status == MemoryStatus.TOMBSTONED
    assert tombstone.tombstone_reason == "user deleted"
    assert store.get_candidate("cand_1").status == MemoryStatus.REJECTED


def test_tombstoned_entry_is_not_restored_by_accepting_same_candidate(tmp_path: Path) -> None:
    store = MemoryStore(tmp_path)
    store.initialize()
    candidate = MemoryCandidate.from_entry(make_entry("same"))
    candidate.id = "cand_same"
    store.upsert_candidate(candidate)
    accepted = store.accept_candidate("cand_same")
    store.tombstone_entry(accepted.id, reason="user deleted")

    try:
        store.accept_candidate("cand_same")
    except ValueError as exc:
        assert "tombstoned" in str(exc)
    else:
        raise AssertionError("tombstoned memory was restored")
    assert store.get_entry(accepted.id).status == MemoryStatus.TOMBSTONED
