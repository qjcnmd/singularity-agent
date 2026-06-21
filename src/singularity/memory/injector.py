from __future__ import annotations

from singularity.memory.models import Confidence, MemoryContextBlock, MemorySearchResult, MemoryType


class MemoryInjector:
    def __init__(self, *, max_items: int = 6, token_budget: int = 512) -> None:
        self.max_items = max_items
        self.token_budget = token_budget

    def build_block(self, results: list[MemorySearchResult]) -> MemoryContextBlock:
        items: list[dict] = []
        used = 0
        for result in results[: self.max_items]:
            entry = result.entry
            body = _truncate_to_budget(entry.body, max(0, self.token_budget - used - 12))
            item_tokens = _estimate_tokens(" ".join([entry.title, body]))
            if item_tokens <= 0 or used + item_tokens > self.token_budget:
                continue
            items.append(
                {
                    "id": entry.id,
                    "title": entry.title,
                    "body": body,
                    "scope": entry.scope.value,
                    "type": entry.type.value,
                    "source": entry.source.value,
                    "confidence": entry.confidence.value,
                    "last_verified_at": entry.last_verified_at,
                    "provenance": [evidence.to_dict() for evidence in entry.provenance.evidence],
                    "matched_fields": result.matched_fields,
                    "score": result.score,
                    "pollution_risk": _pollution_risk(result),
                }
            )
            used += item_tokens
        risk = "bounded"
        if any(item["pollution_risk"] == "high" for item in items):
            risk = "high"
        elif any(item["pollution_risk"] == "medium" for item in items):
            risk = "medium"
        return MemoryContextBlock(
            items=items,
            token_count=used,
            budget=self.token_budget,
            pollution_risk=risk,
        )


def _pollution_risk(result: MemorySearchResult) -> str:
    entry = result.entry
    if entry.confidence in {Confidence.HIGH, Confidence.VERIFIED} and entry.last_verified_at:
        return "low"
    if entry.type in {MemoryType.CAUTION, MemoryType.FAILURE_LESSON} or entry.confidence == Confidence.LOW:
        return "medium"
    return "medium" if not entry.last_verified_at else "low"


def _estimate_tokens(text: str) -> int:
    return max(1, len(text.split()))


def _truncate_to_budget(text: str, budget: int) -> str:
    if budget <= 0:
        return ""
    words = text.split()
    if len(words) <= budget:
        return text
    return " ".join(words[:budget])
