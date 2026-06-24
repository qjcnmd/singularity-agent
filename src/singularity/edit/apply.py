from __future__ import annotations

from typing import Any


class EditApplier:
    def __init__(self, mutation_manager: Any) -> None:
        self.mutation_manager = mutation_manager

    def preview(self, operations: list[Any], *, intent: str, tool_call_id: str | None = None) -> Any:
        return self.mutation_manager.preview_operations(
            operations,
            intent=intent,
            created_by="edit_executor",
            tool_call_id=tool_call_id,
        )

    def apply(self, operations: list[Any], *, intent: str, tool_call_id: str | None = None) -> Any:
        return self.mutation_manager.apply_operations(
            operations,
            intent=intent,
            created_by="edit_executor",
            tool_call_id=tool_call_id,
        )
