from __future__ import annotations

import json
from contextlib import nullcontext
from pathlib import Path
from typing import Any

from miniharness.instructions.compiler import PromptCompiler
from miniharness.instructions.config import InstructionRuntimeConfig
from miniharness.instructions.exceptions import PromptBudgetExceeded, PromptInjectionWarning
from miniharness.instructions.injection import PromptInjectionDetector
from miniharness.instructions.models import (
    InstructionCompilerInput,
    InstructionSource,
    PromptBundle,
    ResolvedInstructions,
)
from miniharness.instructions.resolver import InstructionResolver
from miniharness.instructions.sources import InstructionSourceCollector
from miniharness.model.models import ModelPurpose
from miniharness.observability.models import TraceArtifactKind, TraceEventType, TraceSeverity, TraceStatus


class InstructionRuntime:
    def __init__(
        self,
        *,
        workspace_root: Path | str,
        config: InstructionRuntimeConfig | None = None,
        trace: Any | None = None,
    ) -> None:
        self.workspace_root = Path(workspace_root).resolve(strict=False)
        self.config = config or InstructionRuntimeConfig()
        self.trace = trace
        self.collector = InstructionSourceCollector(self.workspace_root, config=self.config)
        self.detector = PromptInjectionDetector()
        self.resolver = InstructionResolver(detector=self.detector)
        self.compiler = PromptCompiler(config=self.config)
        self._summary = {
            "prompt_bundles_compiled_count": 0,
            "project_instruction_files_loaded_count": 0,
            "injection_warning_count": 0,
            "conflict_count": 0,
            "developer_message_folded_count": 0,
            "prompt_budget_exceeded_count": 0,
            "untrusted_context_sections_count": 0,
            "prompt_hash_references": [],
        }

    def collect_sources(self, **kwargs: Any) -> list[InstructionSource]:
        return self.collector.collect_sources(**kwargs)

    def resolve(
        self,
        sources: list[InstructionSource],
        *,
        purpose: str,
    ) -> ResolvedInstructions:
        warnings = self.detector.detect_many(sources)
        if self.config.fail_on_critical_injection and any(
            warning.severity == "critical" for warning in warnings
        ):
            raise PromptInjectionWarning("critical_prompt_injection_detected")
        return self.resolver.resolve(sources, purpose=purpose, warnings=warnings)

    def compile_prompt(
        self,
        resolved: ResolvedInstructions,
        *,
        purpose: str,
        supports_developer_message: bool = True,
        metadata: dict[str, Any] | None = None,
    ) -> PromptBundle:
        bundle = self.compiler.compile(
            InstructionCompilerInput(
                purpose=purpose,
                frames=resolved.frames,
                conflicts=resolved.conflicts,
                warnings=resolved.warnings,
                supports_developer_message=supports_developer_message,
                metadata=metadata or {},
            )
        )
        if (
            self.config.max_prompt_tokens is not None
            and bundle.token_estimate > self.config.max_prompt_tokens
        ):
            self._summary["prompt_budget_exceeded_count"] += 1
            raise PromptBudgetExceeded(
                f"Prompt token estimate {bundle.token_estimate} exceeds budget "
                f"{self.config.max_prompt_tokens}."
            )
        return bundle

    def build_prompt_bundle(
        self,
        *,
        sources: list[InstructionSource],
        purpose: str,
        supports_developer_message: bool = True,
        ids: dict[str, Any] | None = None,
    ) -> PromptBundle:
        resolved = self.resolve(sources, purpose=purpose)
        self._emit_resolution_events(resolved, ids=ids)
        bundle = self.compile_prompt(
            resolved,
            purpose=purpose,
            supports_developer_message=supports_developer_message,
        )
        self._record_summary(bundle, resolved, sources)
        self._emit_bundle_events(bundle, resolved, ids=ids)
        return bundle

    def build_for_model_turn(
        self,
        *,
        user_task: str,
        purpose: ModelPurpose | str,
        user_session_instructions: list[str] | None = None,
        runtime_observations: list[dict[str, Any]] | None = None,
        retrieved_content: list[dict[str, Any]] | None = None,
        tool_protocol_summary: str | None = None,
        supports_developer_message: bool = True,
        ids: dict[str, Any] | None = None,
    ) -> PromptBundle:
        purpose_text = purpose.value if isinstance(purpose, ModelPurpose) else str(purpose)
        span_cm = (
            self.trace.span("instruction.compile", runtime="instruction", ids=ids or {})
            if self.trace is not None and hasattr(self.trace, "span")
            else nullcontext()
        )
        with span_cm as span:
            span_id = getattr(span, "span_id", None)
            trace_ids = {**(ids or {})}
            if span_id:
                trace_ids["span_id"] = span_id
            try:
                sources = self.collect_sources(
                    user_task=user_task,
                    purpose=purpose_text,
                    user_session_instructions=user_session_instructions,
                    runtime_observations=runtime_observations,
                    retrieved_content=retrieved_content,
                    tool_protocol_summary=tool_protocol_summary,
                )
                self._emit_sources_collected(sources, ids=trace_ids)
                bundle = self.build_prompt_bundle(
                    sources=sources,
                    purpose=purpose_text,
                    supports_developer_message=supports_developer_message,
                    ids=trace_ids,
                )
                if self.trace is not None and hasattr(self.trace, "end_span") and span_id:
                    self.trace.end_span(span_id, status=TraceStatus.SUCCESS)
                return bundle
            except Exception as exc:
                if self.trace is not None and hasattr(self.trace, "end_span") and span_id:
                    self.trace.end_span(span_id, status=TraceStatus.FAILED, error=exc)
                raise

    def summary(self) -> dict[str, Any]:
        return {
            **self._summary,
            "prompt_hash_references": list(self._summary["prompt_hash_references"]),
        }

    def _record_summary(
        self,
        bundle: PromptBundle,
        resolved: ResolvedInstructions,
        sources: list[InstructionSource],
    ) -> None:
        self._summary["prompt_bundles_compiled_count"] += 1
        self._summary["project_instruction_files_loaded_count"] += len(
            [
                source
                for source in sources
                if source.source_type.value == "project_instruction_file"
            ]
        )
        self._summary["injection_warning_count"] += len(resolved.warnings)
        self._summary["conflict_count"] += len(resolved.conflicts)
        self._summary["developer_message_folded_count"] += int(
            bundle.manifest.folded_developer_into_system
        )
        self._summary["untrusted_context_sections_count"] += len(
            [
                section
                for section in bundle.sections
                if section.trust_level.value in {"untrusted_content", "model_generated"}
            ]
        )
        refs = self._summary["prompt_hash_references"]
        if bundle.prompt_hash not in refs:
            refs.append(bundle.prompt_hash)

    def _emit_sources_collected(
        self,
        sources: list[InstructionSource],
        *,
        ids: dict[str, Any] | None,
    ) -> None:
        self._emit(
            TraceEventType.INSTRUCTION_SOURCES_COLLECTED,
            summary="Instruction sources collected.",
            payload={
                "source_count": len(sources),
                "source_types": [source.source_type.value for source in sources],
                "source_hashes": [source.source_hash for source in sources],
            },
            ids=ids,
        )

    def _emit_resolution_events(
        self,
        resolved: ResolvedInstructions,
        *,
        ids: dict[str, Any] | None,
    ) -> None:
        for conflict in resolved.conflicts:
            self._emit(
                TraceEventType.INSTRUCTION_CONFLICT_DETECTED,
                summary=conflict.description,
                payload=conflict.to_dict(),
                ids=ids,
                severity=TraceSeverity.WARNING,
            )
        for warning in resolved.warnings:
            payload = warning.to_dict()
            if payload.get("evidence_excerpt"):
                payload["evidence_excerpt_hash"] = self._hash_text(str(payload["evidence_excerpt"]))
                payload["evidence_excerpt"] = "<redacted>"
            self._emit(
                TraceEventType.INSTRUCTION_INJECTION_DETECTED,
                summary=warning.message,
                payload=payload,
                ids=ids,
                severity=(
                    TraceSeverity.CRITICAL
                    if warning.severity == "critical"
                    else TraceSeverity.WARNING
                ),
            )

    @staticmethod
    def _hash_text(text: str) -> str:
        import hashlib

        return hashlib.sha256(text.encode("utf-8")).hexdigest()

    def _emit_bundle_events(
        self,
        bundle: PromptBundle,
        resolved: ResolvedInstructions,
        *,
        ids: dict[str, Any] | None,
    ) -> None:
        manifest_payload = bundle.manifest.to_dict()
        artifact_refs: list[str] = []
        if (
            self.config.store_prompt_manifest
            and self.trace is not None
            and hasattr(self.trace, "write_artifact")
        ):
            artifact = self.trace.write_artifact(
                kind=TraceArtifactKind.PROMPT_MANIFEST,
                text=json.dumps(manifest_payload, ensure_ascii=False, sort_keys=True, default=str),
                task_id=(ids or {}).get("task_id"),
                summary="Redacted prompt manifest.",
                sensitive=False,
                content_type="application/json",
            )
            artifact_refs = [artifact.artifact_id]
        self._emit(
            TraceEventType.PROMPT_COMPILED,
            summary="Prompt compiled.",
            payload={
                "bundle_id": bundle.bundle_id,
                "purpose": bundle.purpose,
                "message_count": len(bundle.messages),
                "section_count": len(bundle.sections),
                "prompt_hash": bundle.prompt_hash,
                "token_estimate": bundle.token_estimate,
                "conflict_count": len(resolved.conflicts),
                "injection_warning_count": len(resolved.warnings),
            },
            ids=ids,
        )
        self._emit(
            TraceEventType.PROMPT_MANIFEST_CREATED,
            summary="Prompt manifest created.",
            payload=manifest_payload,
            ids=ids,
            artifact_refs=artifact_refs,
        )

    def _emit(
        self,
        event_type: TraceEventType,
        *,
        summary: str,
        payload: dict[str, Any],
        ids: dict[str, Any] | None,
        severity: TraceSeverity = TraceSeverity.INFO,
        artifact_refs: list[str] | None = None,
    ) -> None:
        if self.trace is None:
            return
        if hasattr(self.trace, "emit"):
            self.trace.emit(
                event_type,
                runtime="instruction",
                summary=summary,
                payload=payload,
                ids=ids or {},
                severity=severity,
                artifact_refs=artifact_refs,
            )
        elif hasattr(self.trace, "record"):
            self.trace.record(event_type.value, payload)
