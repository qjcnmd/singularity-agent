from __future__ import annotations

from typing import Any

from singularity.instructions.models import (
    InjectionWarning,
    InstructionConflict,
    InstructionFrame,
    PromptManifest,
    PromptSection,
    _new_id,
)
from singularity.observability.redaction import TraceRedactor


class PromptManifestBuilder:
    def __init__(self, *, redactor: TraceRedactor | None = None) -> None:
        self.redactor = redactor or TraceRedactor()

    def build(
        self,
        *,
        bundle_id: str,
        purpose: str,
        frames: list[InstructionFrame],
        sections: list[PromptSection],
        conflicts: list[InstructionConflict],
        warnings: list[InjectionWarning],
        prompt_hash: str,
        token_estimate: int,
        folded_developer_into_system: bool = False,
        metadata: dict[str, Any] | None = None,
    ) -> PromptManifest:
        trust_summary: dict[str, int] = {}
        priority_summary: dict[str, int] = {}
        for frame in frames:
            trust = frame.effective_trust_level.value
            priority = frame.effective_priority.value
            trust_summary[trust] = trust_summary.get(trust, 0) + 1
            priority_summary[priority] = priority_summary.get(priority, 0) + 1
        safe_metadata = self.redactor.redact_payload(
            {
                **(metadata or {}),
                "source_hashes": {
                    frame.source.source_id: frame.source.source_hash
                    for frame in frames
                },
                "conflict_ids": [conflict.conflict_id for conflict in conflicts],
                "injection_warning_patterns": sorted({warning.pattern for warning in warnings}),
            }
        )
        return PromptManifest(
            manifest_id=_new_id("manifest"),
            bundle_id=bundle_id,
            purpose=purpose,
            source_count=len(frames),
            section_count=len(sections),
            trust_summary=trust_summary,
            priority_summary=priority_summary,
            conflict_count=len(conflicts),
            injection_warning_count=len(warnings),
            redaction_applied=True,
            prompt_hash=prompt_hash,
            token_estimate=token_estimate,
            folded_developer_into_system=folded_developer_into_system,
            metadata=safe_metadata,
        )
