from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any

from singularity.context.models import CacheAttribution, ContextFreshness, PartialCompactionRange


@dataclass(frozen=True)
class CompactionGroup:
    group_id: str
    layer: str
    item_type: str
    source_component: str
    item_ids: list[str]
    mode: str
    utility_score: float
    token_cost: int
    volatility: float
    reference_density: float
    recency_score: float
    content_digest: str
    fragment: dict[str, Any]


@dataclass(frozen=True)
class CompactionPlan:
    source_item_ids: list[str]
    buckets: list[CompactionGroup]
    retained_item_ids: list[str]
    current_summary_item_ids: list[str]
    omitted_item_ids: list[str]
    llm_buckets: list[CompactionGroup]
    deterministic_buckets: list[CompactionGroup]
    archive_buckets: list[CompactionGroup]
    recent_tail: list[dict[str, Any]]
    previous_summary: Any | None = None
    cache_attribution: CacheAttribution = field(default_factory=CacheAttribution)
    partial_range: PartialCompactionRange | None = None

    @property
    def groups(self) -> list[CompactionGroup]:
        return self.buckets

    @property
    def llm_groups(self) -> list[CompactionGroup]:
        return self.llm_buckets

    @property
    def deterministic_groups(self) -> list[CompactionGroup]:
        return self.deterministic_buckets

    @property
    def archive_groups(self) -> list[CompactionGroup]:
        return self.archive_buckets


class ContextCompactionPlanner:
    def __init__(self, manager: Any) -> None:
        self.manager = manager

    def prepare(
        self,
        *,
        focused_item_ids: set[str] | None = None,
        partial_range: PartialCompactionRange | None = None,
    ) -> CompactionPlan:
        source_items = [
            item
            for item in self.manager.store.query_items(run_id=self.manager.run_id)
            if item.freshness == ContextFreshness.CURRENT
        ]
        if focused_item_ids is not None:
            source_items = [
                item
                for item in source_items
                if item.item_id in focused_item_ids or item.pinned
            ]
        if partial_range is not None:
            source_items = [
                item
                for item in source_items
                if self.manager._item_in_partial_range(item, partial_range) or item.pinned
            ]
        previous_summary = self.manager._previous_summary_payload()
        retained = set(self.manager._required_retained_item_ids(source_items))
        current_summary_item_ids = self.manager._current_summary_item_ids(source_items)
        retained.update(current_summary_item_ids)
        recent_tail = [] if partial_range is not None else self.manager._recent_tail_messages()
        tail_items = [] if partial_range is not None else self.manager._recent_tail_items(source_items)
        for item in tail_items:
            retained.add(item.item_id)
        buckets = self.manager._bucketize_compaction_items(source_items, retained=retained)
        omitted = [item_id for bucket in buckets for item_id in bucket.item_ids]
        return CompactionPlan(
            source_item_ids=[item.item_id for item in source_items],
            buckets=buckets,
            retained_item_ids=sorted(retained),
            current_summary_item_ids=current_summary_item_ids,
            omitted_item_ids=omitted,
            llm_buckets=[bucket for bucket in buckets if bucket.mode == "llm"],
            deterministic_buckets=[bucket for bucket in buckets if bucket.mode != "llm"],
            archive_buckets=[bucket for bucket in buckets if bucket.mode == "archive"],
            recent_tail=recent_tail,
            previous_summary=previous_summary,
            cache_attribution=self.manager._current_cache_attribution(
                source_items=source_items,
                previous_summary=previous_summary,
            ),
            partial_range=partial_range,
        )


class ContextCompactionExecutor:
    def __init__(self, manager: Any) -> None:
        self.manager = manager

    def render(self, plan: CompactionPlan) -> dict[str, Any]:
        return self.manager._render_compaction(plan)


class ContextCompactionCommitter:
    def __init__(self, manager: Any) -> None:
        self.manager = manager

    def commit(self, plan: CompactionPlan, *, context: dict[str, Any]) -> Any:
        return self.manager._commit_compaction(plan, context=context)

    def recover_after_failure(self, plan: CompactionPlan) -> None:
        self.manager._recover_after_compaction_failure(plan)
