from __future__ import annotations

import re
from datetime import UTC, datetime
from typing import Iterable

from singularity.memory.models import (
    MemoryAuthorType,
    MemoryEntry,
    MemoryQuery,
    MemorySearchResult,
)


class MemoryRetriever:
    def __init__(self, entries: Iterable[MemoryEntry]) -> None:
        self.entries = list(entries)

    def search(self, query: MemoryQuery | str, *, now: datetime | None = None) -> list[MemorySearchResult]:
        if isinstance(query, str):
            query = MemoryQuery(goal=query)
        now = now or datetime.now(UTC)
        results: list[MemorySearchResult] = []
        for entry in self.entries:
            if not entry.is_active_for_retrieval(now=now):
                continue
            if query.min_confidence is not None and entry.confidence.score < query.min_confidence.score:
                continue
            score, matched_fields = _score(entry, query, now=now)
            if _requires_explicit_match(entry) and not _has_explicit_match(matched_fields):
                continue
            if score <= 0:
                continue
            results.append(MemorySearchResult(entry=entry, score=round(score, 4), matched_fields=matched_fields))
        return sorted(results, key=lambda item: item.score, reverse=True)[: query.limit]


def _score(entry: MemoryEntry, query: MemoryQuery, *, now: datetime) -> tuple[float, list[str]]:
    score = entry.confidence.score
    matched: list[str] = []
    goal_tokens = _tokens(query.goal)
    text_tokens = _tokens(" ".join([entry.title, entry.body, *entry.tags]))
    if goal_tokens:
        overlap = goal_tokens & text_tokens
        if overlap:
            score += min(1.0, len(overlap) / max(1, len(goal_tokens))) * 2.0
            matched.append("goal")
    score += _field_score(query.paths, entry.paths, "path", matched, weight=2.2)
    score += _field_score(query.tools, entry.tools, "tool", matched, weight=1.8)
    score += _field_score(query.error_types, entry.error_types, "error_type", matched, weight=1.7)
    score += _field_score(query.modules, entry.modules, "module", matched, weight=1.6)
    if entry.last_verified_at:
        score += 0.35
        try:
            verified_at = datetime.fromisoformat(entry.last_verified_at.replace("Z", "+00:00")).astimezone(UTC)
            age_days = max(0, (now - verified_at).days)
            score += max(0.0, 0.25 - (age_days / 365.0))
        except ValueError:
            pass
    if entry.provenance.evidence:
        score += min(0.4, 0.1 * len(entry.provenance.evidence))
        matched.append("provenance")
    if _is_human_context(entry):
        score += 0.45
        matched.append("human_context")
    return score, list(dict.fromkeys(matched))


def _field_score(query_values: list[str], entry_values: list[str], label: str, matched: list[str], *, weight: float) -> float:
    if not query_values or not entry_values:
        return 0.0
    normalized_entry = {_normalize(value) for value in entry_values}
    normalized_query = {_normalize(value) for value in query_values}
    direct = normalized_entry & normalized_query
    fuzzy = {
        query
        for query in normalized_query
        for candidate in normalized_entry
        if query and candidate and (query in candidate or candidate in query)
    }
    hits = direct | fuzzy
    if not hits:
        return 0.0
    matched.append(label)
    return min(weight, (len(hits) / max(1, len(normalized_query))) * weight)


def _tokens(text: str) -> set[str]:
    return {
        token.lower()
        for token in re.findall(r"[A-Za-z0-9_./\\-]+", text)
        if len(token) > 2
    }


def _normalize(value: str) -> str:
    return value.replace("\\", "/").strip().lower()


def _requires_explicit_match(entry: MemoryEntry) -> bool:
    return not _is_human_context(entry)


def _has_explicit_match(matched_fields: list[str]) -> bool:
    return any(field in {"goal", "path", "tool", "error_type", "module"} for field in matched_fields)


def _is_human_context(entry: MemoryEntry) -> bool:
    if entry.author_type == MemoryAuthorType.HUMAN:
        return True
    return entry.metadata.get("memory_kind") in {"human_file", "path_rule"}
