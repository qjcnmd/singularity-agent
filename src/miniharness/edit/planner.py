from __future__ import annotations

from miniharness.edit.models import EditIntent, EditPlan
from miniharness.edit.strategy import EditStrategySelector


class EditPlanBuilder:
    def __init__(self, selector: EditStrategySelector | None = None) -> None:
        self.selector = selector or EditStrategySelector()

    def build(self, intent: EditIntent) -> EditPlan:
        decision = self.selector.choose(intent)
        return EditPlan(
            intent_id=intent.id,
            strategy=decision.strategy,
            operations=list(intent.operations),
            rationale=decision.rationale,
            scope=intent.scope,
            metadata={"intent_summary": intent.summary, "actor": intent.actor},
        )
