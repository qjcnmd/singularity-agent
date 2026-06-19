from __future__ import annotations

import json
import os
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from miniharness.memory.models import (
    SCHEMA_VERSION,
    MemoryCandidate,
    MemoryEntry,
    MemoryStatus,
    MemoryType,
    _now,
)


HUMAN_FILES = {
    "project": "project.md",
    "user_preferences": "user_preferences.md",
    "lessons": "lessons.md",
}


@dataclass(frozen=True)
class MemoryLayout:
    root: Path
    entries_jsonl: Path
    candidates_jsonl: Path
    index_json: Path
    human_dir: Path


class MemoryStore:
    def __init__(self, workspace_root: Path | str, *, memory_root: Path | None = None) -> None:
        self.workspace_root = Path(workspace_root).resolve(strict=False)
        self.root = memory_root or (self.workspace_root / ".miniharness" / "memory")
        self.layout = MemoryLayout(
            root=self.root,
            entries_jsonl=self.root / "entries.jsonl",
            candidates_jsonl=self.root / "candidates.jsonl",
            index_json=self.root / "index.json",
            human_dir=self.root / "human",
        )

    def initialize(self) -> None:
        self.root.mkdir(parents=True, exist_ok=True)
        self.layout.human_dir.mkdir(parents=True, exist_ok=True)
        for path in (self.layout.entries_jsonl, self.layout.candidates_jsonl):
            if not path.exists():
                _atomic_write(path, "")
        for label, filename in HUMAN_FILES.items():
            path = self.layout.human_dir / filename
            if not path.exists():
                _atomic_write(path, _initial_markdown(label))
        if not self.layout.index_json.exists():
            self.rebuild_index()

    def load_entries(self) -> list[MemoryEntry]:
        self.initialize()
        return [MemoryEntry.from_dict(item) for item in _read_jsonl(self.layout.entries_jsonl)]

    def load_candidates(self) -> list[MemoryCandidate]:
        self.initialize()
        return [MemoryCandidate.from_dict(item) for item in _read_jsonl(self.layout.candidates_jsonl)]

    def get_entry(self, entry_id: str) -> MemoryEntry:
        for entry in self.load_entries():
            if entry.id == entry_id:
                return entry
        raise KeyError(entry_id)

    def get_candidate(self, candidate_id: str) -> MemoryCandidate:
        for candidate in self.load_candidates():
            if candidate.id == candidate_id:
                return candidate
        raise KeyError(candidate_id)

    def upsert_entry(self, entry: MemoryEntry) -> MemoryEntry:
        entries = self.load_entries()
        updated = False
        for index, existing in enumerate(entries):
            if existing.id == entry.id:
                entries[index] = entry
                updated = True
                break
        if not updated:
            entries.append(entry)
        self._write_entries(entries)
        return entry

    def upsert_candidate(self, candidate: MemoryCandidate) -> MemoryCandidate:
        candidates = self.load_candidates()
        updated = False
        for index, existing in enumerate(candidates):
            if existing.id == candidate.id:
                candidates[index] = candidate
                updated = True
                break
        if not updated:
            candidates.append(candidate)
        self._write_candidates(candidates)
        return candidate

    def accept_candidate(self, candidate_id: str) -> MemoryEntry:
        candidate = self.get_candidate(candidate_id)
        accepted = candidate.with_status(MemoryStatus.ACTIVE, reason="accepted")
        entry = accepted.to_entry()
        existing = next((item for item in self.load_entries() if item.id == entry.id), None)
        if existing is not None and existing.status == MemoryStatus.TOMBSTONED:
            raise ValueError(f"Cannot restore tombstoned memory entry: {entry.id}")
        self.upsert_candidate(accepted)
        return self.upsert_entry(entry)

    def reject_candidate(self, candidate_id: str, *, reason: str = "rejected") -> MemoryCandidate:
        rejected = self.get_candidate(candidate_id).with_status(MemoryStatus.REJECTED, reason=reason)
        self.upsert_candidate(rejected)
        return rejected

    def tombstone_entry(self, entry_id: str, *, reason: str = "deleted") -> MemoryEntry:
        entry = self.get_entry(entry_id)
        payload = entry.to_dict()
        payload["status"] = MemoryStatus.TOMBSTONED.value
        payload["tombstone_reason"] = reason
        payload["updated_at"] = _now()
        tombstone = MemoryEntry.from_dict(payload)
        self.upsert_entry(tombstone)
        return tombstone

    def replace_entries(self, entries: list[MemoryEntry], *, rebuild: bool = True) -> None:
        if rebuild:
            self._write_entries(entries)
            return
        _write_jsonl(self.layout.entries_jsonl, [entry.to_dict() for entry in entries])

    def replace_candidates(self, candidates: list[MemoryCandidate], *, rebuild: bool = True) -> None:
        if rebuild:
            self._write_candidates(candidates)
            return
        _write_jsonl(self.layout.candidates_jsonl, [candidate.to_dict() for candidate in candidates])

    def rebuild_index(self) -> dict[str, Any]:
        entries = (
            [MemoryEntry.from_dict(item) for item in _read_jsonl(self.layout.entries_jsonl)]
            if self.layout.entries_jsonl.exists()
            else []
        )
        candidates = (
            [MemoryCandidate.from_dict(item) for item in _read_jsonl(self.layout.candidates_jsonl)]
            if self.layout.candidates_jsonl.exists()
            else []
        )
        active = [entry for entry in entries if entry.status == MemoryStatus.ACTIVE]
        index = {
            "schema_version": SCHEMA_VERSION,
            "updated_at": _now(),
            "counts": {
                "entries": len(entries),
                "active_entries": len(active),
                "candidates": len(candidates),
                "tombstones": len([entry for entry in entries if entry.status == MemoryStatus.TOMBSTONED]),
            },
            "by_scope": _count_by(entries, "scope"),
            "by_type": _count_by(entries, "type"),
            "by_source": _count_by(entries, "source"),
            "paths": sorted({path for entry in active for path in entry.paths}),
            "tools": sorted({tool for entry in active for tool in entry.tools}),
            "modules": sorted({module for entry in active for module in entry.modules}),
            "error_types": sorted({error for entry in active for error in entry.error_types}),
        }
        _atomic_write(self.layout.index_json, json.dumps(index, ensure_ascii=False, indent=2, sort_keys=True))
        self.write_human_projection(entries)
        return index

    def write_human_projection(self, entries: list[MemoryEntry] | None = None) -> None:
        entries = entries if entries is not None else self.load_entries()
        active_or_review = [
            entry
            for entry in entries
            if entry.status in {MemoryStatus.ACTIVE, MemoryStatus.EXPIRED}
        ]
        project_entries = [
            entry
            for entry in active_or_review
            if entry.type
            in {
                MemoryType.PROJECT_CONVENTION,
                MemoryType.BUILD_COMMAND,
                MemoryType.TEST_COMMAND,
                MemoryType.MODULE_BOUNDARY,
                MemoryType.VERIFICATION_FACT,
            }
        ]
        user_entries = [
            entry for entry in active_or_review if entry.type == MemoryType.USER_PREFERENCE
        ]
        lesson_entries = [
            entry
            for entry in active_or_review
            if entry.type in {MemoryType.LESSON, MemoryType.CAUTION, MemoryType.FAILURE_LESSON, MemoryType.TOOL_RUNTIME}
        ]
        projections = {
            "project": project_entries,
            "user_preferences": user_entries,
            "lessons": lesson_entries,
        }
        for label, items in projections.items():
            path = self.layout.human_dir / HUMAN_FILES[label]
            _atomic_write(path, _render_markdown(label, items))

    def _write_entries(self, entries: list[MemoryEntry]) -> None:
        _write_jsonl(self.layout.entries_jsonl, [entry.to_dict() for entry in entries])
        self.rebuild_index()

    def _write_candidates(self, candidates: list[MemoryCandidate]) -> None:
        _write_jsonl(self.layout.candidates_jsonl, [candidate.to_dict() for candidate in candidates])
        self.rebuild_index()


def _render_markdown(label: str, entries: list[MemoryEntry]) -> str:
    lines = [_initial_markdown(label).rstrip(), ""]
    for entry in sorted(entries, key=lambda item: (item.type.value, item.title, item.id)):
        lines.extend(
            [
                f"<!-- memory:id={entry.id} schema_version={entry.schema_version} content_hash={entry.content_hash} -->",
                f"## {entry.title}",
                f"Scope: {entry.scope.value}",
                f"Type: {entry.type.value}",
                f"Source: {entry.source.value}",
                f"Confidence: {entry.confidence.value}",
                f"Status: {entry.status.value}",
                f"Conflict: {entry.conflict_status.value}",
                f"Last verified: {entry.last_verified_at or '-'}",
                "",
                entry.body,
                "",
            ]
        )
    return "\n".join(lines).rstrip() + "\n"


def _initial_markdown(label: str) -> str:
    titles = {
        "project": "Project Memory",
        "user_preferences": "User Preference Memory",
        "lessons": "Lessons Memory",
    }
    return (
        f"# {titles.get(label, label.title())}\n\n"
        "<!-- memory:template -->\n"
        "This file is managed by MiniHarness MemoryRuntime. Human notes may be edited, "
        "but protected memory blocks are validated on refresh.\n"
        "<!-- /memory:template -->\n"
    )


def _read_jsonl(path: Path) -> list[dict[str, Any]]:
    if not path.exists():
        return []
    items: list[dict[str, Any]] = []
    for line_number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), start=1):
        if not line.strip():
            continue
        try:
            payload = json.loads(line)
        except json.JSONDecodeError as exc:
            raise ValueError(f"Invalid JSONL in {path}:{line_number}") from exc
        if not isinstance(payload, dict):
            raise ValueError(f"Invalid memory JSONL object in {path}:{line_number}")
        items.append(payload)
    return items


def _write_jsonl(path: Path, items: list[dict[str, Any]]) -> None:
    text = "".join(
        json.dumps(item, ensure_ascii=False, sort_keys=True, default=str) + "\n"
        for item in items
    )
    _atomic_write(path, text)


def _atomic_write(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(f".{path.name}.{os.getpid()}.tmp")
    with tmp.open("w", encoding="utf-8", newline="\n") as handle:
        handle.write(text)
        handle.flush()
        os.fsync(handle.fileno())
    _replace_with_retry(tmp, path)
    try:
        directory_fd = os.open(str(path.parent), os.O_RDONLY)
    except OSError:
        return
    try:
        os.fsync(directory_fd)
    except OSError:
        pass
    finally:
        os.close(directory_fd)


def _replace_with_retry(tmp: Path, path: Path) -> None:
    last_error: PermissionError | None = None
    for attempt in range(8):
        try:
            os.replace(tmp, path)
            return
        except PermissionError as exc:
            last_error = exc
            time.sleep(0.05 * (attempt + 1))
    if last_error is not None:
        raise last_error


def _count_by(entries: list[MemoryEntry], attr: str) -> dict[str, int]:
    counts: dict[str, int] = {}
    for entry in entries:
        value = getattr(entry, attr)
        key = value.value if hasattr(value, "value") else str(value)
        counts[key] = counts.get(key, 0) + 1
    return counts
