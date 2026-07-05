from __future__ import annotations

import json
from dataclasses import dataclass, field
from typing import Any, cast
from uuid import uuid4

from singularity.context.compression import ContextCompressor, ContextSummaryValidationError
from singularity.context.models import (
    CacheAttribution,
    ContextAuthority,
    ContextFreshness,
    ContextItem,
    ContextItemType,
    ContextLayer,
    ContextSnapshot,
    ContextSource,
    ContextSummaryEnvelope,
    ContextSummaryPayload,
    PartialCompactionRange,
    digest_value,
)
from singularity.context.ranking import (
    CONTEXT_COMPACTION_PINNED_SCORE_BONUS,
    CONTEXT_COMPACTION_STALE_FRESHNESS_SCORE_PENALTY,
    CONTEXT_COMPACTION_WEIGHT_SCALE,
    CONTEXT_RECENCY_CURRENT,
    CONTEXT_RECENCY_DEFAULT,
    CONTEXT_RECENCY_STALE,
    CONTEXT_REFERENCE_DENSITY_PRECISION,
    CONTEXT_VOLATILITY_DEFAULT,
    CONTEXT_VOLATILITY_EVIDENCE,
    CONTEXT_VOLATILITY_MESSAGE_OR_FAILURE,
    CONTEXT_VOLATILITY_RECENT_OR_FAILURE,
    authority_weight,
    layer_weight,
)

COMPACTION_RECENT_TAIL_MESSAGES = 8
COMPACTION_FRAGMENT_LIMIT = 8000


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
                if self.item_in_partial_range(item, partial_range) or item.pinned
            ]
        previous_summary = self.previous_summary_payload()
        retained = set(self.required_retained_item_ids(source_items))
        current_summary_item_ids = self.current_summary_item_ids(source_items)
        retained.update(current_summary_item_ids)
        recent_tail = [] if partial_range is not None else self.recent_tail_messages()
        tail_items = [] if partial_range is not None else self.recent_tail_items(source_items)
        for item in tail_items:
            retained.add(item.item_id)
        buckets = self.bucketize_compaction_items(source_items, retained=retained)
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
            cache_attribution=self.manager.usage_reporter.current_cache_attribution(
                last_bundle=self.manager.last_bundle,
                source_items=source_items,
                previous_summary=previous_summary,
            ),
            partial_range=partial_range,
        )

    def bucketize_compaction_items(
        self,
        source_items: list[ContextItem],
        *,
        retained: set[str],
    ) -> list[CompactionGroup]:
        buckets: list[CompactionGroup] = []
        for item in source_items:
            if item.item_id in retained:
                continue
            if item.item_type in {
                ContextItemType.SYSTEM_INSTRUCTION,
                ContextItemType.USER_GOAL,
            }:
                retained.add(item.item_id)
                continue
            mode = self.compaction_mode(item)
            fragment = self.compaction_fragment(item)
            buckets.append(
                CompactionGroup(
                    group_id=self.bucket_id(item),
                    layer=item.layer.value,
                    item_type=item.item_type.value,
                    source_component=item.source_component.value,
                    item_ids=[item.item_id],
                    mode=mode,
                    utility_score=self.utility_score(item),
                    token_cost=int(item.token_count or 0),
                    volatility=self.volatility_score(item),
                    reference_density=self.reference_density(item),
                    recency_score=self.recency_score(item),
                    content_digest=item.content_digest,
                    fragment=fragment,
                )
            )
        return sorted(buckets, key=self.bucket_sort_key)

    @staticmethod
    def bucket_id(item: ContextItem) -> str:
        return f"{item.layer.value}:{item.item_type.value}:{item.source_component.value}:{item.item_id}"

    @staticmethod
    def bucket_sort_key(bucket: CompactionGroup) -> tuple[Any, ...]:
        return (
            -bucket.utility_score,
            bucket.token_cost,
            -bucket.volatility,
            -bucket.reference_density,
            -bucket.recency_score,
            bucket.content_digest,
            bucket.item_ids[0],
        )

    def utility_score(self, item: ContextItem) -> float:
        score = float(item.importance)
        score += layer_weight(item.layer) / CONTEXT_COMPACTION_WEIGHT_SCALE
        score += authority_weight(item.authority) / CONTEXT_COMPACTION_WEIGHT_SCALE
        if item.relevance_score is not None:
            score += float(item.relevance_score)
        if item.pinned:
            score += CONTEXT_COMPACTION_PINNED_SCORE_BONUS
        if item.freshness == ContextFreshness.STALE:
            score += CONTEXT_COMPACTION_STALE_FRESHNESS_SCORE_PENALTY
        return score

    @staticmethod
    def volatility_score(item: ContextItem) -> float:
        if item.layer in {ContextLayer.RECENT_DIALOGUE, ContextLayer.FAILURE_MEMORY}:
            return CONTEXT_VOLATILITY_RECENT_OR_FAILURE
        if item.item_type in {
            ContextItemType.ASSISTANT_MESSAGE,
            ContextItemType.USER_MESSAGE,
            ContextItemType.FAILURE,
        }:
            return CONTEXT_VOLATILITY_MESSAGE_OR_FAILURE
        if item.item_type in {
            ContextItemType.VERIFICATION_EVIDENCE,
            ContextItemType.MUTATION_EVIDENCE,
            ContextItemType.EDIT_EVIDENCE,
            ContextItemType.COMMAND_OBSERVATION,
        }:
            return CONTEXT_VOLATILITY_EVIDENCE
        return CONTEXT_VOLATILITY_DEFAULT

    @staticmethod
    def reference_density(item: ContextItem) -> float:
        refs = max(1, len(item.references))
        tokens = max(1, int(item.token_count or 1))
        return round(refs / tokens, CONTEXT_REFERENCE_DENSITY_PRECISION)

    @staticmethod
    def recency_score(item: ContextItem) -> float:
        if item.freshness == ContextFreshness.CURRENT:
            return CONTEXT_RECENCY_CURRENT
        if item.freshness == ContextFreshness.STALE:
            return CONTEXT_RECENCY_STALE
        return CONTEXT_RECENCY_DEFAULT

    @staticmethod
    def current_summary_item_ids(items: list[ContextItem]) -> list[str]:
        return [
            item.item_id
            for item in items
            if item.layer == ContextLayer.COMPRESSED_HISTORY
            and item.item_type == ContextItemType.SUMMARY
            and item.freshness == ContextFreshness.CURRENT
        ]

    @staticmethod
    def active_summary_item_id(items: list[ContextItem]) -> str | None:
        summary_ids = ContextCompactionPlanner.current_summary_item_ids(items)
        return summary_ids[-1] if summary_ids else None

    def required_retained_item_ids(self, items: list[ContextItem]) -> list[str]:
        retained: set[str] = set()
        for item in items:
            if item.pinned or item.layer in {ContextLayer.SYSTEM, ContextLayer.USER_GOAL}:
                retained.add(item.item_id)
        for layer, item_type in (
            (ContextLayer.POLICY_STATE, ContextItemType.POLICY_OBSERVATION),
            (ContextLayer.PLANNER_STATE, ContextItemType.PLANNER_STATE),
        ):
            latest = self.latest_item(items, layer=layer, item_type=item_type)
            if latest is not None:
                retained.add(latest.item_id)
        return list(retained)

    @staticmethod
    def latest_item(
        items: list[ContextItem],
        *,
        layer: ContextLayer,
        item_type: ContextItemType,
    ) -> ContextItem | None:
        for item in reversed(items):
            if item.layer == layer and item.item_type == item_type:
                return item
        return None

    def recent_tail_items(self, items: list[ContextItem]) -> list[ContextItem]:
        max_tail_tokens = max(32, int(self.manager.assembler.model_context_window * 0.1))
        candidates = [
            item
            for item in items
            if item.layer in {ContextLayer.RECENT_DIALOGUE, ContextLayer.TOOL_OBSERVATIONS}
            and int(item.token_count or 0) <= max_tail_tokens
        ]
        return candidates[-COMPACTION_RECENT_TAIL_MESSAGES:]

    def recent_tail_messages(
        self,
        *,
        partial_range: PartialCompactionRange | None = None,
    ) -> list[dict[str, Any]]:
        max_tail_tokens = max(32, int(self.manager.assembler.model_context_window * 0.1))
        history = [safe_message(message) for message in self.manager._messages[2:]]
        bounded = [
            message
            for message in history
            if self.manager.token_counter.count_message(message) <= max_tail_tokens
            and (partial_range is None or not message_in_partial_range(message, partial_range))
        ]
        return bounded[-COMPACTION_RECENT_TAIL_MESSAGES:]

    @staticmethod
    def item_in_partial_range(
        item: ContextItem,
        compaction_range: PartialCompactionRange,
    ) -> bool:
        turn = metadata_turn(item.metadata)
        if turn is not None:
            if compaction_range.start_turn is not None and turn < compaction_range.start_turn:
                return False
            return compaction_range.end_turn is None or turn <= compaction_range.end_turn
        checkpoint_id = str(item.metadata.get("checkpoint_id") or "")
        return bool(compaction_range.checkpoint_id and checkpoint_id == compaction_range.checkpoint_id)

    @staticmethod
    def compaction_mode(item: ContextItem) -> str:
        if item.layer in {ContextLayer.RECENT_DIALOGUE, ContextLayer.FAILURE_MEMORY}:
            return "llm"
        if item.item_type in {ContextItemType.FAILURE, ContextItemType.ASSISTANT_MESSAGE, ContextItemType.USER_MESSAGE}:
            return "llm"
        return "deterministic"

    def compaction_fragment(self, item: ContextItem) -> dict[str, Any]:
        payload = item.content if isinstance(item.content, dict) else {"content": item.content}
        refs = [ref.ref_id for ref in item.references] or [item.item_id]
        base = {
            "item_id": item.item_id,
            "layer": item.layer.value,
            "item_type": item.item_type.value,
            "source_component": item.source_component.value,
            "reference_ids": refs,
            "raw_digest": item.content_digest,
        }
        if item.item_type == ContextItemType.TOOL_OBSERVATION:
            content = payload.get("content")
            tool_payload = json_object(content) if isinstance(content, str) else payload
            metadata_value = payload.get("metadata")
            metadata = metadata_value if isinstance(metadata_value, dict) else {}
            return {
                **base,
                "tool_name": payload.get("name") or payload.get("tool_name") or tool_payload.get("tool_name"),
                "tool_call_id": payload.get("tool_call_id") or tool_payload.get("tool_call_id"),
                "ok": tool_payload.get("ok") if "ok" in tool_payload else payload.get("ok"),
                "preview": bounded_string(
                    tool_payload.get("preview")
                    or tool_payload.get("content")
                    or payload.get("preview")
                    or "",
                ),
                "artifact_refs": tool_payload.get("artifact_refs") or metadata.get("artifact_refs") or [],
                "logs_ref": tool_payload.get("logs_ref") or metadata.get("logs_ref"),
            }
        if isinstance(item.content, dict) and item.content.get("role"):
            safe = safe_message(item.content)
            if isinstance(safe.get("content"), str):
                safe["content"] = bounded_string(safe["content"])
            return {**base, "message": safe}
        if item.item_type == ContextItemType.VERIFICATION_EVIDENCE:
            return {
                **base,
                "status": payload.get("status"),
                "failure_summary": payload.get("failure_summary"),
                "repair_hints": list(payload.get("repair_hints") or [])[:10],
                "logs_ref": payload.get("logs_ref"),
            }
        if item.item_type in {
            ContextItemType.MUTATION_EVIDENCE,
            ContextItemType.EDIT_EVIDENCE,
            ContextItemType.WORKSPACE_STATE,
        }:
            return {
                **base,
                "changed_files": list(
                    payload.get("changed_files")
                    or payload.get("files_changed")
                    or payload.get("files")
                    or []
                )[:50],
                "patch_digest": payload.get("patch_digest") or payload.get("diff_digest"),
                "rollback_ref": payload.get("rollback_ref") or payload.get("transaction_id"),
                "verification_state": payload.get("verification_state")
                or payload.get("verification_status")
                or payload.get("status"),
            }
        return {
            **base,
            "preview": bounded_string(
                json.dumps(item.content, ensure_ascii=False, sort_keys=True, default=str)
            ),
        }

    def previous_summary_payload(self) -> ContextSummaryPayload | None:
        if self.manager._summary_payload is not None:
            return self.manager._summary_payload
        snapshot = self.manager.store.latest_snapshot(self.manager.run_id)
        if snapshot is None:
            return None
        envelope_payload = snapshot.metadata.get("summary_envelope")
        if isinstance(envelope_payload, dict):
            envelope = ContextSummaryEnvelope.from_dict(envelope_payload)
            if envelope.summary_payload is not None:
                self.manager._summary_envelope = envelope
                self.manager._summary_payload = envelope.summary_payload
                self.manager._summary = envelope.rendered_summary or self.manager._summary
                return envelope.summary_payload
        payload = snapshot.metadata.get("summary_payload")
        if not isinstance(payload, dict):
            return None
        try:
            summary = ContextSummaryPayload.from_dict(payload)
            self.manager._summary_payload = summary
            self.manager._summary = snapshot.summary or self.manager._summary
            return summary
        except Exception:
            return None


class ContextCompactionExecutor:
    def __init__(self, manager: Any) -> None:
        self.manager = manager

    def render(self, plan: CompactionPlan) -> dict[str, Any]:
        llm_payload: dict[str, Any] | None = None
        if plan.llm_buckets:
            llm_payload = self.run_llm_compaction(plan)
        source_ids = set(plan.source_item_ids)
        source_items = [
            item
            for item in self.manager.store.query_items(run_id=self.manager.run_id)
            if item.item_id in source_ids
        ]
        summary_payload = self.summary_payload_from_plan(
            plan,
            llm_payload=llm_payload,
            source_items=source_items,
        )
        validated = self.manager.compressor.parse_summary(
            json.dumps(summary_payload, ensure_ascii=False, sort_keys=True, default=str),
            source_items=source_items,
            previous_summary=plan.previous_summary,
        )
        envelope = self.summary_envelope_for_plan(
            plan,
            summary=validated,
            rendered_summary=self.render_summary_for_context(validated),
        )
        return {
            "summary": validated,
            "envelope": envelope,
            "summary_text": envelope.rendered_summary,
        }

    def run_llm_compaction(self, plan: CompactionPlan) -> dict[str, Any]:
        compression_messages: list[dict[str, Any]] = [
            {
                "role": "system",
                "content": (
                    "Summarize only the provided old dialogue, failed attempts, "
                    "open questions, and user-intent evolution as compact JSON. "
                    "Return keys: goal, current_state, completed_actions, pending_actions, "
                    "verified_facts, failed_attempts, policy_constraints, workspace_changes, "
                    "verification_status, open_questions, reference_ids, omitted_item_ids, confidence. "
                    "Every verified_facts entry must include reference_ids. Do not invent facts."
                ),
            },
            {
                "role": "user",
                "content": json.dumps(
                    {
                        "goal": self.manager.user_goal,
                        "previous_summary": (
                            plan.previous_summary.__dict__
                            if plan.previous_summary is not None
                            else None
                        ),
                        "messages": [
                            group.fragment["message"]
                            for group in plan.llm_groups
                            if isinstance(group.fragment.get("message"), dict)
                        ],
                        "items": [group.fragment for group in plan.llm_groups],
                    },
                    ensure_ascii=False,
                    sort_keys=True,
                    default=str,
                ),
            },
        ]
        if self.manager.model_runner is not None:
            from singularity.model import (
                ModelBudget,
                ModelMessage,
                ModelPurpose,
                ModelTurnRequest,
                ToolChoicePolicy,
            )
            from singularity.model import (
                ToolChoiceMode as ContextToolChoiceMode,
            )

            request_id = f"model_compact_{uuid4().hex[:12]}"
            result = self.manager.model_runner.run_turn(
                ModelTurnRequest(
                    request_id=request_id,
                    run_id=self.manager.run_id,
                    session_id=self.manager.session_id,
                    task_id=self.manager.task_id,
                    phase_id="context_compaction",
                    action_id=request_id,
                    purpose=ModelPurpose.COMPACT_CONTEXT,
                    messages=[ModelMessage.from_dict(message) for message in compression_messages],
                    tools=[],
                    tool_choice=ToolChoicePolicy(mode=ContextToolChoiceMode.NONE),
                    budget=ModelBudget(),
                    context_metadata={"compaction_plan": plan_metadata(plan)},
                    trace_metadata={"compaction_plan": plan_metadata(plan)},
                )
            )
            content = result.assistant_message.text if result.assistant_message else ""
        else:
            from singularity.model.models import ToolChoiceMode

            provider = self.manager.provider
            if provider is None:
                return {}
            response = provider.chat(
                messages=compression_messages,
                tools=[],
                tool_choice=ToolChoiceMode.NONE,
            )
            content = (
                ((response.get("choices") or [{}])[0].get("message") or {}).get("content")
                or ""
            )
        normalized = self.normalize_summary_payload(
            content,
            source_item_ids=plan.omitted_item_ids,
        )
        return json.loads(normalized)

    def summary_payload_from_plan(
        self,
        plan: CompactionPlan,
        *,
        llm_payload: dict[str, Any] | None,
        source_items: list[ContextItem],
    ) -> dict[str, Any]:
        payload = dict(llm_payload or {})
        deterministic = self.deterministic_summary(plan, source_items=source_items)
        previous = plan.previous_summary.__dict__ if plan.previous_summary is not None else {}
        reference_ids = unique_strings(
            list(previous.get("reference_ids") or [])
            + list(payload.get("reference_ids") or [])
            + deterministic["reference_ids"]
        )
        return {
            "goal": self.manager.user_goal,
            "current_state": str(
                payload.get("current_state")
                or payload.get("summary")
                or deterministic["current_state"]
            ),
            "completed_actions": unique_values(
                list(previous.get("completed_actions") or [])
                + list(payload.get("completed_actions") or [])
            ),
            "pending_actions": unique_values(list(payload.get("pending_actions") or [])),
            "verified_facts": unique_values(
                list(previous.get("verified_facts") or [])
                + list(payload.get("verified_facts") or [])
                + deterministic["verified_facts"]
            ),
            "failed_attempts": unique_values(
                list(previous.get("failed_attempts") or [])
                + list(payload.get("failed_attempts") or [])
                + deterministic["failed_attempts"]
            ),
            "policy_constraints": unique_strings(
                list(previous.get("policy_constraints") or [])
                + list(payload.get("policy_constraints") or [])
                + deterministic["policy_constraints"]
            ),
            "workspace_changes": unique_values(
                list(previous.get("workspace_changes") or [])
                + list(payload.get("workspace_changes") or [])
                + deterministic["workspace_changes"]
            ),
            "verification_status": deterministic["verification_status"]
            or str(payload.get("verification_status") or previous.get("verification_status") or "unknown"),
            "open_questions": unique_values(
                list(payload.get("open_questions") or [])
                + list(previous.get("open_questions") or [])
            ),
            "reference_ids": reference_ids,
            "omitted_item_ids": list(plan.omitted_item_ids),
            "confidence": float(payload.get("confidence") or 0.65),
        }

    @staticmethod
    def deterministic_summary(
        plan: CompactionPlan,
        *,
        source_items: list[ContextItem],
    ) -> dict[str, Any]:
        reference_ids: list[str] = []
        verified_facts: list[dict[str, Any]] = []
        failed_attempts: list[dict[str, Any]] = []
        policy_constraints: list[str] = []
        workspace_changes: list[dict[str, Any]] = []
        latest_verification_status = ""
        for item in source_items:
            if item.item_type == ContextItemType.POLICY_OBSERVATION and isinstance(item.content, dict):
                policy_constraints.extend(str(value) for value in item.content.get("constraints_summary") or [])
                if item.content.get("outcome") not in {None, "allow", "allowed"}:
                    policy_constraints.append(str(item.content.get("reason") or item.content.get("outcome")))
        for group in plan.deterministic_groups:
            fragment = group.fragment
            refs = [str(ref) for ref in fragment.get("reference_ids") or group.item_ids]
            reference_ids.extend(refs)
            if group.item_type == ContextItemType.TOOL_OBSERVATION.value:
                fact = {
                    "fact": f"tool {fragment.get('tool_name') or '<unknown>'} returned ok={bool(fragment.get('ok'))}",
                    "reference_ids": refs,
                }
                verified_facts.append(fact)
                if not fragment.get("ok"):
                    failed_attempts.append(
                        {
                            "source": "tool_observation",
                            "tool_name": fragment.get("tool_name"),
                            "error_code": fragment.get("error_code"),
                            "reference_ids": refs,
                        }
                    )
            elif group.item_type == ContextItemType.VERIFICATION_EVIDENCE.value:
                latest_verification_status = str(fragment.get("status") or "unknown")
                verified_facts.append(
                    {
                        "fact": f"verification status {latest_verification_status}",
                        "reference_ids": refs,
                    }
                )
                if latest_verification_status not in {"passed", "succeeded"}:
                    failed_attempts.append(
                        {
                            "source": "verification",
                            "status": latest_verification_status,
                            "failure_summary": fragment.get("failure_summary"),
                            "repair_hints": fragment.get("repair_hints") or [],
                            "logs_ref": fragment.get("logs_ref"),
                            "reference_ids": refs,
                        }
                    )
            elif group.item_type in {
                ContextItemType.MUTATION_EVIDENCE.value,
                ContextItemType.EDIT_EVIDENCE.value,
                ContextItemType.WORKSPACE_STATE.value,
            }:
                workspace_changes.append(
                    {
                        "changed_files": fragment.get("changed_files") or [],
                        "patch_digest": fragment.get("patch_digest"),
                        "rollback_ref": fragment.get("rollback_ref"),
                        "verification_state": fragment.get("verification_state"),
                        "reference_ids": refs,
                    }
                )
        return {
            "current_state": (
                f"Compacted {len(plan.omitted_item_ids)} context items into "
                f"{len(plan.deterministic_groups)} deterministic groups and "
                f"{len(plan.llm_groups)} dialogue groups."
            ),
            "reference_ids": unique_strings(reference_ids),
            "verified_facts": verified_facts,
            "failed_attempts": failed_attempts,
            "policy_constraints": unique_strings(policy_constraints),
            "workspace_changes": workspace_changes,
            "verification_status": latest_verification_status,
        }

    def summary_envelope_for_plan(
        self,
        plan: CompactionPlan,
        *,
        summary: ContextSummaryPayload,
        rendered_summary: str,
    ) -> ContextSummaryEnvelope:
        previous_digest = digest_value(plan.previous_summary) if plan.previous_summary is not None else None
        summary_digest = digest_value(summary)
        summary_id = self.next_summary_item_id(summary_digest=summary_digest)
        return ContextSummaryEnvelope(
            version=ContextCompressor.SUMMARY_VERSION,
            summary_id=summary_id,
            summary_payload=summary,
            source_item_ids=list(plan.source_item_ids),
            cache_attribution=plan.cache_attribution,
            previous_summary_digest=previous_digest,
            summary_digest=summary_digest,
            rendered_summary=rendered_summary,
            metadata={
                "compaction_generation": self.manager._compaction_generation + 1,
                "compaction_plan": plan_metadata(plan),
            },
        )

    def next_summary_item_id(self, *, summary_digest: str) -> str:
        generation = self.manager._compaction_generation + 1
        suffix = summary_digest[:12] if summary_digest else "pending"
        return f"summary_{self.manager.run_id}_{generation:04d}_{suffix}"

    @staticmethod
    def render_summary_for_context(summary: ContextSummaryPayload) -> str:
        lines = [summary.current_state]
        if summary.verification_status not in {"", "unknown", "not_run"}:
            lines.append(f"verification={summary.verification_status}")
        if summary.policy_constraints:
            lines.append("policy=" + "; ".join(summary.policy_constraints[:4]))
        if summary.workspace_changes:
            lines.append(
                "workspace="
                + json.dumps(summary.workspace_changes[:3], ensure_ascii=False, default=str)
            )
        if summary.failed_attempts:
            lines.append(
                "failed="
                + json.dumps(summary.failed_attempts[:3], ensure_ascii=False, default=str)
            )
        if summary.open_questions:
            lines.append(
                "open="
                + json.dumps(summary.open_questions[:3], ensure_ascii=False, default=str)
            )
        if summary.reference_ids:
            lines.append("refs=" + ",".join(summary.reference_ids[:12]))
        return " | ".join(lines)

    @staticmethod
    def normalize_summary_payload(content: str, *, source_item_ids: list[str]) -> str:
        try:
            parsed = json.loads(content)
        except json.JSONDecodeError as exc:
            raise ContextSummaryValidationError("context_summary_invalid_json") from exc
        if not isinstance(parsed, dict):
            raise ValueError("context_summary_not_object")
        if "current_state" in parsed:
            return json.dumps(parsed, ensure_ascii=False, sort_keys=True, default=str)
        reference_ids = parsed.get("reference_ids") or []
        verified_facts = parsed.get("verified_facts") or []
        normalized_verified: list[Any] = []
        for fact in verified_facts:
            if isinstance(fact, dict):
                normalized_verified.append(fact)
            elif reference_ids:
                normalized_verified.append(
                    {"fact": str(fact), "reference_ids": list(reference_ids)}
                )
            else:
                normalized_verified.append(fact)
        normalized = {
            "goal": str(parsed.get("goal") or ""),
            "current_state": str(parsed.get("summary") or parsed.get("current_state") or ""),
            "completed_actions": list(parsed.get("completed_actions") or []),
            "pending_actions": list(parsed.get("pending_actions") or []),
            "verified_facts": normalized_verified,
            "failed_attempts": list(parsed.get("failed_attempts") or []),
            "policy_constraints": list(parsed.get("policy_constraints") or parsed.get("constraints") or []),
            "workspace_changes": list(parsed.get("workspace_changes") or []),
            "verification_status": str(parsed.get("verification_status") or "unknown"),
            "open_questions": list(parsed.get("open_questions") or []),
            "reference_ids": [str(item) for item in reference_ids],
            "omitted_item_ids": list(parsed.get("omitted_item_ids") or source_item_ids),
            "confidence": float(parsed.get("confidence") or 0.5),
        }
        return json.dumps(normalized, ensure_ascii=False, sort_keys=True, default=str)


class ContextCompactionCommitter:
    def __init__(self, manager: Any) -> None:
        self.manager = manager

    def commit(self, plan: CompactionPlan, *, context: dict[str, Any]) -> ContextSnapshot:
        validated = context["summary"]
        envelope = context["envelope"]
        summary_text = context["summary_text"]
        known_ids = validated.reference_ids
        self.manager._summary = summary_text
        self.manager._summary_payload = validated
        self.manager._summary_envelope = envelope
        self.manager._compaction_generation += 1
        summary_item = self.manager.add_context_item(
            self.manager._make_item(
                item_id=envelope.summary_id or self.summary_item_id(),
                layer=ContextLayer.COMPRESSED_HISTORY,
                source_component=ContextSource.SUMMARY,
                item_type=ContextItemType.SUMMARY,
                content=summary_text,
                authority=ContextAuthority.SUMMARY,
                importance=0.8,
                pinned=True,
                metadata={
                    "summary_payload": validated.to_dict(),
                    "summary_envelope": envelope.to_dict(),
                    "summary_digest": envelope.summary_digest,
                    "compaction_generation": self.manager._compaction_generation,
                    "compaction_plan": plan_metadata(plan),
                },
            )
        )
        self.retire_previous_summary_items(
            plan.current_summary_item_ids,
            superseded_by=summary_item.item_id,
        )
        omitted_item_ids = [
            item_id
            for item_id in validated.omitted_item_ids
            if item_id not in set(plan.retained_item_ids)
            and item_id != summary_item.item_id
        ]
        self.manager.store.compact_items(
            run_id=self.manager.run_id,
            omitted_item_ids=omitted_item_ids,
            summary_item_id=summary_item.item_id,
        )
        retained_messages = self.compacted_messages(
            validated,
            recent_tail=plan.recent_tail,
            summary_text=summary_text,
            partial_range=plan.partial_range,
        )
        snapshot = ContextSnapshot(
            snapshot_id=uuid4().hex,
            run_id=self.manager.run_id,
            session_id=self.manager.session_id,
            task_id=self.manager.task_id,
            goal=self.manager.user_goal,
            summary=summary_text,
            retained_item_ids=[*plan.retained_item_ids, summary_item.item_id],
            retained_messages=retained_messages,
            known_observation_ids=known_ids,
            version=self.manager.store.current_version(self.manager.run_id),
            created_at=self.manager._now(),
            metadata={
                "summary_payload": validated.to_dict(),
                "summary_envelope": envelope.to_dict(),
                "summary_digest": envelope.summary_digest,
                "omitted_item_ids": omitted_item_ids,
                "compaction_generation": self.manager._compaction_generation,
                "compaction_plan": plan_metadata(plan),
                "cache_attribution": envelope.cache_attribution.to_dict(),
            },
        )
        self.manager.store.save_summary(
            run_id=self.manager.run_id,
            summary_id=summary_item.item_id,
            payload=envelope.to_dict(),
            source_item_ids=plan.source_item_ids,
        )
        self.manager.store.save_snapshot(snapshot)
        self.manager._messages = retained_messages
        return snapshot

    def apply_compacted_messages(
        self,
        summary_payload: ContextSummaryPayload | None,
        *,
        recent_tail: list[dict[str, Any]],
        summary_text: str,
    ) -> None:
        self.manager._messages = self.compacted_messages(
            summary_payload,
            recent_tail=recent_tail,
            summary_text=summary_text,
        )

    def recover_after_failure(self, plan: CompactionPlan | None) -> dict[str, Any]:
        recovery_errors: list[dict[str, str]] = []
        try:
            snapshot = self.manager.store.latest_snapshot(self.manager.run_id)
        except Exception as exc:
            snapshot = None
            recovery_errors.append(
                {
                    "stage": "latest_snapshot",
                    "error_type": type(exc).__name__,
                    "message": str(exc),
                }
            )
        if snapshot is not None and snapshot.retained_messages:
            self.manager._summary = snapshot.summary
            self.manager._messages = list(snapshot.retained_messages)
            envelope_payload = snapshot.metadata.get("summary_envelope")
            if isinstance(envelope_payload, dict):
                try:
                    envelope = ContextSummaryEnvelope.from_dict(envelope_payload)
                except Exception as exc:
                    recovery_errors.append(
                        {
                            "stage": "summary_envelope",
                            "error_type": type(exc).__name__,
                            "message": str(exc),
                        }
                    )
                else:
                    self.manager._summary_envelope = envelope
                    self.manager._summary_payload = envelope.summary_payload
            return {
                "mode": "latest_snapshot",
                "snapshot_id": snapshot.snapshot_id,
                "message_count": len(snapshot.retained_messages),
                "errors": recovery_errors,
            }
        recent_tail = (
            plan.recent_tail
            if plan is not None
            else self.manager._messages[2:][-COMPACTION_RECENT_TAIL_MESSAGES:]
        )
        self.manager._messages = [
            *safe_base_messages(self.manager._messages, self.manager.user_goal),
            *[safe_message(message) for message in recent_tail],
        ]
        return {
            "mode": "minimal_context",
            "snapshot_id": None,
            "message_count": len(self.manager._messages),
            "recent_tail_count": len(recent_tail),
            "errors": recovery_errors,
        }

    def compacted_messages(
        self,
        summary_payload: ContextSummaryPayload | None,
        *,
        recent_tail: list[dict[str, Any]],
        summary_text: str,
        partial_range: PartialCompactionRange | None = None,
    ) -> list[dict[str, Any]]:
        _ = summary_payload
        base = [safe_message(message) for message in self.manager._messages[:2]]
        if len(base) < 2:
            base = [
                {"role": "system", "content": ""},
                {"role": "user", "content": self.manager.user_goal},
            ]
        preserved = []
        if partial_range is not None:
            preserved = [
                safe_message(message)
                for message in self.manager._messages[2:]
                if not message_in_partial_range(message, partial_range)
            ]
        return [
            *base,
            {"role": "system", "content": f"Context summary:\n{summary_text}"},
            *preserved,
            *[safe_message(message) for message in recent_tail],
        ]

    def retire_previous_summary_items(self, summary_item_ids: list[str], *, superseded_by: str) -> None:
        for item_id in summary_item_ids:
            if item_id == superseded_by:
                continue
            self.manager.store.supersede_item(item_id, superseded_by=superseded_by)

    def summary_item_id(self) -> str:
        if self.manager._summary_envelope is not None and self.manager._summary_envelope.summary_id:
            return self.manager._summary_envelope.summary_id
        snapshot = self.manager.store.latest_snapshot(self.manager.run_id)
        if snapshot is not None:
            envelope_payload = snapshot.metadata.get("summary_envelope")
            if isinstance(envelope_payload, dict):
                envelope = ContextSummaryEnvelope.from_dict(envelope_payload)
                if envelope.summary_id:
                    self.manager._summary_envelope = envelope
                    return envelope.summary_id
        return f"summary_{self.manager.run_id}"

    @staticmethod
    def failure_payload(
        plan: CompactionPlan | None,
        exc: Exception,
        *,
        stage: str,
        focused_item_ids: set[str] | None = None,
        partial_range: PartialCompactionRange | None = None,
        fallback_result: dict[str, Any] | None = None,
    ) -> dict[str, Any]:
        return {
            "stage": stage,
            "error_type": type(exc).__name__,
            "message": str(exc),
            "fallback": "latest_snapshot_or_deterministic_tail",
            "focused_item_ids": sorted(focused_item_ids or []),
            "partial_range": partial_range.to_dict() if partial_range is not None else None,
            "fallback_result": fallback_result or {},
            "plan": plan_metadata(plan) if plan is not None else None,
        }


def plan_metadata(plan: CompactionPlan) -> dict[str, Any]:
    return {
        "groups": [
            {
                "group_id": group.group_id,
                "layer": group.layer,
                "item_type": group.item_type,
                "source_component": group.source_component,
                "item_ids": group.item_ids,
                "mode": group.mode,
            }
            for group in plan.groups
        ],
        "retained_item_ids": plan.retained_item_ids,
        "omitted_item_ids": plan.omitted_item_ids,
        "llm_group_count": len(plan.llm_groups),
        "deterministic_group_count": len(plan.deterministic_groups),
        "partial_range": plan.partial_range.to_dict() if plan.partial_range is not None else None,
    }


def safe_message(message: dict[str, Any]) -> dict[str, Any]:
    copied = dict(message)
    if "tool_calls" in copied:
        copied["tool_calls"] = [safe_tool_call(tool_call) for tool_call in copied.get("tool_calls") or []]
    return copied


def safe_tool_call(tool_call: Any) -> dict[str, Any]:
    if not isinstance(tool_call, dict):
        return {"id": "", "type": "function", "function": {"name": "<unknown>", "arguments": "{}"}}
    function_value = tool_call.get("function")
    function = function_value if isinstance(function_value, dict) else {}
    return {
        "id": str(tool_call.get("id") or ""),
        "type": str(tool_call.get("type") or "function"),
        "function": {"name": str(function.get("name") or "<unknown>"), "arguments": "{}"},
    }


def metadata_turn(metadata: dict[str, Any]) -> int | None:
    value = metadata.get("turn")
    if value is None:
        return None
    try:
        return int(value)
    except (TypeError, ValueError):
        return None


def message_in_partial_range(
    message: dict[str, Any],
    compaction_range: PartialCompactionRange,
) -> bool:
    metadata = (
        cast(dict[str, Any], message.get("metadata"))
        if isinstance(message.get("metadata"), dict)
        else {}
    )
    turn = metadata_turn(metadata)
    if turn is None and "turn" in message:
        turn = metadata_turn(message)
    if turn is not None:
        if compaction_range.start_turn is not None and turn < compaction_range.start_turn:
            return False
        return compaction_range.end_turn is None or turn <= compaction_range.end_turn
    checkpoint_id = str(metadata.get("checkpoint_id") or "")
    return bool(compaction_range.checkpoint_id and checkpoint_id == compaction_range.checkpoint_id)


def safe_base_messages(messages: list[dict[str, Any]], user_goal: str) -> list[dict[str, Any]]:
    if len(messages) >= 2:
        return [safe_message(messages[0]), safe_message(messages[1])]
    return [
        {"role": "system", "content": ""},
        {"role": "user", "content": user_goal},
    ]


def bounded_string(value: Any) -> str:
    text = str(value or "")
    encoded = text.encode("utf-8")
    if len(encoded) <= COMPACTION_FRAGMENT_LIMIT:
        return text
    return encoded[:COMPACTION_FRAGMENT_LIMIT].decode("utf-8", errors="ignore").rstrip() + "\n[truncated:compaction_fragment_cap]"


def json_object(value: str) -> dict[str, Any]:
    try:
        parsed = json.loads(value)
    except (TypeError, json.JSONDecodeError):
        return {}
    return parsed if isinstance(parsed, dict) else {}


def unique_strings(values: list[Any]) -> list[str]:
    result: list[str] = []
    seen: set[str] = set()
    for value in values:
        item = str(value)
        if not item or item in seen:
            continue
        seen.add(item)
        result.append(item)
    return result


def unique_values(values: list[Any]) -> list[Any]:
    result: list[Any] = []
    seen: set[str] = set()
    for value in values:
        key = json.dumps(value, ensure_ascii=False, sort_keys=True, default=str)
        if key in seen:
            continue
        seen.add(key)
        result.append(value)
    return result
