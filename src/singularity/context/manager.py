from __future__ import annotations

import json
from dataclasses import asdict, is_dataclass
from datetime import UTC, datetime
from pathlib import Path
from typing import TYPE_CHECKING, Any
from uuid import uuid4

from singularity.context.assembler import ContextAssembler, ContextBudget
from singularity.context.compression import (
    ContextCompressor,
    ContextSummaryValidationError,
)
from singularity.context.models import (
    CommandObservation,
    ContextAuthority,
    ContextItem,
    ContextItemType,
    ContextLayer,
    ContextReference,
    ContextRenderPolicy,
    ContextRuntime,
    ContextSensitivity,
    ContextSnapshot,
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
        self.compressor = ContextCompressor()
        self._persist_initial_messages()
        self._persist_initial_items(system_prompt=system_prompt, user_goal=user_goal)

    def close(self) -> None:
        self.store.close()

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
        if self._summary:
            items.append(
                self._make_item(
                    layer=ContextLayer.COMPRESSED_HISTORY,
                    source_runtime=ContextRuntime.SUMMARY,
                    item_type=ContextItemType.SUMMARY,
                    content=self._summary,
                    authority=ContextAuthority.SUMMARY,
                    importance=0.75,
                    pinned=True,
                    item_id=f"summary_{uuid4().hex[:8]}",
                )
            )
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
        preview = str(payload.get("content_preview") or "")
        sensitivity = self.classifier.classify(payload)
        rendered_preview = (
            self.redactor.redact_text(preview)
            if sensitivity in {ContextSensitivity.SECRET, ContextSensitivity.SENSITIVE}
            else preview
        )
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
            raw_result={
                "tool_call_id": payload.get("tool_call_id"),
                "tool_name": payload.get("tool_name"),
                "status": payload.get("status"),
                "ok": bool(payload.get("ok")),
                "content": rendered_preview,
                "content_preview": rendered_preview,
                "content_digest": payload.get("content_digest"),
                "result_ref": payload.get("raw_result_ref"),
                "artifact_refs": list(payload.get("artifact_refs") or []),
                "error_code": payload.get("error_code"),
                "error_kind": payload.get("error_kind"),
                "observation_id": payload.get("observation_id"),
                "policy_decision_id": payload.get("policy_decision_id"),
                "approval_grant_id": payload.get("approval_grant_id"),
                "truncated": bool(payload.get("truncated")),
                "redacted": True,
                "metadata": metadata,
            },
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

    def _compress_if_possible(self) -> None:
        if self.provider is None and self.model_runtime is None:
            return
        self._emit_context_event(
            "context.compaction_requested",
            {"message_count": len(self._messages)},
        )
        compression_messages = [
            {
                "role": "system",
                "content": (
                    "Summarize Singularity context as compact JSON with keys: "
                    "goal, current_state, completed_actions, pending_actions, "
                    "verified_facts, failed_attempts, policy_constraints, workspace_changes, "
                    "verification_status, open_questions, reference_ids, omitted_item_ids, confidence. "
                    "Every verified_facts entry must include reference_ids."
                ),
            },
            {
                "role": "user",
                "content": json.dumps(
                    {
                        "goal": self.user_goal,
                        "messages": [_safe_message(message) for message in self._messages],
                        "observation_ids": [
                            observation.id for observation in self.tool_observations
                        ],
                    },
                    ensure_ascii=False,
                    default=str,
                ),
            },
        ]
        try:
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
            source_items = self.store.query_items(run_id=self.run_id)
            normalized_content = self._normalize_summary_payload(
                content,
                source_item_ids=[item.item_id for item in source_items],
            )
            summary_payload = self.compressor.parse_summary(
                normalized_content,
                source_items=source_items,
            )
            summary = summary_payload.current_state
            known_ids = summary_payload.reference_ids
            self._summary = summary
            summary_item = self.add_context_item(
                self._make_item(
                    layer=ContextLayer.COMPRESSED_HISTORY,
                    source_runtime=ContextRuntime.SUMMARY,
                    item_type=ContextItemType.SUMMARY,
                    content=summary,
                    authority=ContextAuthority.SUMMARY,
                    importance=0.8,
                    pinned=True,
                )
            )
            snapshot = ContextSnapshot(
                snapshot_id=uuid4().hex,
                run_id=self.run_id,
                session_id=self.session_id,
                task_id=self.task_id,
                goal=self.user_goal,
                summary=summary,
                retained_item_ids=[f"{self.run_id}_system", f"{self.run_id}_user_goal", summary_item.item_id],
                retained_messages=[_safe_message(message) for message in self._messages[:2]],
                known_observation_ids=known_ids,
                version=self.store.current_version(self.run_id),
                created_at=self._now(),
            )
            self.store.save_snapshot(snapshot)
            self._emit_context_event(
                "context.compaction_completed",
                {"snapshot_id": snapshot.snapshot_id, "known_observation_ids": known_ids},
            )
        except Exception as exc:
            self._emit_context_event(
                "context.compaction_failed",
                {"error_type": type(exc).__name__, "message": str(exc)},
            )
            raise

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
