from __future__ import annotations

import json
from dataclasses import asdict, dataclass, field, is_dataclass
from datetime import UTC, datetime
from pathlib import Path
from typing import TYPE_CHECKING, Any
from uuid import uuid4

from singularity.context.assembler import ContextAssembler, ContextBudget
from singularity.context.compression import (
    ContextCompressor,
    ContextSummaryValidationError,
    summary_to_text,
)
from singularity.context.models import (
    CacheAttribution,
    CacheAttributionSource,
    CommandObservation,
    ContextAuthority,
    ContextFreshness,
    ContextItem,
    ContextItemType,
    ContextLayer,
    ContextReference,
    ContextRenderPolicy,
    ContextRuntime,
    ContextSensitivity,
    ContextSnapshot,
    ContextSummaryEnvelope,
    ContextSummaryPayload,
    MutationEvidence,
    PlannerState,
    PolicyObservation,
    ToolObservation,
    VerificationEvidence,
    digest_value,
    new_item_id,
)
from singularity.context.redaction import ContextRedactor, SensitivityClassifier
from singularity.context.store import ObservationStore
from singularity.context.tokens import TokenCounter
from singularity.provider import ToolChoiceMode

if TYPE_CHECKING:
    from singularity.tool_protocol.models import ToolProtocolResultEnvelope


TOOL_RESULT_PREVIEW_LIMIT = 4000
COMPACTION_RECENT_TAIL_MESSAGES = 8
COMPACTION_FRAGMENT_LIMIT = 8000


@dataclass(frozen=True)
class _CompactionGroup:
    group_id: str
    layer: str
    item_type: str
    source_runtime: str
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
class _CompactionPlan:
    source_item_ids: list[str]
    buckets: list[_CompactionGroup]
    retained_item_ids: list[str]
    current_summary_item_ids: list[str]
    omitted_item_ids: list[str]
    llm_buckets: list[_CompactionGroup]
    deterministic_buckets: list[_CompactionGroup]
    archive_buckets: list[_CompactionGroup]
    recent_tail: list[dict[str, Any]]
    previous_summary: ContextSummaryPayload | None = None
    cache_attribution: CacheAttribution = field(default_factory=CacheAttribution)

    @property
    def groups(self) -> list[_CompactionGroup]:
        return self.buckets

    @property
    def llm_groups(self) -> list[_CompactionGroup]:
        return self.llm_buckets

    @property
    def deterministic_groups(self) -> list[_CompactionGroup]:
        return self.deterministic_buckets

    @property
    def archive_groups(self) -> list[_CompactionGroup]:
        return self.archive_buckets


class ContextManager:
    def __init__(
        self,
        *,
        system_prompt: str,
        user_goal: str,
        provider: Any | None = None,
        model_runtime: Any | None = None,
        model_context_window: int = 128000,
        output_token_reserve: int = 4096,
        reasoning_token_reserve: int = 0,
        db_path: Path | None = None,
        run_id: str | None = None,
        session_id: str | None = None,
        task_id: str | None = None,
        phase_id: str = "context",
        token_counter: TokenCounter | None = None,
        trace: Any | None = None,
        render_policy: ContextRenderPolicy | None = None,
        allow_raw_secret_storage: bool = False,
    ) -> None:
        self.run_id = run_id or uuid4().hex
        self.session_id = session_id or self.run_id
        self.task_id = task_id or self.run_id
        self.phase_id = phase_id
        self.user_goal = user_goal
        self.provider = provider
        self.model_runtime = model_runtime
        self.trace = trace
        self.redactor = ContextRedactor()
        self.classifier = SensitivityClassifier()
        self.render_policy = render_policy or ContextRenderPolicy()
        self.token_counter = token_counter or TokenCounter()
        self.assembler = ContextAssembler(
            token_counter=self.token_counter,
            model_context_window=model_context_window,
            output_token_reserve=output_token_reserve,
            reasoning_token_reserve=reasoning_token_reserve,
            redactor=self.redactor,
        )
        self.store = ObservationStore(
            db_path,
            allow_raw_secret_storage=allow_raw_secret_storage,
            redactor=self.redactor,
            trace=trace,
        )
        self._messages: list[dict[str, Any]] = [
            {"role": "system", "content": system_prompt},
            {"role": "user", "content": user_goal},
        ]
        self.tool_observations: list[ToolObservation] = []
        self.last_budget: ContextBudget | None = None
        self.last_bundle: Any | None = None
        self._summary: str | None = None
        self._summary_payload: ContextSummaryPayload | None = None
        self._summary_envelope: ContextSummaryEnvelope | None = None
        self._compaction_generation = 0
        self.compressor = ContextCompressor()
        self._persist_initial_messages()
        self._persist_initial_items(system_prompt=system_prompt, user_goal=user_goal)

    def close(self) -> None:
        self.store.close()

    def set_user_goal(self, user_goal: str) -> None:
        if self.user_goal == user_goal:
            return
        self.user_goal = user_goal
        for message in self._messages:
            if message.get("role") == "user":
                message["content"] = user_goal
                break
        else:
            self._messages.insert(1, {"role": "user", "content": user_goal})
        previous_item_id = f"{self.run_id}_user_goal"
        item_id = f"{previous_item_id}_{uuid4().hex[:8]}"
        self.add_context_item(
            self._make_item(
                layer=ContextLayer.USER_GOAL,
                source_runtime=ContextRuntime.USER,
                item_type=ContextItemType.USER_GOAL,
                content=user_goal,
                authority=ContextAuthority.USER,
                importance=1.0,
                pinned=True,
                item_id=item_id,
            )
        )
        if self.store.load_item(previous_item_id) is not None:
            self.store.supersede_item(previous_item_id, superseded_by=item_id)

    def messages(
        self,
        *,
        tools: list[dict[str, Any]] | None = None,
        planner_context: dict[str, Any] | None = None,
        phase_id: str | None = None,
        render_policy: ContextRenderPolicy | None = None,
        persist: bool = True,
        allow_compression: bool | None = None,
    ) -> list[dict[str, Any]]:
        should_compress = persist if allow_compression is None else allow_compression
        if should_compress and self.assembler.needs_compression(messages=self._messages, tools=tools):
            self._compress_if_possible()
        bundle = self.build_bundle(
            tools=tools,
            planner_context=planner_context,
            phase_id=phase_id or self.phase_id,
            render_policy=render_policy,
            persist=persist,
        )
        return bundle.messages

    def build_bundle(
        self,
        *,
        tools: list[dict[str, Any]] | None = None,
        planner_context: dict[str, Any] | None = None,
        phase_id: str | None = None,
        render_policy: ContextRenderPolicy | None = None,
        persist: bool = True,
    ) -> Any:
        items = self.store.query_items(run_id=self.run_id)
        active_summary_item_id = self._active_summary_item_id(items)
        if active_summary_item_id is not None:
            items = [
                item
                for item in items
                if not (
                    item.layer == ContextLayer.COMPRESSED_HISTORY
                    and item.item_type == ContextItemType.SUMMARY
                    and item.freshness == ContextFreshness.CURRENT
                    and item.item_id != active_summary_item_id
                )
            ]
        if planner_context is not None:
            items.append(
                self._make_item(
                    layer=ContextLayer.PLANNER_STATE,
                    source_runtime=ContextRuntime.PLANNER,
                    item_type=ContextItemType.PLANNER_STATE,
                    content=planner_context,
                    authority=ContextAuthority.RUNTIME,
                    importance=0.85,
                    phase_id=phase_id or self.phase_id,
                    pinned=True,
                    item_id=f"planner_context_{uuid4().hex[:8]}",
                )
            )
        has_current_summary = any(
            item.layer == ContextLayer.COMPRESSED_HISTORY
            and item.item_type == ContextItemType.SUMMARY
            and item.freshness == ContextFreshness.CURRENT
            for item in items
        )
        summary_item_id = self._summary_item_id()
        if self._summary and not has_current_summary:
            items.append(
                self._make_item(
                    layer=ContextLayer.COMPRESSED_HISTORY,
                    source_runtime=ContextRuntime.SUMMARY,
                    item_type=ContextItemType.SUMMARY,
                    content=self._summary,
                    authority=ContextAuthority.SUMMARY,
                    importance=0.75,
                    pinned=False,
                    item_id=summary_item_id,
                    metadata={
                        "summary_envelope": (
                            self._summary_envelope.to_dict()
                            if self._summary_envelope is not None
                            else {}
                        ),
                        "summary_payload": (
                            self._summary_payload.to_dict()
                            if self._summary_payload is not None
                            else {}
                        ),
                    },
                )
            )
        previous_bundle = self.store.latest_bundle(self.run_id)
        bundle = self.assembler.build_bundle(
            items=items,
            run_id=self.run_id,
            task_id=self.task_id,
            phase_id=phase_id or self.phase_id,
            model=getattr(getattr(self.model_runtime, "config", None), "default_model", "") or "",
            provider=getattr(getattr(self.provider, "settings", None), "base_url", "") or "",
            tools=tools,
            render_policy=render_policy or self.render_policy,
            compression_snapshot_id=(
                self.store.latest_snapshot(self.run_id).snapshot_id
                if self.store.latest_snapshot(self.run_id)
                else None
            ),
        )
        self._annotate_bundle_cache(bundle, previous_bundle=previous_bundle)
        self.last_budget = bundle.budget
        self.last_bundle = bundle
        if not persist:
            return bundle
        self.persist_bundle(bundle)
        return bundle

    def persist_bundle(self, bundle: Any) -> None:
        self.store.save_bundle(bundle)
        self._emit_context_event(
            "context.bundle_built",
            {
                "bundle_id": bundle.bundle_id,
                "included": len(bundle.included_item_ids),
                "excluded": len(bundle.excluded_item_ids),
                "message_tokens": bundle.budget.message_tokens,
                "tool_schema_tokens": bundle.budget.tool_schema_tokens,
                "cached_input_tokens": (bundle.metadata.get("cache") or {}).get("cached_input_tokens", 0),
                "cache_hit_ratio": (bundle.metadata.get("cache") or {}).get("cache_hit_ratio", 0.0),
            },
        )
        self._emit_context_event(
            "context.rendered_for_model",
            {
                "bundle_id": bundle.bundle_id,
                "message_count": len(bundle.messages),
                "included": len(bundle.included_item_ids),
                "excluded": len(bundle.excluded_item_ids),
                "message_tokens": bundle.budget.message_tokens,
                "tool_schema_tokens": bundle.budget.tool_schema_tokens,
                "cache_miss_reasons": (bundle.metadata.get("cache") or {}).get("cache_miss_reasons", []),
            },
        )

    def record_model_usage(self, result: Any) -> None:
        if self.last_bundle is None:
            return
        usage = getattr(result, "usage", None)
        if usage is None:
            return
        input_tokens = int(getattr(usage, "input_tokens", 0) or 0)
        cached_tokens = int(getattr(usage, "cached_input_tokens", 0) or 0)
        cache_hit_ratio = _ratio(cached_tokens, input_tokens)
        cache = dict(self.last_bundle.metadata.get("cache") or {})
        cache_attribution = dict(cache.get("cache_attribution") or {})
        result_cache = dict((getattr(result, "metadata", {}) or {}).get("cache") or {})
        result_attribution = (
            dict(result_cache.get("cache_attribution") or {})
            if isinstance(result_cache, dict)
            else {}
        )
        if result_attribution:
            cache_attribution.update(result_attribution)
        if cached_tokens > 0:
            cache_attribution.update(
                CacheAttribution(
                    source=CacheAttributionSource.PROVIDER_NATIVE,
                    confidence=1.0,
                    reasons=[
                        *[str(item) for item in cache_attribution.get("reasons") or []],
                        "usage.cached_input_tokens",
                    ],
                    evidence=[
                        *[str(item) for item in cache_attribution.get("evidence") or []],
                        "usage.cached_input_tokens",
                    ],
                    provider_name=cache_attribution.get("provider_name"),
                    model_name=cache_attribution.get("model_name"),
                ).to_dict()
            )
        elif not cache_attribution:
            cache_attribution = self._current_cache_attribution().to_dict()
        cache.update(
            {
                "input_tokens": input_tokens,
                "cached_input_tokens": cached_tokens,
                "cache_hit_ratio": cache_hit_ratio,
                "cache_miss_reasons": list(
                    (getattr(result, "metadata", {}) or {}).get("cache_miss_reasons")
                    or cache.get("cache_miss_reasons")
                    or []
                ),
                "cache_attribution": cache_attribution,
            }
        )
        self.last_bundle.metadata["cache"] = cache
        report = dict(self.last_bundle.metadata.get("context_usage_report") or {})
        report.update(
            {
                "input_tokens": input_tokens,
                "cached_input_tokens": cached_tokens,
                "cache_hit_ratio": cache_hit_ratio,
                "cache_miss_reasons": cache["cache_miss_reasons"],
                "cache_attribution": cache_attribution,
            }
        )
        self.last_bundle.metadata["context_usage_report"] = report
        self.store.update_bundle_metadata(
            bundle_id=self.last_bundle.bundle_id,
            run_id=self.run_id,
            metadata=self.last_bundle.metadata,
        )
        self._emit_context_event(
            "context.cache_usage_recorded",
            {
                "bundle_id": self.last_bundle.bundle_id,
                "input_tokens": input_tokens,
                "cached_input_tokens": cached_tokens,
                "cache_hit_ratio": cache_hit_ratio,
                "cache_miss_reasons": cache["cache_miss_reasons"],
                "cache_attribution": cache_attribution,
            },
        )

    def add_context_item(self, item: ContextItem) -> ContextItem:
        if not item.token_count:
            item.token_count = self.token_counter.count_text(
                json.dumps(item.content, ensure_ascii=False, sort_keys=True, default=str)
            )
        stored = self.store.append_item(item)
        return stored

    def add_assistant_message(self, message: dict[str, Any]) -> None:
        copied = dict(message)
        safe = _safe_message(copied)
        self._messages.append(copied)
        self.store.append_message(run_id=self.run_id, message=safe)
        self.add_context_item(
            self._make_item(
                layer=ContextLayer.RECENT_DIALOGUE,
                source_runtime=ContextRuntime.MODEL,
                item_type=ContextItemType.ASSISTANT_MESSAGE,
                content=safe,
                authority=ContextAuthority.MODEL,
                importance=0.55 if not copied.get("tool_calls") else 0.8,
            )
        )

    def add_tool_result(
        self,
        *,
        tool_call: dict[str, Any],
        result: dict[str, Any],
        turn: int = 0,
    ) -> ToolObservation:
        function = tool_call.get("function") or {}
        tool_name = function.get("name", "<unknown>")
        tool_call_id = tool_call.get("id")
        preview, truncated, truncation_reason = self._preview_result(result)
        sensitivity = self.classifier.classify(result)
        rendered_preview = (
            self.redactor.redact_text(preview)
            if sensitivity in {ContextSensitivity.SECRET, ContextSensitivity.SENSITIVE}
            else preview
        )
        raw_digest = digest_value(result)
        references = self._references_for_result(result, raw_digest=raw_digest)
        metadata = dict(result.get("metadata") or {})
        observation = ToolObservation(
            id=uuid4().hex,
            run_id=self.run_id,
            turn=turn,
            tool_name=tool_name,
            tool_call_id=tool_call_id,
            ok=bool(result.get("ok")),
            raw_result=result,
            preview=rendered_preview,
            truncated=truncated,
            metadata={
                "result_keys": sorted(result.keys()),
                **metadata,
            },
            created_at=self._now(),
            input_tokens=self.token_counter.count_text(
                json.dumps(tool_call, ensure_ascii=False, sort_keys=True, default=str)
            ),
            preview_tokens=self.token_counter.count_text(rendered_preview),
            raw_digest=raw_digest,
            source_refs=[],
            cache_hit=bool(metadata.get("cache_hit")),
            duration_seconds=metadata.get("duration_seconds"),
            error_code=result.get("error_code"),
            tool_version=metadata.get("tool_version"),
            truncation_reason=truncation_reason,
            sensitivity=sensitivity,
        )
        observation.source_refs = [
            ContextReference(
                ref_id=ref.ref_id,
                ref_type=ref.ref_type,
                target=ref.target,
                path=ref.path,
                line_start=ref.line_start,
                line_end=ref.line_end,
                digest=ref.digest,
                source_item_id=observation.id,
                observation_id=observation.id,
                metadata=ref.metadata,
            )
            for ref in references
        ]
        self.tool_observations.append(observation)
        self.store.save_observation(observation)
        tool_message = self._tool_message(observation)
        self._messages.append(tool_message)
        self.store.append_message(run_id=self.run_id, message=tool_message)
        self.add_context_item(
            self._make_item(
                item_id=observation.id,
                layer=ContextLayer.TOOL_OBSERVATIONS,
                source_runtime=ContextRuntime.TOOL,
                item_type=ContextItemType.TOOL_OBSERVATION,
                content=tool_message,
                authority=ContextAuthority.TOOL,
                sensitivity=sensitivity,
                importance=0.7 if observation.ok else 0.9,
                references=observation.source_refs,
                metadata={
                    "tool_name": tool_name,
                    "tool_call_id": tool_call_id,
                    "ok": observation.ok,
                    "raw_digest": raw_digest,
                    "error_code": observation.error_code,
                    "truncated": observation.truncated,
                },
            )
        )
        self._emit_context_event(
            "context.item_added",
            {
                "tool_name": tool_name,
                "tool_call_id": tool_call_id,
                "ok": observation.ok,
                "preview_tokens": observation.preview_tokens,
                "raw_digest": raw_digest,
                "sensitivity": sensitivity.value,
            },
        )
        return observation

    def add_tool_protocol_result(
        self,
        envelope: "ToolProtocolResultEnvelope | dict[str, Any]",
        *,
        turn: int = 0,
    ) -> ToolObservation:
        from singularity.tool_protocol.models import ToolProtocolResultEnvelope

        result_envelope = (
            ToolProtocolResultEnvelope.from_dict(envelope)
            if isinstance(envelope, dict)
            else envelope
        )
        payload = result_envelope.to_dict()
        model_payload = result_envelope.to_observation_view().to_model_payload()
        preview = str(payload.get("content_preview") or "")
        sensitivity = self.classifier.classify(payload)
        rendered_preview = (
            self.redactor.redact_text(preview)
            if sensitivity in {ContextSensitivity.SECRET, ContextSensitivity.SENSITIVE}
            else preview
        )
        if "content" in model_payload:
            model_payload["content"] = rendered_preview
        if "content_preview" in model_payload:
            model_payload["content_preview"] = rendered_preview
        model_payload["redacted"] = True
        raw_digest = digest_value(payload)
        metadata = {
            "status": payload.get("status"),
            "policy_decision_id": payload.get("policy_decision_id"),
            "approval_grant_id": payload.get("approval_grant_id"),
            "truncated": bool(payload.get("truncated")),
            "redacted": True,
            "result_ref": payload.get("raw_result_ref"),
            **{
                key: value
                for key, value in dict(payload.get("metadata") or {}).items()
                if key not in {"raw_result", "raw_args", "raw_arguments", "result"}
            },
        }
        observation = ToolObservation(
            id=uuid4().hex,
            run_id=self.run_id,
            turn=turn,
            tool_name=str(payload.get("tool_name") or "<unknown>"),
            tool_call_id=payload.get("tool_call_id"),
            ok=bool(payload.get("ok")),
            raw_result=model_payload,
            preview=rendered_preview,
            truncated=bool(payload.get("truncated")),
            metadata=metadata,
            created_at=self._now(),
            input_tokens=0,
            preview_tokens=self.token_counter.count_text(rendered_preview),
            raw_digest=raw_digest,
            source_refs=[
                ContextReference(
                    ref_id=str(ref),
                    ref_type="artifact",
                    target=str(ref),
                    source_item_id="",
                )
                for ref in list(payload.get("artifact_refs") or [])
            ],
            cache_hit=bool(metadata.get("cache_hit")),
            duration_seconds=metadata.get("duration_seconds"),
            error_code=payload.get("error_code"),
            tool_version=metadata.get("tool_version"),
            truncation_reason="tool_result" if payload.get("truncated") else None,
            sensitivity=sensitivity,
        )
        self.tool_observations.append(observation)
        self.store.save_observation(observation)
        tool_message = self._tool_message(observation)
        self._messages.append(tool_message)
        self.store.append_message(run_id=self.run_id, message=tool_message)
        self.add_context_item(
            self._make_item(
                item_id=observation.id,
                layer=ContextLayer.TOOL_OBSERVATIONS,
                source_runtime=ContextRuntime.TOOL_PROTOCOL,
                item_type=ContextItemType.TOOL_OBSERVATION,
                content=tool_message,
                authority=ContextAuthority.TOOL,
                sensitivity=sensitivity,
                importance=0.7 if observation.ok else 0.9,
                references=observation.source_refs,
                metadata={
                    "tool_name": observation.tool_name,
                    "tool_call_id": observation.tool_call_id,
                    "ok": observation.ok,
                    "raw_digest": raw_digest,
                    "error_code": observation.error_code,
                    "truncated": observation.truncated,
                    "result_ref": payload.get("raw_result_ref"),
                },
            )
        )
        self._emit_context_event(
            "context.item_added",
            {
                "tool_name": observation.tool_name,
                "tool_call_id": observation.tool_call_id,
                "ok": observation.ok,
                "preview_tokens": observation.preview_tokens,
                "raw_digest": raw_digest,
                "sensitivity": sensitivity.value,
                "source_runtime": ContextRuntime.TOOL_PROTOCOL.value,
            },
        )
        return observation

    def add_synthetic_tool_error(
        self,
        *,
        tool_call: dict[str, Any],
        error_code: str,
        message: str,
        turn: int = 0,
        metadata: dict[str, Any] | None = None,
    ) -> ToolObservation:
        from singularity.tool_protocol.models import ToolProtocolResultEnvelope

        tool_name = str((tool_call.get("function") or {}).get("name") or "<unknown>")
        envelope = ToolProtocolResultEnvelope(
            tool_call_id=str(tool_call.get("id") or ""),
            tool_name=tool_name,
            ok=False,
            status="rejected",
            error_code=error_code,
            content_preview=message,
            content_digest=digest_value(
                {
                    "tool_call_id": tool_call.get("id"),
                    "tool_name": tool_name,
                    "error_code": error_code,
                    "message": message,
                }
            ),
            redacted=True,
            truncated=False,
            metadata={"synthetic": True, **(metadata or {})},
        )
        observation = self.add_tool_protocol_result(envelope)
        if turn:
            observation.turn = turn
        return observation

    def add_trace_summary(self, lines: list[str]) -> None:
        if not lines:
            return
        content = "\n".join(lines)
        message = {"role": "system", "content": content}
        self._messages.append(message)
        self.store.append_message(run_id=self.run_id, message=message)
        self.add_context_item(
            self._make_item(
                layer=ContextLayer.FAILURE_MEMORY,
                source_runtime=ContextRuntime.SUMMARY,
                item_type=ContextItemType.SUMMARY,
                content=content,
                authority=ContextAuthority.SUMMARY,
                importance=0.65,
                metadata={"raw_message": True, "role": "system"},
            )
        )
        self._emit_context_event(
            "context.item_added",
            {"line_count": len(lines), "content_digest": digest_value(lines)},
        )

    def add_policy_observation(self, observation: PolicyObservation | dict[str, Any]) -> ContextItem:
        payload = _plain(observation)
        refs = []
        if payload.get("reference"):
            refs.append(
                ContextReference(
                    ref_id=str(payload["reference"]),
                    ref_type="policy_decision",
                    target=str(payload.get("decision_id") or payload["reference"]),
                    source_item_id="",
                )
            )
        return self.add_context_item(
            self._make_item(
                layer=ContextLayer.POLICY_STATE,
                source_runtime=ContextRuntime.POLICY,
                item_type=ContextItemType.POLICY_OBSERVATION,
                content=payload,
                authority=ContextAuthority.RUNTIME,
                importance=0.95 if payload.get("outcome") not in {"allow", "allowed"} else 0.7,
                references=refs,
            )
        )

    def add_planner_state(self, state: PlannerState | dict[str, Any]) -> ContextItem:
        payload = _plain(state)
        return self.add_context_item(
            self._make_item(
                layer=ContextLayer.PLANNER_STATE,
                source_runtime=ContextRuntime.PLANNER,
                item_type=ContextItemType.PLANNER_STATE,
                content=payload,
                authority=ContextAuthority.RUNTIME,
                importance=0.9,
                pinned=True,
                phase_id=str(payload.get("current_phase") or self.phase_id),
            )
        )

    def add_mutation_evidence(self, evidence: MutationEvidence | dict[str, Any]) -> ContextItem:
        payload = _plain(evidence)
        refs = []
        if payload.get("transaction_id"):
            refs.append(
                ContextReference(
                    ref_id=f"ref_tx_{payload['transaction_id']}",
                    ref_type="transaction",
                    target=str(payload["transaction_id"]),
                    source_item_id="",
                )
            )
        return self.add_context_item(
            self._make_item(
                layer=ContextLayer.EVIDENCE,
                source_runtime=ContextRuntime.MUTATION,
                item_type=ContextItemType.MUTATION_EVIDENCE,
                content=payload,
                authority=ContextAuthority.RUNTIME,
                importance=0.85,
                references=refs,
            )
        )

    def add_command_observation(self, observation: CommandObservation | dict[str, Any]) -> ContextItem:
        payload = _plain(observation)
        refs = []
        if payload.get("output_ref"):
            refs.append(
                ContextReference(
                    ref_id=str(payload["output_ref"]),
                    ref_type="artifact",
                    target=str(payload["output_ref"]),
                    source_item_id="",
                )
            )
        return self.add_context_item(
            self._make_item(
                layer=ContextLayer.EVIDENCE,
                source_runtime=ContextRuntime.COMMAND,
                item_type=ContextItemType.COMMAND_OBSERVATION,
                content=payload,
                authority=ContextAuthority.RUNTIME,
                importance=0.9 if payload.get("status") not in {"succeeded", "completed"} else 0.72,
                references=refs,
            )
        )

    def add_verification_evidence(self, evidence: VerificationEvidence | dict[str, Any]) -> ContextItem:
        payload = _plain(evidence)
        refs = []
        if payload.get("check_id"):
            refs.append(
                ContextReference(
                    ref_id=f"ref_verify_{payload['check_id']}",
                    ref_type="verification",
                    target=str(payload["check_id"]),
                    source_item_id="",
                    metadata={"logs_ref": payload.get("logs_ref")},
                )
            )
        return self.add_context_item(
            self._make_item(
                layer=ContextLayer.VERIFICATION,
                source_runtime=ContextRuntime.VERIFICATION,
                item_type=ContextItemType.VERIFICATION_EVIDENCE,
                content=payload,
                authority=ContextAuthority.RUNTIME,
                importance=0.95 if payload.get("status") not in {"passed", "succeeded"} else 0.78,
                references=refs,
            )
        )

    def add_workspace_state(self, state: dict[str, Any]) -> ContextItem:
        return self.add_context_item(
            self._make_item(
                layer=ContextLayer.WORKSPACE_STATE,
                source_runtime=ContextRuntime.WORKSPACE_STATE,
                item_type=ContextItemType.WORKSPACE_STATE,
                content=dict(state),
                authority=ContextAuthority.RUNTIME,
                importance=0.82,
            )
        )

    def add_edit_result(self, result: dict[str, Any]) -> ContextItem:
        payload = _bounded_edit_payload(result)
        refs = []
        if payload.get("edit_plan_id"):
            refs.append(
                ContextReference(
                    ref_id=f"ref_edit_plan_{payload['edit_plan_id']}",
                    ref_type="edit_plan",
                    target=str(payload["edit_plan_id"]),
                    source_item_id="",
                )
            )
        if payload.get("patch_digest"):
            refs.append(
                ContextReference(
                    ref_id=f"ref_patch_{str(payload['patch_digest'])[:16]}",
                    ref_type="patch_digest",
                    target=str(payload["patch_digest"]),
                    source_item_id="",
                )
            )
        return self.add_context_item(
            self._make_item(
                layer=ContextLayer.EVIDENCE,
                source_runtime=ContextRuntime.EDIT,
                item_type=ContextItemType.EDIT_EVIDENCE,
                content=payload,
                authority=ContextAuthority.RUNTIME,
                importance=0.88 if payload.get("ok") else 0.94,
                references=refs,
            )
        )

    def add_project_index(self, observation: dict[str, Any]) -> ContextItem:
        payload = dict(observation)
        payload["trust_level"] = "untrusted_workspace_data"
        return self.add_context_item(
            self._make_item(
                layer=ContextLayer.WORKSPACE_STATE,
                source_runtime=ContextRuntime.PROJECT_INDEX,
                item_type=ContextItemType.PROJECT_INDEX,
                content=payload,
                authority=ContextAuthority.RUNTIME,
                sensitivity=ContextSensitivity.WORKSPACE,
                importance=0.86,
                metadata={
                    "index_id": payload.get("index_id"),
                    "freshness": ((payload.get("summary") or {}).get("freshness")),
                    "trust_level": "untrusted_workspace_data",
                },
            )
        )

    def add_memory_context_block(self, block: Any) -> ContextItem:
        payload = block.to_dict() if hasattr(block, "to_dict") else dict(block)
        payload["trust_level"] = "untrusted_memory"
        return self.add_context_item(
            self._make_item(
                layer=ContextLayer.FAILURE_MEMORY,
                source_runtime=ContextRuntime.MEMORY,
                item_type=ContextItemType.MEMORY_CONTEXT,
                content=payload,
                authority=ContextAuthority.RUNTIME,
                sensitivity=ContextSensitivity.WORKSPACE,
                importance=float(payload.get("priority") or 0.65),
                metadata={
                    "trust_level": "untrusted_memory",
                    "pollution_risk": payload.get("pollution_risk"),
                    "token_budget": payload.get("budget"),
                    "item_count": len(payload.get("items") or []),
                },
            )
        )

    def add_failure(self, failure: dict[str, Any] | str) -> ContextItem:
        return self.add_context_item(
            self._make_item(
                layer=ContextLayer.FAILURE_MEMORY,
                source_runtime=ContextRuntime.SYSTEM,
                item_type=ContextItemType.FAILURE,
                content=failure,
                authority=ContextAuthority.RUNTIME,
                importance=0.9,
            )
        )

    def pin_item(self, item_id: str) -> None:
        self.store.set_item_pinned(item_id, pinned=True)

    def mark_item_stale(self, item_id: str, *, reason: str = "") -> None:
        self.store.mark_stale(item_id, reason=reason)

    def instruction_sources(self) -> list[dict[str, Any]]:
        sources: list[dict[str, Any]] = []
        for observation in self.tool_observations:
            source_type = "tool_output"
            if observation.tool_name in {"run_command", "read_process_output", "start_process"}:
                source_type = "command_output"
            elif "verification" in observation.tool_name:
                source_type = "verification_evidence"
            elif observation.tool_name.startswith("edit_"):
                source_type = "edit_evidence"
            elif observation.tool_name == "workspace_health":
                source_type = "workspace_state"
            elif "index" in observation.tool_name:
                source_type = "project_index"
            sources.append(
                {
                    "source_type": source_type,
                    "origin": observation.tool_name,
                    "content": observation.preview,
                    "trust_level": "untrusted_content",
                    "metadata": {
                        "observation_id": observation.id,
                        "tool_call_id": observation.tool_call_id,
                        "tool_name": observation.tool_name,
                        "ok": observation.ok,
                        "truncated": observation.truncated,
                        "raw_digest": observation.raw_digest,
                        "reference_ids": [ref.ref_id for ref in observation.source_refs],
                        "error_code": observation.error_code,
                    },
                }
            )
        if self._summary:
            sources.append(
                {
                    "source_type": "context_summary",
                    "origin": "context.compaction",
                    "content": self._summary,
                    "trust_level": "untrusted_content",
                    "metadata": {"summary": True},
                }
            )
        for item in self.store.query_items(run_id=self.run_id):
            if item.source_runtime == ContextRuntime.PROJECT_INDEX:
                sources.append(
                    {
                        "source_type": "project_index",
                        "origin": "ProjectIndexRuntime",
                        "content": json.dumps(item.content, ensure_ascii=False, sort_keys=True, default=str),
                        "trust_level": "untrusted_content",
                        "metadata": {
                            "item_id": item.item_id,
                            "content_digest": item.content_digest,
                            "freshness": item.freshness.value,
                        },
                    }
                )
        return sources

    def _persist_initial_messages(self) -> None:
        if self.store.load_messages(self.run_id):
            self._messages = self.store.load_messages(self.run_id)
            return
        for message in self._messages:
            self.store.append_message(run_id=self.run_id, message=_safe_message(message))

    def _persist_initial_items(self, *, system_prompt: str, user_goal: str) -> None:
        existing = self.store.query_items(run_id=self.run_id)
        if existing:
            return
        self.add_context_item(
            self._make_item(
                item_id=f"{self.run_id}_system",
                layer=ContextLayer.SYSTEM,
                source_runtime=ContextRuntime.SYSTEM,
                item_type=ContextItemType.SYSTEM_INSTRUCTION,
                content=system_prompt,
                authority=ContextAuthority.SYSTEM,
                sensitivity=ContextSensitivity.PUBLIC,
                importance=1.0,
                pinned=True,
            )
        )
        self.add_context_item(
            self._make_item(
                item_id=f"{self.run_id}_user_goal",
                layer=ContextLayer.USER_GOAL,
                source_runtime=ContextRuntime.USER,
                item_type=ContextItemType.USER_GOAL,
                content=user_goal,
                authority=ContextAuthority.USER,
                sensitivity=ContextSensitivity.PUBLIC,
                importance=1.0,
                pinned=True,
            )
        )

    def compact_context(self) -> bool:
        return self._compress_if_possible(force=True)

    def focused_compact(self, item_ids: list[str]) -> bool:
        return self._compress_if_possible(force=True, focused_item_ids=set(item_ids))

    def partial_compact(self, item_ids: list[str]) -> bool:
        return self._compress_if_possible(force=True, focused_item_ids=set(item_ids))

    def _compress_if_possible(
        self,
        *,
        force: bool = False,
        focused_item_ids: set[str] | None = None,
    ) -> bool:
        if self.provider is None and self.model_runtime is None:
            return False
        plan = self._prepare_compaction_plan(focused_item_ids=focused_item_ids)
        self._observe_compaction(plan)
        if not force and not plan.omitted_item_ids and self._summary is not None:
            self._apply_compacted_messages(
                self._summary_payload,
                recent_tail=plan.recent_tail,
                summary_text=self._summary,
            )
            return False
        try:
            context = self._render_compaction(plan)
            committed = self._commit_compaction(plan, context=context)
            self._observe_compaction_committed(plan, committed)
            return True
        except Exception as exc:
            failure_payload = self._compaction_failure_payload(plan, exc)
            self._observe_compaction_failed(plan, failure_payload)
            self._recover_after_compaction_failure(plan)
            return False

    def _prepare_compaction_plan(self, *, focused_item_ids: set[str] | None = None) -> _CompactionPlan:
        source_items = [
            item
            for item in self.store.query_items(run_id=self.run_id)
            if item.freshness == ContextFreshness.CURRENT
        ]
        if focused_item_ids is not None:
            source_items = [
                item
                for item in source_items
                if item.item_id in focused_item_ids or item.pinned
            ]
        previous_summary = self._previous_summary_payload()
        retained = set(self._required_retained_item_ids(source_items))
        current_summary_item_ids = self._current_summary_item_ids(source_items)
        retained.update(current_summary_item_ids)
        recent_tail = self._recent_tail_messages()
        for item in self._recent_tail_items(source_items):
            retained.add(item.item_id)
        buckets = self._bucketize_compaction_items(source_items, retained=retained)
        omitted = [item_id for bucket in buckets for item_id in bucket.item_ids]
        plan = _CompactionPlan(
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
            cache_attribution=self._current_cache_attribution(source_items=source_items, previous_summary=previous_summary),
        )
        return plan

    def _bucketize_compaction_items(
        self,
        source_items: list[ContextItem],
        *,
        retained: set[str],
    ) -> list[_CompactionGroup]:
        buckets: list[_CompactionGroup] = []
        for item in source_items:
            if item.item_id in retained:
                continue
            if item.item_type in {
                ContextItemType.SYSTEM_INSTRUCTION,
                ContextItemType.USER_GOAL,
            }:
                retained.add(item.item_id)
                continue
            mode = self._compaction_mode(item)
            fragment = self._compaction_fragment(item)
            utility_score = self._utility_score(item)
            token_cost = int(item.token_count or 0)
            volatility = self._volatility_score(item)
            reference_density = self._reference_density(item)
            recency_score = self._recency_score(item)
            buckets.append(
                _CompactionGroup(
                    group_id=self._bucket_id(item),
                    layer=item.layer.value,
                    item_type=item.item_type.value,
                    source_runtime=item.source_runtime.value,
                    item_ids=[item.item_id],
                    mode=mode,
                    utility_score=utility_score,
                    token_cost=token_cost,
                    volatility=volatility,
                    reference_density=reference_density,
                    recency_score=recency_score,
                    content_digest=item.content_digest,
                    fragment=fragment,
                )
            )
        return sorted(buckets, key=self._bucket_sort_key)

    @staticmethod
    def _bucket_id(item: ContextItem) -> str:
        return f"{item.layer.value}:{item.item_type.value}:{item.source_runtime.value}:{item.item_id}"

    @staticmethod
    def _bucket_sort_key(bucket: _CompactionGroup) -> tuple[Any, ...]:
        return (
            -bucket.utility_score,
            bucket.token_cost,
            -bucket.volatility,
            -bucket.reference_density,
            -bucket.recency_score,
            bucket.content_digest,
            bucket.item_ids[0],
        )

    def _utility_score(self, item: ContextItem) -> float:
        score = float(item.importance)
        score += self._layer_weight(item.layer) / 100.0
        score += self._authority_weight(item.authority) / 100.0
        if item.relevance_score is not None:
            score += float(item.relevance_score)
        if item.pinned:
            score += 10.0
        if item.freshness == ContextFreshness.STALE:
            score -= 1.0
        return score

    @staticmethod
    def _volatility_score(item: ContextItem) -> float:
        if item.layer in {ContextLayer.RECENT_DIALOGUE, ContextLayer.FAILURE_MEMORY}:
            return 1.0
        if item.item_type in {
            ContextItemType.ASSISTANT_MESSAGE,
            ContextItemType.USER_MESSAGE,
            ContextItemType.FAILURE,
        }:
            return 0.8
        if item.item_type in {
            ContextItemType.VERIFICATION_EVIDENCE,
            ContextItemType.MUTATION_EVIDENCE,
            ContextItemType.EDIT_EVIDENCE,
            ContextItemType.COMMAND_OBSERVATION,
        }:
            return 0.5
        return 0.2

    @staticmethod
    def _reference_density(item: ContextItem) -> float:
        refs = max(1, len(item.references))
        tokens = max(1, int(item.token_count or 1))
        return round(refs / tokens, 4)

    @staticmethod
    def _recency_score(item: ContextItem) -> float:
        if item.freshness == ContextFreshness.CURRENT:
            return 1.0
        if item.freshness == ContextFreshness.STALE:
            return 0.3
        return 0.0

    def _observe_compaction(self, plan: _CompactionPlan) -> None:
        self._emit_context_event(
            "context.compaction_requested",
            {
                "message_count": len(self._messages),
                "bucket_count": len(plan.buckets),
                "llm_bucket_count": len(plan.llm_buckets),
                "deterministic_bucket_count": len(plan.deterministic_buckets),
                "archive_bucket_count": len(plan.archive_buckets),
                "omitted_item_ids": plan.omitted_item_ids,
                "retained_item_ids": plan.retained_item_ids,
                "source_item_ids": plan.source_item_ids,
                "cache_attribution": plan.cache_attribution.to_dict(),
            },
        )

    def _render_compaction(self, plan: _CompactionPlan) -> dict[str, Any]:
        llm_payload: dict[str, Any] | None = None
        if plan.llm_buckets:
            llm_payload = self._run_llm_compaction(plan)
        summary_payload = self._summary_payload_from_plan(
            plan,
            llm_payload=llm_payload,
            source_items=[item for item in self.store.query_items(run_id=self.run_id) if item.item_id in set(plan.source_item_ids)],
        )
        validated = self.compressor.parse_summary(
            json.dumps(summary_payload, ensure_ascii=False, sort_keys=True, default=str),
            source_items=[item for item in self.store.query_items(run_id=self.run_id) if item.item_id in set(plan.source_item_ids)],
            previous_summary=plan.previous_summary,
        )
        envelope = self._summary_envelope_for_plan(
            plan,
            summary=validated,
            rendered_summary=self._render_summary_for_context(validated),
        )
        return {
            "summary": validated,
            "envelope": envelope,
            "summary_text": envelope.rendered_summary,
        }

    def _commit_compaction(self, plan: _CompactionPlan, *, context: dict[str, Any]) -> ContextSnapshot:
        validated = context["summary"]
        envelope = context["envelope"]
        summary_text = context["summary_text"]
        known_ids = validated.reference_ids
        self._summary = summary_text
        self._summary_payload = validated
        self._summary_envelope = envelope
        self._compaction_generation += 1
        summary_item = self.add_context_item(
                self._make_item(
                    item_id=envelope.summary_id or self._summary_item_id(),
                    layer=ContextLayer.COMPRESSED_HISTORY,
                    source_runtime=ContextRuntime.SUMMARY,
                    item_type=ContextItemType.SUMMARY,
                    content=summary_text,
                    authority=ContextAuthority.SUMMARY,
                    importance=0.8,
                    pinned=True,
                    metadata={
                    "summary_payload": validated.to_dict(),
                    "summary_envelope": envelope.to_dict(),
                    "summary_digest": envelope.summary_digest,
                    "compaction_generation": self._compaction_generation,
                    "compaction_plan": self._plan_metadata(plan),
                },
            )
        )
        self._retire_previous_summary_items(plan.current_summary_item_ids, superseded_by=summary_item.item_id)
        omitted_item_ids = [
            item_id
            for item_id in validated.omitted_item_ids
            if item_id not in set(plan.retained_item_ids)
            and item_id != summary_item.item_id
        ]
        self.store.compact_items(
            run_id=self.run_id,
            omitted_item_ids=omitted_item_ids,
            summary_item_id=summary_item.item_id,
        )
        retained_messages = self._compacted_messages(
            validated,
            recent_tail=plan.recent_tail,
            summary_text=summary_text,
        )
        snapshot = ContextSnapshot(
            snapshot_id=uuid4().hex,
            run_id=self.run_id,
            session_id=self.session_id,
            task_id=self.task_id,
            goal=self.user_goal,
            summary=summary_text,
            retained_item_ids=[*plan.retained_item_ids, summary_item.item_id],
            retained_messages=retained_messages,
            known_observation_ids=known_ids,
            version=self.store.current_version(self.run_id),
            created_at=self._now(),
            metadata={
                "summary_payload": validated.to_dict(),
                "summary_envelope": envelope.to_dict(),
                "summary_digest": envelope.summary_digest,
                "omitted_item_ids": omitted_item_ids,
                "compaction_generation": self._compaction_generation,
                "compaction_plan": self._plan_metadata(plan),
                "cache_attribution": envelope.cache_attribution.to_dict(),
            },
        )
        self.store.save_summary(
            run_id=self.run_id,
            summary_id=summary_item.item_id,
            payload=envelope.to_dict(),
            source_item_ids=plan.source_item_ids,
        )
        self.store.save_snapshot(snapshot)
        self._messages = retained_messages
        return snapshot

    def _observe_compaction_committed(self, plan: _CompactionPlan, committed: ContextSnapshot) -> None:
        self._emit_context_event(
            "context.compaction_completed",
            {
                "snapshot_id": committed.snapshot_id,
                "summary_item_id": committed.retained_item_ids[-1] if committed.retained_item_ids else "",
                "known_observation_ids": committed.known_observation_ids,
                "omitted_item_ids": committed.metadata.get("omitted_item_ids") or [],
                "compaction_generation": self._compaction_generation,
                "cache_attribution": committed.metadata.get("cache_attribution") or {},
            },
        )

    def _observe_compaction_failed(self, plan: _CompactionPlan, failure_payload: dict[str, Any]) -> None:
        self.store.record_event(
            self.run_id,
            event_type="context.compaction_failed",
            payload=failure_payload,
        )
        self._emit_context_event("context.compaction_failed", failure_payload)

    def _compaction_failure_payload(self, plan: _CompactionPlan, exc: Exception) -> dict[str, Any]:
        return {
            "error_type": type(exc).__name__,
            "message": str(exc),
            "fallback": "latest_snapshot_or_deterministic_tail",
            "plan": self._plan_metadata(plan),
        }

    def _summary_item_id(self) -> str:
        if self._summary_envelope is not None and self._summary_envelope.summary_id:
            return self._summary_envelope.summary_id
        snapshot = self.store.latest_snapshot(self.run_id)
        if snapshot is not None:
            envelope_payload = snapshot.metadata.get("summary_envelope")
            if isinstance(envelope_payload, dict):
                envelope = ContextSummaryEnvelope.from_dict(envelope_payload)
                if envelope.summary_id:
                    self._summary_envelope = envelope
                    return envelope.summary_id
        return f"summary_{self.run_id}"

    def _next_summary_item_id(self, *, summary_digest: str) -> str:
        generation = self._compaction_generation + 1
        suffix = summary_digest[:12] if summary_digest else "pending"
        return f"summary_{self.run_id}_{generation:04d}_{suffix}"

    @staticmethod
    def _layer_weight(layer: ContextLayer) -> float:
        weights = {
            ContextLayer.SYSTEM: 100,
            ContextLayer.USER_GOAL: 90,
            ContextLayer.TASK_STATE: 40,
            ContextLayer.PLANNER_STATE: 38,
            ContextLayer.POLICY_STATE: 36,
            ContextLayer.VERIFICATION: 34,
            ContextLayer.FAILURE_MEMORY: 32,
            ContextLayer.WORKSPACE_STATE: 30,
            ContextLayer.EVIDENCE: 26,
            ContextLayer.TOOL_OBSERVATIONS: 24,
            ContextLayer.COMPRESSED_HISTORY: 22,
            ContextLayer.RECENT_DIALOGUE: 10,
            ContextLayer.REFERENCES: 8,
            ContextLayer.SCRATCHPAD: 0,
        }
        return float(weights.get(layer, 0))

    @staticmethod
    def _authority_weight(authority: ContextAuthority) -> float:
        weights = {
            ContextAuthority.SYSTEM: 10,
            ContextAuthority.USER: 9,
            ContextAuthority.RUNTIME: 7,
            ContextAuthority.TOOL: 6,
            ContextAuthority.SUMMARY: 4,
            ContextAuthority.MODEL: 1,
        }
        return float(weights.get(authority, 0))

    def _current_cache_attribution(
        self,
        *,
        source_items: list[ContextItem] | None = None,
        previous_summary: ContextSummaryPayload | None = None,
    ) -> CacheAttribution:
        provider_name = None
        model_name = None
        if self.model_runtime is not None:
            config = getattr(self.model_runtime, "config", None)
            model_name = getattr(config, "default_model", None) or getattr(config, "model", None)
        elif self.provider is not None:
            provider_name = getattr(self.provider, "provider_name", None) or getattr(self.provider, "name", lambda: None)()
        reasons = []
        evidence = []
        confidence = 0.0
        source = CacheAttributionSource.UNKNOWN
        if self.last_bundle is not None:
            cache = dict(self.last_bundle.metadata.get("cache") or {})
            cache_attribution = dict(cache.get("cache_attribution") or {})
            if cache.get("cached_input_tokens"):
                source = CacheAttributionSource.PROVIDER_NATIVE
                confidence = 1.0
                evidence.append("bundle_cache.cached_input_tokens")
                reasons.append("provider_native_cache_diagnostic_present")
            elif cache_attribution.get("source") == CacheAttributionSource.PROVIDER_NATIVE.value:
                source = CacheAttributionSource.PROVIDER_NATIVE
                confidence = float(cache_attribution.get("confidence") or 0.9)
                evidence.extend(str(item) for item in cache_attribution.get("evidence") or [])
                reasons.extend(str(item) for item in cache_attribution.get("reasons") or [])
            else:
                reasons.append("no_native_cache_diagnostics")
        else:
            reasons.append("no_bundle")
        if source == CacheAttributionSource.UNKNOWN and (source_items or previous_summary):
            source = CacheAttributionSource.RUNTIME_INFERRED
            confidence = 0.35 if source_items else 0.2
            if previous_summary is not None:
                evidence.append("previous_summary_present")
        return CacheAttribution(
            source=source,
            confidence=confidence,
            reasons=reasons,
            evidence=evidence,
            provider_name=provider_name,
            model_name=model_name,
        )

    def _summary_envelope_for_plan(
        self,
        plan: _CompactionPlan,
        *,
        summary: ContextSummaryPayload,
        rendered_summary: str,
    ) -> ContextSummaryEnvelope:
        previous = self._previous_summary_payload()
        previous_digest = digest_value(previous) if previous is not None else None
        summary_digest = digest_value(summary)
        summary_id = self._next_summary_item_id(summary_digest=summary_digest)
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
                "compaction_generation": self._compaction_generation + 1,
                "compaction_plan": self._plan_metadata(plan),
            },
        )

    @staticmethod
    def _current_summary_item_ids(items: list[ContextItem]) -> list[str]:
        return [
            item.item_id
            for item in items
            if item.layer == ContextLayer.COMPRESSED_HISTORY
            and item.item_type == ContextItemType.SUMMARY
            and item.freshness == ContextFreshness.CURRENT
        ]

    @staticmethod
    def _active_summary_item_id(items: list[ContextItem]) -> str | None:
        summary_ids = ContextManager._current_summary_item_ids(items)
        return summary_ids[-1] if summary_ids else None

    def _retire_previous_summary_items(self, summary_item_ids: list[str], *, superseded_by: str) -> None:
        for item_id in summary_item_ids:
            if item_id == superseded_by:
                continue
            self.store.supersede_item(item_id, superseded_by=superseded_by)

    def _summary_payload_from_plan(
        self,
        plan: _CompactionPlan,
        *,
        llm_payload: dict[str, Any] | None,
        source_items: list[ContextItem],
    ) -> dict[str, Any]:
        payload = dict(llm_payload or {})
        deterministic = self._deterministic_summary(plan, source_items=source_items)
        previous = plan.previous_summary.__dict__ if plan.previous_summary is not None else {}
        reference_ids = _unique_strings(
            list(previous.get("reference_ids") or [])
            + list(payload.get("reference_ids") or [])
            + deterministic["reference_ids"]
        )
        return {
            "goal": self.user_goal,
            "current_state": str(
                payload.get("current_state")
                or payload.get("summary")
                or deterministic["current_state"]
            ),
            "completed_actions": _unique_values(
                list(previous.get("completed_actions") or [])
                + list(payload.get("completed_actions") or [])
            ),
            "pending_actions": _unique_values(list(payload.get("pending_actions") or [])),
            "verified_facts": _unique_values(
                list(previous.get("verified_facts") or [])
                + list(payload.get("verified_facts") or [])
                + deterministic["verified_facts"]
            ),
            "failed_attempts": _unique_values(
                list(previous.get("failed_attempts") or [])
                + list(payload.get("failed_attempts") or [])
                + deterministic["failed_attempts"]
            ),
            "policy_constraints": _unique_strings(
                list(previous.get("policy_constraints") or [])
                + list(payload.get("policy_constraints") or [])
                + deterministic["policy_constraints"]
            ),
            "workspace_changes": _unique_values(
                list(previous.get("workspace_changes") or [])
                + list(payload.get("workspace_changes") or [])
                + deterministic["workspace_changes"]
            ),
            "verification_status": deterministic["verification_status"]
            or str(payload.get("verification_status") or previous.get("verification_status") or "unknown"),
            "open_questions": _unique_values(
                list(payload.get("open_questions") or [])
                + list(previous.get("open_questions") or [])
            ),
            "reference_ids": reference_ids,
            "omitted_item_ids": list(plan.omitted_item_ids),
            "confidence": float(payload.get("confidence") or 0.65),
        }

    def _required_retained_item_ids(self, items: list[ContextItem]) -> list[str]:
        retained: set[str] = set()
        for item in items:
            if item.pinned or item.layer in {ContextLayer.SYSTEM, ContextLayer.USER_GOAL}:
                retained.add(item.item_id)
        for layer, item_type in (
            (ContextLayer.POLICY_STATE, ContextItemType.POLICY_OBSERVATION),
            (ContextLayer.PLANNER_STATE, ContextItemType.PLANNER_STATE),
        ):
            latest = self._latest_item(items, layer=layer, item_type=item_type)
            if latest is not None:
                retained.add(latest.item_id)
        return list(retained)

    @staticmethod
    def _latest_item(
        items: list[ContextItem],
        *,
        layer: ContextLayer,
        item_type: ContextItemType,
    ) -> ContextItem | None:
        for item in reversed(items):
            if item.layer == layer and item.item_type == item_type:
                return item
        return None

    def _recent_tail_items(self, items: list[ContextItem]) -> list[ContextItem]:
        max_tail_tokens = max(32, int(self.assembler.model_context_window * 0.1))
        candidates = [
            item
            for item in items
            if item.layer in {ContextLayer.RECENT_DIALOGUE, ContextLayer.TOOL_OBSERVATIONS}
            and int(item.token_count or 0) <= max_tail_tokens
        ]
        return candidates[-COMPACTION_RECENT_TAIL_MESSAGES:]

    def _recent_tail_messages(self) -> list[dict[str, Any]]:
        max_tail_tokens = max(32, int(self.assembler.model_context_window * 0.1))
        history = [_safe_message(message) for message in self._messages[2:]]
        bounded = [
            message
            for message in history
            if self.token_counter.count_message(message) <= max_tail_tokens
        ]
        return bounded[-COMPACTION_RECENT_TAIL_MESSAGES:]

    @staticmethod
    def _compaction_mode(item: ContextItem) -> str:
        if item.layer in {ContextLayer.RECENT_DIALOGUE, ContextLayer.FAILURE_MEMORY}:
            return "llm"
        if item.item_type in {ContextItemType.FAILURE, ContextItemType.ASSISTANT_MESSAGE, ContextItemType.USER_MESSAGE}:
            return "llm"
        return "deterministic"

    def _compaction_fragment(self, item: ContextItem) -> dict[str, Any]:
        payload = item.content if isinstance(item.content, dict) else {"content": item.content}
        refs = [ref.ref_id for ref in item.references] or [item.item_id]
        base = {
            "item_id": item.item_id,
            "layer": item.layer.value,
            "item_type": item.item_type.value,
            "source_runtime": item.source_runtime.value,
            "reference_ids": refs,
            "raw_digest": item.content_digest,
        }
        if item.item_type == ContextItemType.TOOL_OBSERVATION:
            content = payload.get("content")
            tool_payload = _json_object(content) if isinstance(content, str) else payload
            metadata = payload.get("metadata") if isinstance(payload.get("metadata"), dict) else {}
            return {
                **base,
                "tool_name": payload.get("name") or payload.get("tool_name") or tool_payload.get("tool_name"),
                "tool_call_id": payload.get("tool_call_id") or tool_payload.get("tool_call_id"),
                "ok": tool_payload.get("ok") if "ok" in tool_payload else payload.get("ok"),
                "preview": _bounded_string(
                    tool_payload.get("preview")
                    or tool_payload.get("content")
                    or payload.get("preview")
                    or "",
                ),
                "artifact_refs": tool_payload.get("artifact_refs") or metadata.get("artifact_refs") or [],
                "logs_ref": tool_payload.get("logs_ref") or metadata.get("logs_ref"),
            }
        if isinstance(item.content, dict) and item.content.get("role"):
            safe = _safe_message(item.content)
            if isinstance(safe.get("content"), str):
                safe["content"] = _bounded_string(safe["content"])
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
            "preview": _bounded_string(
                json.dumps(item.content, ensure_ascii=False, sort_keys=True, default=str)
            ),
        }

    def _run_llm_compaction(self, plan: _CompactionPlan) -> dict[str, Any]:
        compression_messages = [
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
                        "goal": self.user_goal,
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
        if self.model_runtime is not None:
            from singularity.model import (
                ModelBudget,
                ModelPurpose,
                ModelTurnRequest,
                ToolChoiceMode as RuntimeToolChoiceMode,
                ToolChoicePolicy,
            )

            request_id = f"model_compact_{uuid4().hex[:12]}"
            result = self.model_runtime.run_turn(
                ModelTurnRequest(
                    request_id=request_id,
                    run_id=self.run_id,
                    session_id=self.session_id,
                    task_id=self.task_id,
                    phase_id="context_compaction",
                    action_id=request_id,
                    purpose=ModelPurpose.COMPACT_CONTEXT,
                    messages=compression_messages,
                    tools=[],
                    tool_choice=ToolChoicePolicy(mode=RuntimeToolChoiceMode.NONE),
                    budget=ModelBudget(),
                    context_metadata={"compaction_plan": self._plan_metadata(plan)},
                    trace_metadata={"compaction_plan": self._plan_metadata(plan)},
                )
            )
            content = result.assistant_message.text if result.assistant_message else ""
        else:
            response = self.provider.chat(
                messages=compression_messages,
                tools=[],
                tool_choice=ToolChoiceMode.NONE,
            )
            content = (
                ((response.get("choices") or [{}])[0].get("message") or {}).get("content")
                or ""
            )
        normalized = self._normalize_summary_payload(
            content,
            source_item_ids=plan.omitted_item_ids,
        )
        return json.loads(normalized)

    def _deterministic_summary(
        self,
        plan: _CompactionPlan,
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
            "reference_ids": _unique_strings(reference_ids),
            "verified_facts": verified_facts,
            "failed_attempts": failed_attempts,
            "policy_constraints": _unique_strings(policy_constraints),
            "workspace_changes": workspace_changes,
            "verification_status": latest_verification_status,
        }

    def _previous_summary_payload(self) -> ContextSummaryPayload | None:
        if self._summary_payload is not None:
            return self._summary_payload
        snapshot = self.store.latest_snapshot(self.run_id)
        if snapshot is None:
            return None
        envelope_payload = snapshot.metadata.get("summary_envelope")
        if isinstance(envelope_payload, dict):
            envelope = ContextSummaryEnvelope.from_dict(envelope_payload)
            if envelope.summary_payload is not None:
                self._summary_envelope = envelope
                self._summary_payload = envelope.summary_payload
                self._summary = envelope.rendered_summary or self._summary
                return envelope.summary_payload
        payload = snapshot.metadata.get("summary_payload")
        if not isinstance(payload, dict):
            return None
        try:
            summary = ContextSummaryPayload.from_dict(payload)
            self._summary_payload = summary
            self._summary = snapshot.summary or self._summary
            return summary
        except Exception:
            return None

    def _compacted_messages(
        self,
        summary_payload: ContextSummaryPayload | None,
        *,
        recent_tail: list[dict[str, Any]],
        summary_text: str,
    ) -> list[dict[str, Any]]:
        base = [_safe_message(message) for message in self._messages[:2]]
        if len(base) < 2:
            base = [
                {"role": "system", "content": ""},
                {"role": "user", "content": self.user_goal},
            ]
        return [
            *base,
            {"role": "system", "content": f"Context summary:\n{summary_text}"},
            *[_safe_message(message) for message in recent_tail],
        ]

    def _apply_compacted_messages(
        self,
        summary_payload: ContextSummaryPayload | None,
        *,
        recent_tail: list[dict[str, Any]],
        summary_text: str,
    ) -> None:
        self._messages = self._compacted_messages(
            summary_payload,
            recent_tail=recent_tail,
            summary_text=summary_text,
        )

    def _recover_after_compaction_failure(self, plan: _CompactionPlan) -> None:
        snapshot = self.store.latest_snapshot(self.run_id)
        if snapshot is not None and snapshot.retained_messages:
            self._summary = snapshot.summary
            self._messages = list(snapshot.retained_messages)
            envelope_payload = snapshot.metadata.get("summary_envelope")
            if isinstance(envelope_payload, dict):
                envelope = ContextSummaryEnvelope.from_dict(envelope_payload)
                self._summary_envelope = envelope
                self._summary_payload = envelope.summary_payload
            return
        self._messages = [
            *_safe_base_messages(self._messages, self.user_goal),
            *[_safe_message(message) for message in plan.recent_tail],
        ]

    @staticmethod
    def _render_summary_for_context(summary: ContextSummaryPayload) -> str:
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
    def _plan_metadata(plan: _CompactionPlan) -> dict[str, Any]:
        return {
            "groups": [
                {
                    "group_id": group.group_id,
                    "layer": group.layer,
                    "item_type": group.item_type,
                    "source_runtime": group.source_runtime,
                    "item_ids": group.item_ids,
                    "mode": group.mode,
                }
                for group in plan.groups
            ],
            "retained_item_ids": plan.retained_item_ids,
            "omitted_item_ids": plan.omitted_item_ids,
            "llm_group_count": len(plan.llm_groups),
            "deterministic_group_count": len(plan.deterministic_groups),
        }

    def _annotate_bundle_cache(self, bundle: Any, *, previous_bundle: Any | None) -> None:
        cache = dict(bundle.metadata.get("cache") or {})
        reasons = self._cache_miss_reasons(bundle, previous_bundle=previous_bundle)
        cache["cache_miss_reasons"] = reasons
        cache.setdefault("cache_attribution", self._current_cache_attribution().to_dict())
        bundle.metadata["cache"] = cache
        report = dict(bundle.metadata.get("context_usage_report") or {})
        report["cache_miss_reasons"] = reasons
        report.setdefault("cache_attribution", cache["cache_attribution"])
        bundle.metadata["context_usage_report"] = report

    def _cache_miss_reasons(self, bundle: Any, *, previous_bundle: Any | None) -> list[str]:
        if previous_bundle is None:
            return ["first_request"]
        reasons: list[str] = []
        previous = previous_bundle.metadata or {}
        current = bundle.metadata or {}
        if previous.get("context_shape_hash") != current.get("context_shape_hash"):
            reasons.append("context_shape_change")
        if previous.get("context_ordering_hash") != current.get("context_ordering_hash"):
            reasons.append("ordering_change")
        if previous_bundle.compression_snapshot_id != bundle.compression_snapshot_id:
            reasons.append("compaction_change")
        previous_tool_tokens = getattr(previous_bundle.budget, "tool_schema_tokens", 0)
        if previous_tool_tokens != bundle.budget.tool_schema_tokens:
            reasons.append("tool_schema_change")
        return reasons

    def _tool_message(self, observation: ToolObservation) -> dict[str, Any]:
        raw_result = observation.raw_result
        if isinstance(raw_result, dict) and {"tool_call_id", "tool_name", "status"}.issubset(raw_result):
            content = raw_result
        else:
            content = {
                "ok": observation.ok,
                "tool_name": observation.tool_name,
                "tool_call_id": observation.tool_call_id,
                "observation_id": observation.id,
                "reference_ids": [ref.ref_id for ref in observation.source_refs],
                "content": observation.preview,
                "truncated": observation.truncated,
                "truncation_reason": observation.truncation_reason,
                "raw_digest": observation.raw_digest,
            }
        return {
            "role": "tool",
            "tool_call_id": observation.tool_call_id,
            "name": observation.tool_name,
            "content": json.dumps(content, ensure_ascii=False),
        }

    def _make_item(
        self,
        *,
        layer: ContextLayer,
        source_runtime: ContextRuntime,
        item_type: ContextItemType,
        content: Any,
        authority: ContextAuthority,
        item_id: str | None = None,
        phase_id: str | None = None,
        sensitivity: ContextSensitivity | None = None,
        importance: float = 0.5,
        references: list[ContextReference] | None = None,
        metadata: dict[str, Any] | None = None,
        pinned: bool = False,
    ) -> ContextItem:
        if sensitivity is None:
            sensitivity = self.classifier.classify(content)
        refs = references or []
        resolved_item_id = item_id or new_item_id(item_type.value)
        for ref in refs:
            if not ref.source_item_id:
                ref.source_item_id = resolved_item_id
            if not ref.observation_id:
                ref.observation_id = resolved_item_id
        return ContextItem(
            item_id=resolved_item_id,
            run_id=self.run_id,
            session_id=self.session_id,
            task_id=self.task_id,
            phase_id=phase_id or self.phase_id,
            layer=layer,
            source_runtime=source_runtime,
            item_type=item_type,
            content=content,
            authority=authority,
            sensitivity=sensitivity,
            token_count=self.token_counter.count_text(
                json.dumps(content, ensure_ascii=False, sort_keys=True, default=str)
            ),
            importance=importance,
            references=refs,
            metadata=metadata or {},
            pinned=pinned,
        )

    @staticmethod
    def _preview_result(result: dict[str, Any]) -> tuple[str, bool, str | None]:
        content = result.get("content")
        if isinstance(content, dict) and isinstance(content.get("content"), str):
            source = content["content"]
        elif isinstance(content, str):
            source = content
        else:
            source = json.dumps(result, ensure_ascii=False, sort_keys=True, default=str)
        result_already_truncated = bool(result.get("truncated"))
        if len(source) <= TOOL_RESULT_PREVIEW_LIMIT:
            return source, result_already_truncated, (
                "tool_result" if result_already_truncated else None
            )
        return source[:TOOL_RESULT_PREVIEW_LIMIT], True, "preview_limit"

    def _references_for_result(
        self,
        result: dict[str, Any],
        *,
        raw_digest: str,
    ) -> list[ContextReference]:
        content = result.get("content")
        payload = content if isinstance(content, dict) else result
        path = payload.get("path") if isinstance(payload, dict) else None
        if not path:
            return []
        line_start = payload.get("line") if isinstance(payload, dict) else None
        line_end = line_start
        return [
            ContextReference(
                ref_id=f"ref_{uuid4().hex}",
                ref_type="file",
                target=str(path),
                path=str(path),
                line_start=line_start if isinstance(line_start, int) else None,
                line_end=line_end if isinstance(line_end, int) else None,
                digest=raw_digest,
            )
        ]

    @staticmethod
    def _parse_summary(content: str) -> tuple[str, list[str]]:
        try:
            parsed = json.loads(content)
        except json.JSONDecodeError:
            return content, []
        summary = parsed.get("summary") or parsed.get("current_state") or content
        reference_ids = parsed.get("reference_ids") or []
        if not isinstance(reference_ids, list):
            reference_ids = []
        return str(summary), [str(reference_id) for reference_id in reference_ids]

    @staticmethod
    def _normalize_summary_payload(content: str, *, source_item_ids: list[str]) -> str:
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

    def _emit_context_event(self, event_type: str, payload: dict[str, Any]) -> None:
        if self.trace is None:
            return
        if hasattr(self.trace, "emit"):
            self.trace.emit(
                event_type,
                runtime="context",
                summary=event_type,
                payload=payload,
                ids={
                    "run_id": self.run_id,
                    "session_id": self.session_id,
                    "task_id": self.task_id,
                    "phase_id": self.phase_id,
                },
            )
        elif hasattr(self.trace, "record"):
            self.trace.record(event_type, payload)

    @staticmethod
    def _now() -> str:
        return datetime.now(UTC).isoformat()


def _plain(value: Any) -> Any:
    if is_dataclass(value):
        return asdict(value)
    if hasattr(value, "to_dict"):
        return value.to_dict()
    if hasattr(value, "model_dump"):
        return value.model_dump(mode="json")
    return value


def _safe_message(message: dict[str, Any]) -> dict[str, Any]:
    copied = dict(message)
    if "tool_calls" in copied:
        copied["tool_calls"] = [_safe_tool_call(tool_call) for tool_call in copied.get("tool_calls") or []]
    return copied


def _safe_tool_call(tool_call: Any) -> dict[str, Any]:
    if not isinstance(tool_call, dict):
        return {"id": "", "type": "function", "function": {"name": "<unknown>", "arguments": "{}"}}
    function = tool_call.get("function") if isinstance(tool_call.get("function"), dict) else {}
    return {
        "id": str(tool_call.get("id") or ""),
        "type": str(tool_call.get("type") or "function"),
        "function": {"name": str(function.get("name") or "<unknown>"), "arguments": "{}"},
    }


def _safe_base_messages(messages: list[dict[str, Any]], user_goal: str) -> list[dict[str, Any]]:
    if len(messages) >= 2:
        return [_safe_message(messages[0]), _safe_message(messages[1])]
    return [
        {"role": "system", "content": ""},
        {"role": "user", "content": user_goal},
    ]


def _bounded_string(value: Any) -> str:
    text = str(value or "")
    encoded = text.encode("utf-8")
    if len(encoded) <= COMPACTION_FRAGMENT_LIMIT:
        return text
    return encoded[:COMPACTION_FRAGMENT_LIMIT].decode("utf-8", errors="ignore").rstrip() + "\n[truncated:compaction_fragment_cap]"


def _json_object(value: str) -> dict[str, Any]:
    try:
        parsed = json.loads(value)
    except (TypeError, json.JSONDecodeError):
        return {}
    return parsed if isinstance(parsed, dict) else {}


def _unique_strings(values: list[Any]) -> list[str]:
    result: list[str] = []
    seen: set[str] = set()
    for value in values:
        item = str(value)
        if not item or item in seen:
            continue
        seen.add(item)
        result.append(item)
    return result


def _unique_values(values: list[Any]) -> list[Any]:
    result: list[Any] = []
    seen: set[str] = set()
    for value in values:
        key = json.dumps(value, ensure_ascii=False, sort_keys=True, default=str)
        if key in seen:
            continue
        seen.add(key)
        result.append(value)
    return result


def _ratio(cached_tokens: int, input_tokens: int) -> float:
    if input_tokens <= 0:
        return 0.0
    return round(cached_tokens / input_tokens, 4)


def _bounded_edit_payload(result: dict[str, Any]) -> dict[str, Any]:
    validation = result.get("validation") or {}
    issues = validation.get("issues") or []
    return {
        "edit_result_id": result.get("edit_result_id"),
        "edit_plan_id": result.get("edit_plan_id"),
        "intent_id": result.get("intent_id"),
        "strategy": result.get("strategy"),
        "status": result.get("status"),
        "ok": result.get("ok"),
        "patch_candidate_id": result.get("patch_candidate_id"),
        "patch_digest": result.get("patch_digest"),
        "changed_files": list(result.get("changed_files") or [])[:50],
        "changeset_id": result.get("changeset_id"),
        "transaction_id": result.get("transaction_id"),
        "verification_plan_id": (result.get("verification_plan") or {}).get("id")
        or (result.get("verification_plan") or {}).get("verification_plan_id"),
        "validation": {
            "ok": validation.get("ok"),
            "requires_review": validation.get("requires_review"),
            "failure_category": validation.get("failure_category"),
            "issue_codes": [issue.get("code") for issue in issues if isinstance(issue, dict)][:20],
            "diff_summary": list(validation.get("diff_summary") or [])[:20],
        },
        "repair_attempts": [
            {
                "attempt": attempt.get("attempt"),
                "category": attempt.get("category"),
                "action": attempt.get("action"),
                "status": attempt.get("status"),
            }
            for attempt in list(result.get("repair_attempts") or [])[:5]
            if isinstance(attempt, dict)
        ],
        "error_code": result.get("error_code"),
        "message": result.get("message"),
    }
