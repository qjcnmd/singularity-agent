from __future__ import annotations

from miniharness.planner.models import ExecutionBudget


class BudgetController:
    def __init__(self, budget: ExecutionBudget) -> None:
        self.budget = budget

    def record_tool_call(self) -> None:
        self.budget.tool_calls += 1

    def record_model_turn(self) -> None:
        self.budget.model_turns += 1

    def record_command(self) -> None:
        self.budget.command_runs += 1

    def record_mutation(self, *, changed_files: int = 0) -> None:
        self.budget.mutation_transactions += 1
        self.budget.changed_files = max(self.budget.changed_files, changed_files)

    def record_repair(self) -> None:
        self.budget.repair_iterations += 1

    def record_failure(self, fingerprint: str) -> int:
        current = self.budget.repeated_failures.get(fingerprint, 0) + 1
        self.budget.repeated_failures[fingerprint] = current
        return current

    def exceeded(self) -> str | None:
        if self.budget.model_turns > self.budget.max_model_turns:
            return "max_model_turns"
        if self.budget.tool_calls > self.budget.max_tool_calls:
            return "max_tool_calls"
        if self.budget.command_runs > self.budget.max_command_runs:
            return "max_command_runs"
        if self.budget.mutation_transactions > self.budget.max_mutation_transactions:
            return "max_mutation_transactions"
        if self.budget.repair_iterations > self.budget.max_repair_iterations:
            return "repair_budget_exceeded"
        if self.budget.changed_files > self.budget.max_changed_files:
            return "max_changed_files"
        if self.budget.context_growth > self.budget.max_context_growth:
            return "max_context_growth"
        for count in self.budget.repeated_failures.values():
            if count > self.budget.max_repeated_failures:
                return "repeated_failure"
        return None
