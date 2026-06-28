from __future__ import annotations

from dataclasses import replace
from typing import Any

from singularity.edit.models import (
    EditFailureCategory,
    EditIntent,
    EditRepairAttempt,
)

RECOVERABLE_CATEGORIES = {
    EditFailureCategory.FRESHNESS,
    EditFailureCategory.CONTEXT_MISMATCH,
}


class EditRepairController:
    def __init__(self, mutation_manager: Any) -> None:
        self.mutation_manager = mutation_manager

    def can_repair(self, category: EditFailureCategory | str) -> bool:
        return EditFailureCategory(category) in RECOVERABLE_CATEGORIES

    def repair_intent(
        self,
        intent: EditIntent,
        *,
        category: EditFailureCategory | str,
        attempt_number: int,
    ) -> tuple[EditIntent | None, EditRepairAttempt]:
        category = EditFailureCategory(category)
        if not self.can_repair(category):
            return None, EditRepairAttempt(
                attempt=attempt_number,
                category=category,
                action="not_recoverable",
                status="skipped",
                message=f"{category.value} is not automatically repairable.",
            )
        if category == EditFailureCategory.FRESHNESS:
            repaired = self._refresh_hashes(intent)
            return repaired, EditRepairAttempt(
                attempt=attempt_number,
                category=category,
                action="refresh_expected_hashes",
                status="candidate",
                message="Refreshed expected hashes from current workspace snapshots.",
            )
        if category == EditFailureCategory.CONTEXT_MISMATCH:
            repaired = self._fallback_context(intent)
            if repaired is None:
                return None, EditRepairAttempt(
                    attempt=attempt_number,
                    category=category,
                    action="safe_strategy_fallback",
                    status="failed",
                    message="No safe line-range fallback was present in the edit intent.",
                )
            return repaired, EditRepairAttempt(
                attempt=attempt_number,
                category=category,
                action="safe_strategy_fallback",
                status="candidate",
                message="Retried with available line-range information.",
            )
        return None, EditRepairAttempt(
            attempt=attempt_number,
            category=category,
            action="not_recoverable",
            status="skipped",
        )

    def _refresh_hashes(self, intent: EditIntent) -> EditIntent:
        scope = replace(intent.scope, expected_hashes=dict(intent.scope.expected_hashes))
        operations = []
        for operation in intent.operations:
            current = self.mutation_manager.index.current_hash(operation.path)
            if current:
                operation = replace(operation, expected_sha256=current)
                scope.expected_hashes[operation.path] = current
            operations.append(operation)
        return replace(intent, operations=operations, scope=scope, metadata={**intent.metadata, "repair": "freshness"})

    @staticmethod
    def _fallback_context(intent: EditIntent) -> EditIntent | None:
        repaired_operations = []
        changed = False
        for operation in intent.operations:
            if (
                operation.kind.value == "replace_text"
                and operation.start_line is not None
                and operation.end_line is not None
            ):
                operation = replace(operation, kind="replace_range")
                changed = True
            repaired_operations.append(operation)
        if not changed:
            return None
        return replace(intent, operations=repaired_operations, metadata={**intent.metadata, "repair": "context_fallback"})
