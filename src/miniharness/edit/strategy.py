from __future__ import annotations

from dataclasses import dataclass

from miniharness.edit.models import (
    EditIntent,
    EditOperationKind,
    EditStrategyKind,
)


@dataclass(frozen=True)
class StrategyDecision:
    strategy: EditStrategyKind
    rationale: list[str]

    def to_dict(self) -> dict[str, object]:
        return {"strategy": self.strategy.value, "rationale": self.rationale}


class EditStrategySelector:
    def choose(self, intent: EditIntent) -> StrategyDecision:
        if intent.strategy is not None:
            return StrategyDecision(
                strategy=EditStrategyKind(intent.strategy),
                rationale=["Intent provided an explicit edit strategy."],
            )

        kinds = {operation.kind for operation in intent.operations}
        if kinds & {
            EditOperationKind.UPDATE_JSON,
            EditOperationKind.REPLACE_SYMBOL,
            EditOperationKind.REPLACE_IMPORT,
        }:
            return StrategyDecision(
                strategy=EditStrategyKind.STRUCTURED_EDIT,
                rationale=[
                    "Structured operation requested.",
                    "Patch builder will lower JSON/Python operations into mutation operations.",
                ],
            )

        if kinds & {EditOperationKind.REWRITE_FILE, EditOperationKind.CREATE_FILE}:
            return StrategyDecision(
                strategy=EditStrategyKind.FULL_FILE_REWRITE,
                rationale=[
                    "Operation replaces or creates an entire file.",
                    "Rewrite strategy uses CreateFile or whole-file ReplaceText.",
                ],
            )

        return StrategyDecision(
            strategy=EditStrategyKind.TARGETED_PATCH,
            rationale=[
                "Operations are localized text/range/marker edits.",
                "Targeted patch minimizes changed context and enforces unique matches.",
            ],
        )
