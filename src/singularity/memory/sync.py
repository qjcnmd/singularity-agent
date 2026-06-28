from __future__ import annotations

import json
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

from singularity.memory.models import (
    MemoryCandidate,
    MemoryEntry,
    MemorySource,
    MemoryStatus,
    Provenance,
    digest_value,
)
from singularity.memory.store import MemoryStore

BUNDLE_SCHEMA = "singularity.memory_sync_bundle/v1"


@dataclass(frozen=True)
class MemorySyncExport:
    path: Path
    entries: int
    candidates: int
    content_digest: str

    def to_dict(self) -> dict[str, object]:
        return {
            "path": str(self.path),
            "entries": self.entries,
            "candidates": self.candidates,
            "content_digest": self.content_digest,
        }


@dataclass(frozen=True)
class MemorySyncImport:
    entries: int
    candidates: int
    entries_as_candidates: int
    trusted_entries: int
    content_digest: str

    def to_dict(self) -> dict[str, object]:
        return {
            "entries": self.entries,
            "candidates": self.candidates,
            "entries_as_candidates": self.entries_as_candidates,
            "trusted_entries": self.trusted_entries,
            "content_digest": self.content_digest,
        }


class MemoryBundleSync:
    """File-backed memory export/import pipeline.

    Imported active entries become candidates by default. This keeps remote or
    shared memory reviewable instead of silently becoming local truth.
    """

    def __init__(self, store: MemoryStore) -> None:
        self.store = store

    def export_bundle(
        self,
        output_path: Path,
        *,
        include_entries: bool = True,
        include_candidates: bool = True,
    ) -> MemorySyncExport:
        entries = (
            [entry.to_dict() for entry in self.store.load_entries()]
            if include_entries
            else []
        )
        candidates = (
            [candidate.to_dict() for candidate in self.store.load_candidates()]
            if include_candidates
            else []
        )
        content = {"entries": entries, "candidates": candidates}
        digest = digest_value(content)
        payload = {
            "schema_version": BUNDLE_SCHEMA,
            "created_at": _now(),
            "workspace_root": str(self.store.workspace_root),
            "entries": entries,
            "candidates": candidates,
            "content_digest": digest,
        }
        output_path.parent.mkdir(parents=True, exist_ok=True)
        output_path.write_text(
            json.dumps(payload, ensure_ascii=False, indent=2, sort_keys=True, default=str) + "\n",
            encoding="utf-8",
        )
        return MemorySyncExport(
            path=output_path,
            entries=len(entries),
            candidates=len(candidates),
            content_digest=digest,
        )

    def import_bundle(self, bundle_path: Path, *, trust_entries: bool = False) -> MemorySyncImport:
        payload = _read_bundle(bundle_path)
        entries_payload = list(payload.get("entries") or [])
        candidates_payload = list(payload.get("candidates") or [])
        digest = digest_value({"entries": entries_payload, "candidates": candidates_payload})
        if digest != payload.get("content_digest"):
            raise ValueError("Memory sync bundle digest mismatch.")

        trusted_entries = 0
        entries_as_candidates = 0
        for item in entries_payload:
            if not isinstance(item, dict):
                raise ValueError("Memory sync entries must be objects.")
            entry = MemoryEntry.from_dict(item)
            if trust_entries:
                self.store.upsert_entry(entry)
                trusted_entries += 1
            else:
                self.store.upsert_candidate(_candidate_from_entry(entry))
                entries_as_candidates += 1

        imported_candidates = 0
        for item in candidates_payload:
            if not isinstance(item, dict):
                raise ValueError("Memory sync candidates must be objects.")
            self.store.upsert_candidate(MemoryCandidate.from_dict(item))
            imported_candidates += 1

        self.store.rebuild_index()
        return MemorySyncImport(
            entries=len(entries_payload),
            candidates=imported_candidates,
            entries_as_candidates=entries_as_candidates,
            trusted_entries=trusted_entries,
            content_digest=digest,
        )


def _candidate_from_entry(entry: MemoryEntry) -> MemoryCandidate:
    payload = MemoryCandidate.from_entry(entry).to_dict()
    payload["id"] = f"cand_remote_{digest_value(entry.to_dict())[:12]}"
    payload["status"] = MemoryStatus.CANDIDATE.value
    payload["metadata"] = {
        **dict(payload.get("metadata") or {}),
        "remote_source_entry_id": entry.id,
        "remote_source_status": entry.status.value,
    }
    provenance = Provenance.from_dict(payload.get("provenance"))
    payload["provenance"] = Provenance(
        evidence=provenance.evidence,
        created_by=provenance.created_by,
        source_run_id=provenance.source_run_id,
        source_session_id=provenance.source_session_id,
        source_task_id=provenance.source_task_id,
        extracted_at=provenance.extracted_at,
        notes=[*provenance.notes, "remote_memory_sync_import"],
    ).to_dict()
    if payload.get("source") == MemorySource.HUMAN_FILE.value:
        payload["source"] = MemorySource.MANUAL.value
    return MemoryCandidate.from_dict(payload)


def _read_bundle(path: Path) -> dict[str, Any]:
    payload = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(payload, dict):
        raise ValueError("Memory sync bundle must be a JSON object.")
    if payload.get("schema_version") != BUNDLE_SCHEMA:
        raise ValueError(f"Unsupported memory sync schema: {payload.get('schema_version')}")
    return payload


def _now() -> str:
    return datetime.now(UTC).isoformat()
