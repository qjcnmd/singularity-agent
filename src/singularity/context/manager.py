from __future__ import annotations

import json
import time
from contextlib import suppress
from dataclasses import asdict, is_dataclass
from datetime import UTC, datetime
from pathlib import Path
from typing import TYPE_CHECKING, Any
from uuid import uuid4

from singularity.context.assembler import ContextAssembler, ContextBudget
from singularity.context.compaction import (
    COMPACTION_RECENT_TAIL_MESSAGES,
    CompactionPlan,
    ContextCompactionCommitter,
    ContextCompactionExecutor,
    ContextCompactionPlanner,
    safe_base_messages,
    safe_message,
)
from singularity.context.compression import ContextCompressor
from singularity.context.models import (
    CommandObservation,
    ContextAuthority,
    ContextFreshness,
    ContextItem,
    ContextItemType,
    ContextLayer,
    ContextReference,
    ContextRenderPolicy,
    ContextSensitivity,
    ContextSnapshot,
    ContextSource,
    ContextSummaryEnvelope,
    ContextSummaryPayload,
    MutationEvidence,
    PartialCompactionRange,
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
from singularity.context.usage import ContextUsageReporter
from singularity.session.models import SessionResumeContext

if TYPE_CHECKING:
    from singularity.tool_protocol.models import ToolProtocolResultEnvelope


TOOL_RESULT_PREVIEW_LIMIT = 4000


class ContextManager:
    def __init__(
        self,
        *,
        system_prompt: str,
        user_goal: str,
        provider: Any | None = None,
        model_runner: Any | None = None,
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
        self.model_runner = model_runner
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
        self.compaction_planner = ContextCompactionPlanner(self)
        self.compaction_executor = ContextCompactionExecutor(self)
        self.compaction_committer = ContextCompactionCommitter(self)
        self.usage_reporter = ContextUsageReporter(
            run_id=self.run_id,
            store=self.store,
            provider=self.provider,
            model_runner=self.model_runner,
            emit_context_event=self._emit_context_event,
        )
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
                source_component=ContextSource.USER,
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
        decision_started = time.perf_counter()
        compaction_attempted = should_compress and self.assembler.needs_compression(
            messages=self._messages,
            tools=tools,
        )
        compaction_decision_duration_ms = int((time.perf_counter() - decision_started) * 1000)
        if compaction_attempted:
            self._compress_if_possible()
        try:
            bundle = self.build_bundle(
                tools=tools,
                planner_context=planner_context,
                phase_id=phase_id or self.phase_id,
                render_policy=render_policy,
                persist=False,
            )
            bundle.metadata.setdefault("timing", {})["compaction_decision_duration_ms"] = (
                compaction_decision_duration_ms
            )
            if persist:
                self.persist_bundle(bundle)
            return bundle.messages
        except Exception as exc:
            if not compaction_attempted:
                raise
            messages = self._fallback_messages_for_compaction_failure(tools=tools)
            failure_payload = self.compaction_committer.failure_payload(
                None,
                exc,
                stage="fallback_build_bundle",
                fallback_result={
                    "mode": "minimal_messages",
                    "message_count": len(messages),
                },
            )
            self._observe_compaction_failed(None, failure_payload)
            return messages

    def build_bundle(
        self,
        *,
        tools: list[dict[str, Any]] | None = None,
        planner_context: dict[str, Any] | None = None,
        phase_id: str | None = None,
        render_policy: ContextRenderPolicy | None = None,
        persist: bool = True,
    ) -> Any:
        started = time.perf_counter()
        items = self.store.query_items(run_id=self.run_id)
        active_summary_item_id = self.compaction_planner.active_summary_item_id(items)
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
                    source_component=ContextSource.PLANNER,
                    item_type=ContextItemType.PLANNER_STATE,
                    content=planner_context,
                    authority=ContextAuthority.COMPONENT,
                    importance=0.85,
                    phase_id=phase_id or self.phase_id,
                    item_id=f"planner_context_{uuid4().hex[:8]}",
                )
            )
        has_current_summary = any(
            item.layer == ContextLayer.COMPRESSED_HISTORY
            and item.item_type == ContextItemType.SUMMARY
            and item.freshness == ContextFreshness.CURRENT
            for item in items
        )
        summary_item_id = self.compaction_committer.summary_item_id()
        if self._summary and not has_current_summary:
            items.append(
                self._make_item(
                    layer=ContextLayer.COMPRESSED_HISTORY,
                    source_component=ContextSource.SUMMARY,
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
        latest_snapshot = self.store.latest_snapshot(self.run_id)
        bundle = self.assembler.build_bundle(
            items=items,
            run_id=self.run_id,
            task_id=self.task_id,
            phase_id=phase_id or self.phase_id,
            model=getattr(getattr(self.model_runner, "config", None), "default_model", "") or "",
            provider=getattr(getattr(self.provider, "settings", None), "base_url", "") or "",
            tools=tools,
            render_policy=render_policy or self.render_policy,
            compression_snapshot_id=latest_snapshot.snapshot_id if latest_snapshot else None,
        )
        self.usage_reporter.annotate_bundle_cache(
            bundle,
            previous_bundle=previous_bundle,
            last_bundle=self.last_bundle,
        )
        self.last_budget = bundle.budget
        self.last_bundle = bundle
        bundle.metadata.setdefault("timing", {})["context_assembly_duration_ms"] = int(
            (time.perf_counter() - started) * 1000
        )
        bundle.metadata.setdefault("timing", {})["compaction_decision_duration_ms"] = 0
        if not persist:
            return bundle
        self.persist_bundle(bundle)
        return bundle

    def persist_bundle(self, bundle: Any) -> None:
        self.store.save_bundle(bundle)
        timing = dict(bundle.metadata.get("timing") or {})
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
                "duration_ms": int(timing.get("context_assembly_duration_ms") or 0),
                "compaction_decision_duration_ms": int(
                    timing.get("compaction_decision_duration_ms") or 0
                ),
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
        self.usage_reporter.record_model_usage(self.last_bundle, result=result)

    def context_usage_diagnostic(self) -> dict[str, Any]:
        return self.usage_reporter.diagnostic(self.last_bundle)

    def add_context_item(self, item: ContextItem) -> ContextItem:
        if not item.token_count:
            item.token_count = self.token_counter.count_text(
                json.dumps(item.content, ensure_ascii=False, sort_keys=True, default=str)
            )
        stored = self.store.append_item(item)
        return stored

    def seed_session_resume_context(self, payload: dict[str, Any]) -> ContextItem:
        resume_context = SessionResumeContext.from_sources(
            session_id=str(payload.get("session_id") or self.session_id),
            user_goal=str(payload.get("user_goal") or self.user_goal),
            current_instruction=str(payload.get("current_instruction") or ""),
            dialogue=list(payload.get("dialogue_summary") or []),
            planner=dict(payload.get("planner_summary") or {}),
            workspace=dict(payload.get("workspace_summary") or {}),
            verification=dict(payload.get("verification_summary") or {}),
            tool_protocol=dict(payload.get("tool_protocol_summary") or {}),
            failures=dict(payload.get("failure_summary") or {}),
        )
        item = self._make_item(
            layer=ContextLayer.COMPRESSED_HISTORY,
            source_component=ContextSource.SUMMARY,
            item_type=ContextItemType.SESSION_RESUME_CONTEXT,
            content=resume_context.to_model_context(),
            authority=ContextAuthority.SUMMARY,
            importance=0.9,
            pinned=True,
            metadata={"session_resume_context": True},
        )
        return self.add_context_item(item)

    def add_assistant_message(self, message: dict[str, Any]) -> None:
        copied = dict(message)
        safe = safe_message(copied)
        self._messages.append(copied)
        self.store.append_message(run_id=self.run_id, message=safe)
        metadata = {"raw_message": True, "role": "assistant"}
        if isinstance(copied.get("metadata"), dict):
            metadata.update(copied["metadata"])
        if "turn" in copied:
            metadata["turn"] = copied["turn"]
        self.add_context_item(
            self._make_item(
                layer=ContextLayer.RECENT_DIALOGUE,
                source_component=ContextSource.MODEL,
                item_type=ContextItemType.ASSISTANT_MESSAGE,
                content=safe,
                authority=ContextAuthority.MODEL,
                importance=0.55 if not copied.get("tool_calls") else 0.8,
                metadata=metadata,
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
        rendered_preview = self.redactor.redact_text(preview)
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
                source_component=ContextSource.TOOL,
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
                    "turn": turn,
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
        envelope: ToolProtocolResultEnvelope | dict[str, Any],
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
        rendered_preview = self.redactor.redact_text(preview)
        if "content" in model_payload:
            model_payload["content"] = rendered_preview
        if "content_preview" in model_payload:
            model_payload["content_preview"] = rendered_preview
        if isinstance(model_payload.get("error_code"), str):
            model_payload["error_code"] = self.redactor.redact_text(model_payload["error_code"])
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
                source_component=ContextSource.TOOL_PROTOCOL,
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
                    "turn": turn,
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
                "source_component": ContextSource.TOOL_PROTOCOL.value,
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
        return self.add_tool_protocol_result(envelope, turn=turn)

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
                source_component=ContextSource.SUMMARY,
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
                source_component=ContextSource.POLICY,
                item_type=ContextItemType.POLICY_OBSERVATION,
                content=payload,
                authority=ContextAuthority.COMPONENT,
                importance=0.95 if payload.get("outcome") not in {"allow", "allowed"} else 0.7,
                references=refs,
            )
        )

    def add_planner_state(self, state: PlannerState | dict[str, Any]) -> ContextItem:
        payload = _plain(state)
        return self.add_context_item(
            self._make_item(
                layer=ContextLayer.PLANNER_STATE,
                source_component=ContextSource.PLANNER,
                item_type=ContextItemType.PLANNER_STATE,
                content=payload,
                authority=ContextAuthority.COMPONENT,
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
                source_component=ContextSource.MUTATION,
                item_type=ContextItemType.MUTATION_EVIDENCE,
                content=payload,
                authority=ContextAuthority.COMPONENT,
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
                source_component=ContextSource.COMMAND,
                item_type=ContextItemType.COMMAND_OBSERVATION,
                content=payload,
                authority=ContextAuthority.COMPONENT,
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
                source_component=ContextSource.VERIFICATION,
                item_type=ContextItemType.VERIFICATION_EVIDENCE,
                content=payload,
                authority=ContextAuthority.COMPONENT,
                importance=0.95 if payload.get("status") not in {"passed", "succeeded"} else 0.78,
                references=refs,
            )
        )

    def add_workspace_state(self, state: dict[str, Any]) -> ContextItem:
        return self.add_context_item(
            self._make_item(
                layer=ContextLayer.WORKSPACE_STATE,
                source_component=ContextSource.WORKSPACE_STATE,
                item_type=ContextItemType.WORKSPACE_STATE,
                content=dict(state),
                authority=ContextAuthority.COMPONENT,
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
                source_component=ContextSource.EDIT,
                item_type=ContextItemType.EDIT_EVIDENCE,
                content=payload,
                authority=ContextAuthority.COMPONENT,
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
                source_component=ContextSource.PROJECT_INDEX,
                item_type=ContextItemType.PROJECT_INDEX,
                content=payload,
                authority=ContextAuthority.COMPONENT,
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
                source_component=ContextSource.MEMORY,
                item_type=ContextItemType.MEMORY_CONTEXT,
                content=payload,
                authority=ContextAuthority.COMPONENT,
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
                source_component=ContextSource.SYSTEM,
                item_type=ContextItemType.FAILURE,
                content=failure,
                authority=ContextAuthority.COMPONENT,
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
            if item.source_component == ContextSource.PROJECT_INDEX:
                sources.append(
                    {
                        "source_type": "project_index",
                        "origin": "ProjectIndex",
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
            self.store.append_message(run_id=self.run_id, message=safe_message(message))

    def _persist_initial_items(self, *, system_prompt: str, user_goal: str) -> None:
        existing = self.store.query_items(run_id=self.run_id)
        if existing:
            return
        self.add_context_item(
            self._make_item(
                item_id=f"{self.run_id}_system",
                layer=ContextLayer.SYSTEM,
                source_component=ContextSource.SYSTEM,
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
                source_component=ContextSource.USER,
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

    def partial_compact(self, compaction_range: PartialCompactionRange) -> bool:
        return self._compress_if_possible(force=True, partial_range=compaction_range)

    def _compress_if_possible(
        self,
        *,
        force: bool = False,
        focused_item_ids: set[str] | None = None,
        partial_range: PartialCompactionRange | None = None,
    ) -> bool:
        if self.provider is None and self.model_runner is None:
            return False
        try:
            plan = self.compaction_planner.prepare(
                focused_item_ids=focused_item_ids,
                partial_range=partial_range,
            )
        except Exception as exc:
            return self._handle_compaction_failure(
                None,
                exc,
                stage="plan_preparation",
                focused_item_ids=focused_item_ids,
                partial_range=partial_range,
            )
        try:
            self._observe_compaction(plan)
        except Exception as exc:
            return self._handle_compaction_failure(
                plan,
                exc,
                stage="event_recording",
                focused_item_ids=focused_item_ids,
                partial_range=partial_range,
            )
        if not force and not plan.omitted_item_ids and self._summary is not None:
            try:
                self.compaction_committer.apply_compacted_messages(
                    self._summary_payload,
                    recent_tail=plan.recent_tail,
                    summary_text=self._summary,
                )
            except Exception as exc:
                return self._handle_compaction_failure(
                    plan,
                    exc,
                    stage="recovery",
                    focused_item_ids=focused_item_ids,
                    partial_range=partial_range,
                )
            return False
        try:
            context = self.compaction_executor.render(plan)
        except Exception as exc:
            return self._handle_compaction_failure(
                plan,
                exc,
                stage="render",
                focused_item_ids=focused_item_ids,
                partial_range=partial_range,
            )
        try:
            committed = self.compaction_committer.commit(plan, context=context)
        except Exception as exc:
            return self._handle_compaction_failure(
                plan,
                exc,
                stage="commit",
                focused_item_ids=focused_item_ids,
                partial_range=partial_range,
            )
        self._observe_compaction_committed(plan, committed)
        return True

    def _handle_compaction_failure(
        self,
        plan: CompactionPlan | None,
        exc: Exception,
        *,
        stage: str,
        focused_item_ids: set[str] | None = None,
        partial_range: PartialCompactionRange | None = None,
    ) -> bool:
        try:
            fallback_result = self.compaction_committer.recover_after_failure(plan)
        except Exception as recovery_exc:
            recent_tail = (
                plan.recent_tail
                if plan is not None
                else self._messages[2:][-COMPACTION_RECENT_TAIL_MESSAGES:]
            )
            self._messages = [
                *safe_base_messages(self._messages, self.user_goal),
                *[safe_message(message) for message in recent_tail],
            ]
            fallback_result = {
                "mode": "minimal_context",
                "snapshot_id": None,
                "message_count": len(self._messages),
                "recent_tail_count": len(recent_tail),
                "errors": [
                    {
                        "stage": "recovery",
                        "error_type": type(recovery_exc).__name__,
                        "message": str(recovery_exc),
                    }
                ],
            }
        failure_payload = self.compaction_committer.failure_payload(
            plan,
            exc,
            stage=stage,
            focused_item_ids=focused_item_ids,
            partial_range=partial_range,
            fallback_result=fallback_result,
        )
        self._observe_compaction_failed(plan, failure_payload)
        return False

    def _observe_compaction(self, plan: CompactionPlan) -> None:
        trace_error = self._emit_context_event(
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
        if trace_error is not None:
            raise trace_error

    def _observe_compaction_committed(self, plan: CompactionPlan, committed: ContextSnapshot) -> None:
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

    def _observe_compaction_failed(
        self,
        plan: CompactionPlan | None,
        failure_payload: dict[str, Any],
    ) -> None:
        with suppress(Exception):
            self.store.record_event(
                self.run_id,
                event_type="context.compaction_failed",
                payload=failure_payload,
            )
        self._emit_context_event("context.compaction_failed", failure_payload)

    def _fallback_messages_for_compaction_failure(
        self,
        *,
        tools: list[dict[str, Any]] | None = None,
    ) -> list[dict[str, Any]]:
        base = safe_base_messages(self._messages, self.user_goal)
        tail = [
            safe_message(message)
            for message in self._messages[2:][-COMPACTION_RECENT_TAIL_MESSAGES:]
        ]
        messages, budget = self.assembler.assemble(messages=[*base, *tail], tools=tools)
        self.last_budget = budget
        return messages

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
        source_component: ContextSource,
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
            source_component=source_component,
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

    def _emit_context_event(self, event_type: str, payload: dict[str, Any]) -> Exception | None:
        if self.trace is None:
            return None
        try:
            if hasattr(self.trace, "emit"):
                self.trace.emit(
                    event_type,
                    component="context",
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
        except Exception as exc:
            with suppress(Exception):
                self.store.record_event(
                    self.run_id,
                    event_type="context.event_recording_failed",
                    payload={
                        "event_type": event_type,
                        "error_type": type(exc).__name__,
                        "message": str(exc),
                    },
                )
            return exc
        return None

    @staticmethod
    def _now() -> str:
        return datetime.now(UTC).isoformat()


def _plain(value: Any) -> Any:
    if is_dataclass(value) and not isinstance(value, type):
        return asdict(value)
    if hasattr(value, "to_dict"):
        return value.to_dict()
    if hasattr(value, "model_dump"):
        return value.model_dump(mode="json")
    return value


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
