from __future__ import annotations

from singularity.instructions.hierarchy import InstructionHierarchy
from singularity.instructions.injection import PromptInjectionDetector
from singularity.instructions.models import (
    InjectionWarning,
    InstructionConflict,
    InstructionFrame,
    InstructionSource,
    ResolvedInstructions,
    _new_id,
)


class InstructionResolver:
    def __init__(
        self,
        *,
        hierarchy: InstructionHierarchy | None = None,
        detector: PromptInjectionDetector | None = None,
    ) -> None:
        self.hierarchy = hierarchy or InstructionHierarchy()
        self.detector = detector or PromptInjectionDetector()

    def resolve(
        self,
        sources: list[InstructionSource],
        *,
        purpose: str,
        warnings: list[InjectionWarning] | None = None,
    ) -> ResolvedInstructions:
        source_warnings = warnings if warnings is not None else self.detector.detect_many(sources)
        warning_map: dict[str, list[InjectionWarning]] = {}
        for warning in source_warnings:
            warning_map.setdefault(warning.source_id, []).append(warning)
        filtered = [
            source
            for source in sources
            if source.scope.matches(purpose=purpose, component="model")
        ]
        filtered.sort(
            key=lambda source: self.hierarchy.effective_priority(source),
            reverse=True,
        )
        frames: list[InstructionFrame] = []
        conflicts: list[InstructionConflict] = []
        accepted_instruction_sources: list[InstructionSource] = []
        for source in filtered:
            effective_priority = self.hierarchy.effective_priority(source)
            source_conflicts: list[InstructionConflict] = []
            for existing in accepted_instruction_sources:
                conflict = self.hierarchy.detect_conflict(source, existing)
                if conflict is not None:
                    source_conflicts.append(conflict)
                    conflicts.append(conflict)
            self_conflict = self.hierarchy.detect_self_authorization(source)
            if self_conflict is not None:
                source_conflicts.append(self_conflict)
                conflicts.append(self_conflict)
            can_act = self.hierarchy.source_can_act_as_instruction(source)
            retained_as_data = bool(source_conflicts and not can_act) or any(
                conflict.lower_source_id == source.source_id
                for conflict in source_conflicts
            )
            active = not retained_as_data
            if can_act and active:
                accepted_instruction_sources.append(source)
            metadata = {
                "frame_role": "instruction" if can_act and active else "data",
                "can_act_as_instruction": can_act,
            }
            if retained_as_data:
                metadata["retained_as_data"] = True
            frame = InstructionFrame(
                frame_id=_new_id("frame"),
                source=source,
                normalized_content=_normalize(source.content),
                effective_priority=effective_priority,
                effective_trust_level=source.trust_level,
                injection_warnings=warning_map.get(source.source_id, []),
                conflicts=source_conflicts,
                active=active,
                metadata=metadata,
            )
            frames.append(frame)
        return ResolvedInstructions(
            frames=frames,
            conflicts=conflicts,
            warnings=source_warnings,
        )


def _normalize(content: str) -> str:
    return "\n".join(line.rstrip() for line in content.strip().splitlines()).strip()
