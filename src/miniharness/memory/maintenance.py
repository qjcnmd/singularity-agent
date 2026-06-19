from __future__ import annotations

import re
from datetime import UTC, datetime, timedelta
from typing import Any

from miniharness.memory.models import (
    Confidence,
    ConflictStatus,
    MemoryAuthorType,
    MemoryCandidate,
    MemoryEntry,
    MemoryEvidenceRef,
    MemoryScope,
    MemorySource,
    MemoryStatus,
    MemoryType,
    Provenance,
    _now,
)
from miniharness.memory.policy import contains_secret_like_content
from miniharness.memory.store import HUMAN_FILES, MemoryStore


class MemoryMaintenance:
    def __init__(self, store: MemoryStore) -> None:
        self.store = store

    def run(self) -> dict[str, int]:
        entries = self.store.load_entries()
        expired = 0
        demoted = 0
        now = datetime.now(UTC)
        rewritten: list[MemoryEntry] = []
        for entry in entries:
            payload = entry.to_dict()
            if entry.status == MemoryStatus.ACTIVE and entry.is_expired(now=now):
                payload["status"] = MemoryStatus.EXPIRED.value
                payload["updated_at"] = _now()
                expired += 1
            elif entry.status == MemoryStatus.ACTIVE and _is_stale(entry, now=now):
                payload["confidence"] = _demote_confidence(entry.confidence).value
                payload["updated_at"] = _now()
                demoted += 1
            rewritten.append(MemoryEntry.from_dict(payload))
        conflicts = self._mark_conflicts(rewritten)
        self.store.replace_entries(rewritten)
        return {"expired": expired, "demoted": demoted, "conflicts": conflicts}

    def reload_human_edits(self) -> dict[str, int]:
        entries_by_id = {entry.id: entry for entry in self.store.load_entries()}
        existing_candidates = {candidate.id: candidate for candidate in self.store.load_candidates()}
        review_required = 0
        created_candidates = 0
        for filename in HUMAN_FILES.values():
            path = self.store.layout.human_dir / filename
            if not path.exists():
                continue
            blocks, manual_sections = _parse_markdown(path.read_text(encoding="utf-8"))
            for block in blocks:
                entry = entries_by_id.get(block["id"])
                if entry is None:
                    continue
                if block["body"].strip() != entry.body.strip() or block["title"].strip() != entry.title.strip():
                    payload = entry.to_dict()
                    payload["conflict_status"] = ConflictStatus.MANUAL_REVIEW_REQUIRED.value
                    payload["body"] = block["body"].strip()
                    payload["title"] = block["title"].strip() or entry.title
                    payload["metadata"] = {
                        **entry.metadata,
                        "human_edit_detected": True,
                        "original_body": entry.body,
                        "original_title": entry.title,
                        "human_body": block["body"].strip(),
                        "human_title": block["title"].strip(),
                    }
                    payload["updated_at"] = _now()
                    entries_by_id[entry.id] = MemoryEntry.from_dict(payload)
                    review_required += 1
            for section in manual_sections:
                text = _manual_section_content(section)
                if len(text) < 20:
                    continue
                candidate = MemoryCandidate(
                    id=f"cand_human_{abs(hash(text)) & 0xFFFFFFFF:x}",
                    scope=MemoryScope.PROJECT,
                    type=_memory_type_for_human_file(filename),
                    source=MemorySource.HUMAN_FILE,
                    title=text.splitlines()[0].lstrip("# ").strip()[:100],
                    body=text,
                    confidence=Confidence.LOW,
                    author_type=MemoryAuthorType.HUMAN,
                    provenance=Provenance(
                        evidence=[
                            MemoryEvidenceRef(
                                source=MemorySource.HUMAN_FILE,
                                ref_id=filename,
                                summary="manual human memory section",
                            )
                        ]
                    ),
                )
                if candidate.id not in existing_candidates:
                    created_candidates += 1
                existing_candidates[candidate.id] = candidate
        self.store.replace_candidates(list(existing_candidates.values()), rebuild=False)
        self.store.replace_entries(list(entries_by_id.values()))
        return {
            "manual_review_required": review_required,
            "created_candidates": created_candidates,
        }

    def delete(self, entry_id: str, *, reason: str = "deleted") -> MemoryEntry:
        return self.store.tombstone_entry(entry_id, reason=reason)

    def doctor(self, *, repair: bool = False) -> dict[str, Any]:
        issues: list[dict[str, Any]] = []
        try:
            entries = self.store.load_entries()
            candidates = self.store.load_candidates()
        except Exception as exc:
            return {
                "ok": False,
                "issues": [{"code": "jsonl_unreadable", "message": str(exc)}],
                "counts": {},
            }
        for entry in entries:
            if contains_secret_like_content(entry.body) or contains_secret_like_content(entry.title):
                issues.append({"code": "secret_like_content", "entry_id": entry.id})
            if entry.status == MemoryStatus.TOMBSTONED and not entry.tombstone_reason:
                issues.append({"code": "tombstone_missing_reason", "entry_id": entry.id})
            if entry.conflict_status == ConflictStatus.MANUAL_REVIEW_REQUIRED:
                issues.append({"code": "manual_review_required", "entry_id": entry.id})
        if not self.store.layout.index_json.exists():
            issues.append({"code": "index_missing"})
        if repair:
            self.run()
            self.reload_human_edits()
            self.store.rebuild_index()
        return {
            "ok": not any(issue["code"] in {"jsonl_unreadable", "secret_like_content"} for issue in issues),
            "issues": issues,
            "counts": {
                "entries": len(entries),
                "candidates": len(candidates),
                "active_entries": len([entry for entry in entries if entry.status == MemoryStatus.ACTIVE]),
                "tombstones": len([entry for entry in entries if entry.status == MemoryStatus.TOMBSTONED]),
            },
        }

    def refresh(self) -> dict[str, Any]:
        reload_report = self.reload_human_edits()
        maintenance_report = self.run()
        index = self.store.rebuild_index()
        return {
            "reload": reload_report,
            "maintenance": maintenance_report,
            "index": index,
        }

    def _mark_conflicts(self, entries: list[MemoryEntry]) -> int:
        active = [entry for entry in entries if entry.status == MemoryStatus.ACTIVE]
        groups: dict[tuple[str, str, str], list[MemoryEntry]] = {}
        for entry in active:
            key = (entry.scope.value, entry.type.value, _normalize_title(entry.title))
            groups.setdefault(key, []).append(entry)
        conflicts = 0
        for group in groups.values():
            if len(group) < 2:
                continue
            if len({entry.body.strip() for entry in group}) <= 1:
                continue
            ids = {entry.id for entry in group}
            for index, entry in enumerate(entries):
                if entry.id not in ids:
                    continue
                payload = entry.to_dict()
                payload["conflict_status"] = ConflictStatus.MANUAL_REVIEW_REQUIRED.value
                payload["updated_at"] = _now()
                entries[index] = MemoryEntry.from_dict(payload)
                conflicts += 1
        return conflicts


def _is_stale(entry: MemoryEntry, *, now: datetime) -> bool:
    if entry.ttl.stale(now=now):
        return True
    if not entry.last_verified_at:
        return False
    try:
        verified = datetime.fromisoformat(entry.last_verified_at.replace("Z", "+00:00")).astimezone(UTC)
    except ValueError:
        return False
    return now - verified > timedelta(days=180)


def _demote_confidence(confidence: Confidence) -> Confidence:
    if confidence == Confidence.VERIFIED:
        return Confidence.HIGH
    if confidence == Confidence.HIGH:
        return Confidence.MEDIUM
    if confidence == Confidence.MEDIUM:
        return Confidence.LOW
    return Confidence.LOW


def _parse_markdown(text: str) -> tuple[list[dict[str, str]], list[str]]:
    text = re.sub(
        r"<!-- memory:template -->.*?<!-- /memory:template -->",
        "",
        text,
        flags=re.DOTALL,
    )
    pattern = re.compile(r"<!-- memory:id=(?P<id>[^ ]+)[^>]* -->")
    matches = list(pattern.finditer(text))
    blocks: list[dict[str, str]] = []
    manual_sections: list[str] = []
    cursor = 0
    for index, match in enumerate(matches):
        if match.start() > cursor:
            manual_sections.append(text[cursor:match.start()])
        end = matches[index + 1].start() if index + 1 < len(matches) else len(text)
        block_text = text[match.end():end].strip()
        title = ""
        body_lines: list[str] = []
        in_body = False
        for line in block_text.splitlines():
            if line.startswith("## ") and not title:
                title = line[3:].strip()
                continue
            if not in_body and not line.strip():
                in_body = True
                continue
            if in_body:
                body_lines.append(line)
        blocks.append({"id": match.group("id"), "title": title, "body": "\n".join(body_lines).strip()})
        cursor = end
    if cursor < len(text):
        manual_sections.append(text[cursor:])
    return blocks, manual_sections


def _normalize_title(title: str) -> str:
    return re.sub(r"\s+", " ", title.strip().lower())


def _manual_section_content(section: str) -> str:
    lines = [
        line
        for line in section.splitlines()
        if line.strip() and not line.lstrip().startswith("#")
    ]
    return "\n".join(lines).strip()


def _memory_type_for_human_file(filename: str) -> MemoryType:
    if filename == "commands.md":
        return MemoryType.TEST_COMMAND
    if filename == "preferences.md" or filename == "user_preferences.md":
        return MemoryType.USER_PREFERENCE
    if filename == "project.md":
        return MemoryType.PROJECT_CONVENTION
    return MemoryType.LESSON
