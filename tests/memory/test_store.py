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

    assert (tmp_path / ".miniharness" / "memory" / "auto" / "entries.jsonl").exists()
    assert (tmp_path / ".miniharness" / "memory" / "auto" / "candidates.jsonl").exists()
    assert (tmp_path / ".miniharness" / "memory" / "auto" / "index.json").exists()
    assert (tmp_path / ".miniharness" / "memory" / "human" / "project.md").exists()
    assert (tmp_path / ".miniharness" / "memory" / "human" / "commands.md").exists()
    assert (tmp_path / ".miniharness" / "memory" / "human" / "preferences.md").exists()
    assert (tmp_path / ".miniharness" / "memory" / "human" / "lessons.md").exists()
    assert (tmp_path / ".miniharness" / "rules").exists()


def test_store_writes_jsonl_and_markdown_projection(tmp_path: Path) -> None:
    store = MemoryStore(tmp_path)
    store.initialize()
    entry = make_entry()

    store.upsert_entry(entry)
    loaded = store.load_entries()

    assert loaded[0].id == "mem_1"
    assert '"schema_version": 1' in store.layout.entries_jsonl.read_text(encoding="utf-8")
    lessons = (store.root / "human" / "lessons.md").read_text(encoding="utf-8")
    assert "Verified lesson" not in lessons
    assert "<!-- memory:id=mem_1" not in lessons


def test_store_migrates_legacy_root_jsonl_to_auto_layout(tmp_path: Path) -> None:
    legacy_root = tmp_path / ".miniharness" / "memory"
    legacy_root.mkdir(parents=True)
    legacy_entry = make_entry("mem_legacy")
    legacy_root.joinpath("entries.jsonl").write_text(
        __import__("json").dumps(legacy_entry.to_dict(), sort_keys=True) + "\n",
        encoding="utf-8",
    )
    legacy_root.joinpath("candidates.jsonl").write_text("", encoding="utf-8")

    store = MemoryStore(tmp_path)
    store.initialize()

    assert store.layout.entries_jsonl.parent.name == "auto"
    assert store.load_entries()[0].id == "mem_legacy"
    assert store.layout.entries_jsonl.read_text(encoding="utf-8").strip()


def test_store_migrates_legacy_user_preferences_markdown(tmp_path: Path) -> None:
    human_root = tmp_path / ".miniharness" / "memory" / "human"
    human_root.mkdir(parents=True)
    human_root.joinpath("user_preferences.md").write_text(
        "# Old Preferences\n\nPrefer concise Chinese replies.\n",
        encoding="utf-8",
    )

    store = MemoryStore(tmp_path)
    store.initialize()

    preferences = human_root / "preferences.md"
    assert "Prefer concise Chinese replies." in preferences.read_text(encoding="utf-8")


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


def test_tombstoned_entry_is_not_restored_by_same_content_candidate(tmp_path: Path) -> None:
    store = MemoryStore(tmp_path)
    store.initialize()
    candidate = MemoryCandidate.from_entry(make_entry("same_content"))
    candidate.id = "cand_original"
    store.upsert_candidate(candidate)
    accepted = store.accept_candidate("cand_original")
    store.tombstone_entry(accepted.id, reason="user deleted")
    replacement = MemoryCandidate.from_entry(make_entry("same_content"))
    replacement.id = "cand_replacement"
    store.upsert_candidate(replacement)

    try:
        store.accept_candidate("cand_replacement")
    except ValueError as exc:
        assert "tombstoned" in str(exc)
    else:
        raise AssertionError("same-content tombstoned memory was restored")
