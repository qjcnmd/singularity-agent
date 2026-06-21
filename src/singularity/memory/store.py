from __future__ import annotations

import json
import os
import time
from contextlib import contextmanager
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from singularity.memory.models import (
    SCHEMA_VERSION,
    MemoryCandidate,
    MemoryEntry,
    MemoryStatus,
    _now,
)


HUMAN_FILES = {
    "project": "project.md",
    "commands": "commands.md",
    "preferences": "preferences.md",
    "lessons": "lessons.md",
}


@dataclass(frozen=True)
class MemoryLayout:
    root: Path
    auto_dir: Path
    entries_jsonl: Path
    candidates_jsonl: Path
    index_json: Path
    human_dir: Path
    rules_dir: Path


class MemoryStore:
    def __init__(self, workspace_root: Path | str, *, memory_root: Path | None = None) -> None:
        self.workspace_root = Path(workspace_root).resolve(strict=False)
        self.root = memory_root or (self.workspace_root / ".singularity" / "memory")
        self.layout = MemoryLayout(
            root=self.root,
            auto_dir=self.root / "auto",
            entries_jsonl=self.root / "auto" / "entries.jsonl",
            candidates_jsonl=self.root / "auto" / "candidates.jsonl",
            index_json=self.root / "auto" / "index.json",
            human_dir=self.root / "human",
            rules_dir=self.workspace_root / ".singularity" / "rules",
        )
        self._lock_path = self.root / "auto" / ".memory.lock"

    def initialize(self, *, rebuild_index: bool = True) -> None:
        self.root.mkdir(parents=True, exist_ok=True)
        self.layout.auto_dir.mkdir(parents=True, exist_ok=True)
        self.layout.human_dir.mkdir(parents=True, exist_ok=True)
        self.layout.rules_dir.mkdir(parents=True, exist_ok=True)
        self._migrate_legacy_auto_files()
        for path in (self.layout.entries_jsonl, self.layout.candidates_jsonl):
            if not path.exists():
                _atomic_write(path, "")
        for label, filename in HUMAN_FILES.items():
            path = self.layout.human_dir / filename
            if not path.exists():
                _atomic_write(path, _initial_markdown(label))
        legacy_preferences = self.layout.human_dir / "user_preferences.md"
        preferences = self.layout.human_dir / HUMAN_FILES["preferences"]
        if (
            legacy_preferences.exists()
            and preferences.exists()
            and _is_template_only(preferences.read_text(encoding="utf-8"))
        ):
            _atomic_write(preferences, legacy_preferences.read_text(encoding="utf-8"))
        if rebuild_index and not self.layout.index_json.exists():
            self.rebuild_index()

    def load_entries(self, *, rebuild_index: bool = True) -> list[MemoryEntry]:
        self.initialize(rebuild_index=rebuild_index)
        return [MemoryEntry.from_dict(item) for item in _read_jsonl(self.layout.entries_jsonl)]

    def load_candidates(self, *, rebuild_index: bool = True) -> list[MemoryCandidate]:
        self.initialize(rebuild_index=rebuild_index)
        return [MemoryCandidate.from_dict(item) for item in _read_jsonl(self.layout.candidates_jsonl)]

    def get_entry(self, entry_id: str, *, rebuild_index: bool = True) -> MemoryEntry:
        for entry in self.load_entries(rebuild_index=rebuild_index):
            if entry.id == entry_id:
                return entry
        raise KeyError(entry_id)

    def get_candidate(
        self,
        candidate_id: str,
        *,
        rebuild_index: bool = True,
    ) -> MemoryCandidate:
        for candidate in self.load_candidates(rebuild_index=rebuild_index):
            if candidate.id == candidate_id:
                return candidate
        raise KeyError(candidate_id)

    def upsert_entry(self, entry: MemoryEntry) -> MemoryEntry:
        with _file_lock(self._lock_path):
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
        with _file_lock(self._lock_path):
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
        with _file_lock(self._lock_path):
            candidates = self.load_candidates()
            candidate = _find_candidate(candidates, candidate_id)
            accepted = candidate.with_status(MemoryStatus.ACTIVE, reason="accepted")
            entry = accepted.to_entry()
            entries = self.load_entries()
            existing = next((item for item in entries if item.id == entry.id), None)
            if existing is not None and existing.status == MemoryStatus.TOMBSTONED:
                raise ValueError(f"Cannot restore tombstoned memory entry: {entry.id}")
            tombstoned_match = next(
                (
                    item
                    for item in entries
                    if item.status == MemoryStatus.TOMBSTONED and item.content_hash == entry.content_hash
                ),
                None,
            )
            if tombstoned_match is not None:
                raise ValueError(f"Cannot restore tombstoned memory entry: {tombstoned_match.id}")
            _upsert_candidate(candidates, accepted)
            _upsert_entry(entries, entry)
            self._write_candidates(candidates)
            self._write_entries(entries)
            return entry

    def reject_candidate(self, candidate_id: str, *, reason: str = "rejected") -> MemoryCandidate:
        with _file_lock(self._lock_path):
            candidates = self.load_candidates()
            rejected = _find_candidate(candidates, candidate_id).with_status(MemoryStatus.REJECTED, reason=reason)
            _upsert_candidate(candidates, rejected)
            self._write_candidates(candidates)
            return rejected

    def tombstone_entry(self, entry_id: str, *, reason: str = "deleted") -> MemoryEntry:
        with _file_lock(self._lock_path):
            entries = self.load_entries()
            entry = _find_entry(entries, entry_id)
            payload = entry.to_dict()
            payload["status"] = MemoryStatus.TOMBSTONED.value
            payload["tombstone_reason"] = reason
            payload["updated_at"] = _now()
            tombstone = MemoryEntry.from_dict(payload)
            _upsert_entry(entries, tombstone)
            self._write_entries(entries)
            return tombstone

    def replace_entries(self, entries: list[MemoryEntry], *, rebuild: bool = True) -> None:
        with _file_lock(self._lock_path):
            if rebuild:
                self._write_entries(entries)
                return
            _write_jsonl(self.layout.entries_jsonl, [entry.to_dict() for entry in entries])

    def replace_candidates(self, candidates: list[MemoryCandidate], *, rebuild: bool = True) -> None:
        with _file_lock(self._lock_path):
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
        return index

    def write_human_projection(self, entries: list[MemoryEntry] | None = None) -> None:
        # Human-authored files are the editable source of truth. Keep this
        # compatibility method as a no-op so refresh/index rebuilds never
        # overwrite operator-maintained Markdown.
        self.initialize()

    def _write_entries(self, entries: list[MemoryEntry]) -> None:
        _write_jsonl(self.layout.entries_jsonl, [entry.to_dict() for entry in entries])
        self.rebuild_index()

    def _write_candidates(self, candidates: list[MemoryCandidate]) -> None:
        _write_jsonl(self.layout.candidates_jsonl, [candidate.to_dict() for candidate in candidates])
        self.rebuild_index()

    def _migrate_legacy_auto_files(self) -> None:
        for filename in ("entries.jsonl", "candidates.jsonl", "index.json"):
            legacy = self.root / filename
            target = self.layout.auto_dir / filename
            if legacy.exists() and not target.exists():
                _atomic_write(target, legacy.read_text(encoding="utf-8"))


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
        "commands": "Command Memory",
        "preferences": "Preference Memory",
        "lessons": "Lessons Memory",
    }
    return (
        f"# {titles.get(label, label.title())}\n\n"
        "<!-- memory:template -->\n"
        "This file is managed by Singularity MemoryRuntime. Human notes may be edited, "
        "but protected memory blocks are validated on refresh.\n"
        "<!-- /memory:template -->\n"
    )


def _is_template_only(text: str) -> bool:
    import re

    without_template = re.sub(
        r"<!-- memory:template -->.*?<!-- /memory:template -->",
        "",
        text,
        flags=re.DOTALL,
    )
    lines = [line.strip() for line in without_template.splitlines() if line.strip()]
    return len(lines) <= 1 and (not lines or lines[0].startswith("#"))


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


def _find_entry(entries: list[MemoryEntry], entry_id: str) -> MemoryEntry:
    for entry in entries:
        if entry.id == entry_id:
            return entry
    raise KeyError(entry_id)


def _find_candidate(candidates: list[MemoryCandidate], candidate_id: str) -> MemoryCandidate:
    for candidate in candidates:
        if candidate.id == candidate_id:
            return candidate
    raise KeyError(candidate_id)


def _upsert_entry(entries: list[MemoryEntry], entry: MemoryEntry) -> None:
    for index, existing in enumerate(entries):
        if existing.id == entry.id:
            entries[index] = entry
            return
    entries.append(entry)


def _upsert_candidate(candidates: list[MemoryCandidate], candidate: MemoryCandidate) -> None:
    for index, existing in enumerate(candidates):
        if existing.id == candidate.id:
            candidates[index] = candidate
            return
    candidates.append(candidate)


@contextmanager
def _file_lock(path: Path):
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a+b") as handle:
        _lock_file(handle)
        try:
            yield
        finally:
            _unlock_file(handle)


def _lock_file(handle: Any) -> None:
    if os.name == "nt":
        import msvcrt

        handle.seek(0)
        msvcrt.locking(handle.fileno(), msvcrt.LK_LOCK, 1)
        return
    import fcntl

    fcntl.flock(handle.fileno(), fcntl.LOCK_EX)


def _unlock_file(handle: Any) -> None:
    if os.name == "nt":
        import msvcrt

        handle.seek(0)
        msvcrt.locking(handle.fileno(), msvcrt.LK_UNLCK, 1)
        return
    import fcntl

    fcntl.flock(handle.fileno(), fcntl.LOCK_UN)


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
