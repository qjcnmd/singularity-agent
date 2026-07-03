from __future__ import annotations

from collections.abc import Callable
from typing import Any

from singularity.context import ContextManager
from singularity.error_codes import ErrorCode
from singularity.execution_outcome import ExecutionOutcome, ExecutionOutcomeStatus
from singularity.planner import Planner, TaskStatus
from singularity.run_controller import RunController


class CompletionGate:
    def __init__(
        self,
        *,
        trace: Any,
        result_factory: Callable[..., Any],
        terminal_result_from_outcome: Callable[..., Any],
        record_outcome_context: Callable[[ContextManager, Planner, ExecutionOutcome], None],
        maybe_analyze_failure: Callable[..., ExecutionOutcome | None],
    ) -> None:
        self.trace = trace
        self.result_factory = result_factory
        self.terminal_result_from_outcome = terminal_result_from_outcome
        self.record_outcome_context = record_outcome_context
        self.maybe_analyze_failure = maybe_analyze_failure

    def attempt_finalize(
        self,
        planner: Planner,
        *,
        controller: RunController,
        context: ContextManager,
        turn: int,
        model_answer: str,
    ) -> Any | None:
        assessment = planner.assess_completion(mark_blocked=False)
        if assessment["status"] != TaskStatus.COMPLETED.value:
            outcome = ExecutionOutcome(
                status=ExecutionOutcomeStatus.REPLAN_REQUIRED,
                source="completion",
                reason="completion_rejected",
                error_code=ErrorCode.COMPLETION_REJECTED.value,
                missing_evidence=list(assessment["unmet"]),
                next_action="continue",
                observation_summary=(
                    "Completion rejected because evidence is missing: "
                    + ", ".join(assessment["unmet"])
                ),
                retry_allowed=True,
                metadata={"assessment": assessment},
            )
            controller.apply_outcome(outcome)
            self.record_outcome_context(context, planner, outcome)
            blocked = self.maybe_analyze_failure(
                planner,
                context,
                outcome=outcome,
                failure_source="completion",
                turn=turn,
            )
            if blocked is not None:
                controller.apply_outcome(blocked)
                self.record_outcome_context(context, planner, blocked)
                return self.terminal_result_from_outcome(blocked, turn=turn)
            repair_blocked = self.repair_phase_completion_blocked_outcome(
                planner,
                assessment=assessment,
            )
            if repair_blocked is not None:
                controller.apply_outcome(repair_blocked)
                self.record_outcome_context(context, planner, repair_blocked)
                return self.terminal_result_from_outcome(repair_blocked, turn=turn)
            return None
        if (
            planner.state is not None
            and not planner.state.completion_criteria.required_changes_applied
            and not planner.state.completion_criteria.required_verifications_passed
        ):
            outcome = ExecutionOutcome(
                status=ExecutionOutcomeStatus.SUCCESS,
                source="completion",
                reason="completion_ready",
                next_action="finalize",
                observation_summary="Completion evidence satisfied.",
                retry_allowed=False,
            )
            controller.apply_outcome(outcome)
            self.record_outcome_context(context, planner, outcome)
            self.trace.record("final_answer", {"turn": turn, "content": model_answer})
            return self.result_factory(
                status="completed",
                final_answer=model_answer,
                turn=turn,
            )
        report = planner.finalize()
        report.context_usage_diagnostic = context.context_usage_diagnostic()
        if report.status != TaskStatus.COMPLETED:
            retry_allowed = report.status in {
                TaskStatus.INSPECTING_WORKSPACE,
                TaskStatus.PLANNING_CHANGES,
                TaskStatus.APPLYING_CHANGES,
                TaskStatus.RUNNING_VERIFICATION,
                TaskStatus.REPAIRING_FAILURES,
                TaskStatus.FINALIZING,
            }
            outcome = ExecutionOutcome(
                status=(
                    ExecutionOutcomeStatus.REPLAN_REQUIRED
                    if retry_allowed
                    else ExecutionOutcomeStatus.BLOCKED
                ),
                source="completion",
                reason=f"Final report did not complete: {report.status.value}.",
                error_code=ErrorCode.FINAL_REVIEW_REJECTED.value,
                missing_evidence=list(report.next_steps or ["final_report_completed"]),
                next_action="continue" if retry_allowed else "blocked",
                observation_summary=(
                    f"Final report status={report.status.value}; "
                    f"verification={report.verification_summary.get('status', 'unknown')}."
                ),
                retry_allowed=retry_allowed,
                metadata={
                    "final_report_status": report.status.value,
                    "verification_summary": report.verification_summary,
                    "review_summary": report.review_summary,
                },
            )
            controller.apply_outcome(outcome)
            self.record_outcome_context(context, planner, outcome)
            blocked = self.maybe_analyze_failure(
                planner,
                context,
                outcome=outcome,
                failure_source="completion_review",
                turn=turn,
            )
            if blocked is not None:
                controller.apply_outcome(blocked)
                self.record_outcome_context(context, planner, blocked)
                return self.terminal_result_from_outcome(blocked, turn=turn)
            return self.terminal_result_from_outcome(outcome, turn=turn)
        final_answer = "\n".join(
            [
                f"status: {report.status.value}",
                f"files_changed: {', '.join(report.files_changed) if report.files_changed else '-'}",
                f"verification: {report.verification_summary.get('status', 'unknown')}",
                f"unresolved_issues: {len(report.unresolved_issues)}",
                f"risks: {len(report.risks)}",
            ]
        )
        outcome = ExecutionOutcome(
            status=ExecutionOutcomeStatus.SUCCESS,
            source="completion",
            reason="completion_ready",
            next_action="finalize",
            observation_summary="Completion evidence satisfied.",
            retry_allowed=False,
            metadata={"verification_summary": report.verification_summary},
        )
        controller.apply_outcome(outcome)
        self.record_outcome_context(context, planner, outcome)
        self.trace.record("final_answer", {"turn": turn, "content": final_answer})
        return self.result_factory(
            status="completed",
            final_answer=final_answer,
            turn=turn,
        )

    @staticmethod
    def should_auto_finalize_after_tools(
        planner: Planner,
        protocol_result: Any,
    ) -> bool:
        state = planner.state
        if state is None:
            return False
        if state.status != TaskStatus.FINALIZING and state.current_phase != TaskStatus.FINALIZING.value:
            return False
        if int(getattr(protocol_result, "pending_approval_count", 0) or 0):
            return False
        if int(getattr(protocol_result, "failed_count", 0) or 0):
            return False
        return not int(getattr(protocol_result, "rejected_count", 0) or 0)

    @staticmethod
    def repair_phase_completion_blocked_outcome(
        planner: Planner,
        *,
        assessment: dict[str, Any],
    ) -> ExecutionOutcome | None:
        state = getattr(planner, "state", None)
        if getattr(state, "current_phase", "") != "repairing_failures":
            return None
        unmet = [str(item) for item in assessment.get("unmet") or []]
        if "verification_contract_satisfaction" not in unmet:
            return None
        return ExecutionOutcome(
            status=ExecutionOutcomeStatus.BLOCKED,
            source="completion",
            reason="Repair phase completion rejected because the active repair contract is unsatisfied.",
            error_code=ErrorCode.REPAIR_BUDGET_EXCEEDED.value,
            missing_evidence=unmet,
            next_action="blocked",
            observation_summary=(
                "Repair phase cannot complete because the active repair verification contract is unsatisfied."
            ),
            retry_allowed=False,
            metadata={"assessment": assessment},
        )
