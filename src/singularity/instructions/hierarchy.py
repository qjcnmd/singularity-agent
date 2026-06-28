from __future__ import annotations

from singularity.instructions.models import (
    InstructionConflict,
    InstructionPriority,
    InstructionSource,
    InstructionSourceType,
    TrustLevel,
    _new_id,
    _priority_rank,
)

UNTRUSTED_SOURCE_TYPES = {
    InstructionSourceType.PROJECT_FILE,
    InstructionSourceType.README,
    InstructionSourceType.TOOL_OUTPUT,
    InstructionSourceType.COMMAND_OUTPUT,
    InstructionSourceType.MODEL_OUTPUT,
    InstructionSourceType.CONTEXT_SUMMARY,
}


class InstructionHierarchy:
    def compare(self, priority_a: InstructionPriority, priority_b: InstructionPriority) -> int:
        return _priority_rank(priority_a) - _priority_rank(priority_b)

    def can_override(
        self,
        higher: InstructionPriority,
        lower: InstructionPriority,
    ) -> bool:
        return self.compare(higher, lower) > 0

    def effective_priority(self, source: InstructionSource) -> InstructionPriority:
        if source.source_type in {
            InstructionSourceType.TOOL_OUTPUT,
            InstructionSourceType.COMMAND_OUTPUT,
            InstructionSourceType.PROJECT_FILE,
            InstructionSourceType.README,
        }:
            return InstructionPriority.RETRIEVED_CONTENT
        if source.source_type == InstructionSourceType.MODEL_OUTPUT:
            return InstructionPriority.MODEL_GENERATED
        return source.priority

    def source_can_act_as_instruction(self, source: InstructionSource) -> bool:
        if source.source_type in UNTRUSTED_SOURCE_TYPES:
            return False
        if source.trust_level in {TrustLevel.UNTRUSTED_CONTENT, TrustLevel.MODEL_GENERATED}:
            return False
        return source.priority not in {
            InstructionPriority.RETRIEVED_CONTENT,
            InstructionPriority.MODEL_GENERATED,
        }

    def detect_conflict(
        self,
        candidate: InstructionSource,
        existing: InstructionSource,
    ) -> InstructionConflict | None:
        if not _looks_conflicting(candidate.content):
            return None
        candidate_priority = self.effective_priority(candidate)
        existing_priority = self.effective_priority(existing)
        if self.compare(existing_priority, candidate_priority) >= 0:
            return InstructionConflict(
                conflict_id=_new_id("conflict"),
                higher_source_id=existing.source_id,
                lower_source_id=candidate.source_id,
                description=(
                    f"{candidate.source_id} appears to conflict with "
                    f"{existing.source_id}."
                ),
                resolution=(
                    f"{candidate.priority.value} cannot override "
                    f"{existing.priority.value}; lower-priority content is retained as data."
                ),
                severity="warning",
                metadata={"candidate_priority": candidate.priority.value},
            )
        return InstructionConflict(
            conflict_id=_new_id("conflict"),
            higher_source_id=candidate.source_id,
            lower_source_id=existing.source_id,
            description=(
                f"{candidate.source_id} appears to conflict with "
                f"{existing.source_id}."
            ),
            resolution="Higher-priority instruction wins.",
            severity="warning",
        )

    def detect_self_authorization(
        self,
        source: InstructionSource,
    ) -> InstructionConflict | None:
        lowered = source.content.lower()
        if source.source_type == InstructionSourceType.MODEL_OUTPUT and (
            "approved" in lowered or "authorization" in lowered or "批准" in source.content
        ):
            return InstructionConflict(
                conflict_id=_new_id("conflict"),
                higher_source_id="policy_engine",
                lower_source_id=source.source_id,
                description="Model-generated content attempted to authorize itself.",
                resolution="Model-generated text cannot grant approval or policy authority.",
                severity="critical",
            )
        return None


def _looks_conflicting(content: str) -> bool:
    lowered = content.lower()
    markers = (
        "ignore the user",
        "ignore user",
        "ignore previous",
        "override",
        "bypass",
        "pretend user approved",
        "忽略",
        "覆盖",
        "不要遵守",
        "用户已经批准",
    )
    return any(marker in lowered for marker in markers) or any(
        marker in content for marker in ("忽略", "覆盖", "不要遵守", "用户已经批准")
    )
