from __future__ import annotations

import json
from typing import Any

from singularity.context import ContextManager
from singularity.error_codes import FAILURE_ANALYSIS_EXCLUDED_ERROR_CODES, ErrorCode
from singularity.execution_outcome import ExecutionOutcome, ExecutionOutcomeStatus
from singularity.failure_analysis.analyzer import FailureAnalyzer
from singularity.failure_analysis.request import FailureAnalysisRequest
from singularity.planner import Planner
from singularity.repair import RepairPlanner
from singularity.utils.attributes import nested_getattr


class FailureRecoveryCoordinator:
    def __init__(
        self,
        *,
        failure_analyzer: FailureAnalyzer | None = None,
        repair_planner: RepairPlanner | None = None,
        failure_analysis_fingerprints: set[str] | None = None,
        failure_replan_signals: dict[str, Any] | None = None,
        failure_analysis_snapshots: dict[str, dict[str, int]] | None = None,
        completion_rejection_state: dict[str, dict[str, Any]] | None = None,
    ) -> None:
        self.failure_analyzer = failure_analyzer
        self.repair_planner = repair_planner
        self.failure_analysis_fingerprints = (
            failure_analysis_fingerprints if failure_analysis_fingerprints is not None else set()
        )
        self.failure_replan_signals = (
            failure_replan_signals if failure_replan_signals is not None else {}
        )
        self.failure_analysis_snapshots = (
            failure_analysis_snapshots if failure_analysis_snapshots is not None else {}
        )
        self.completion_rejection_state = (
            completion_rejection_state if completion_rejection_state is not None else {}
        )

    def maybe_analyze_failure(
        self,
        planner: Planner,
        context: ContextManager,
        *,
        failure_source: str,
        turn: int,
        outcome: ExecutionOutcome | None = None,
    ) -> ExecutionOutcome | None:
        if outcome is not None and not self.should_analyze_outcome(planner, outcome):
            return None
        if outcome is None and not self.has_repairable_planner_failure(planner):
            return None
        if self.failure_analyzer is None or self.repair_planner is None:
            raise RuntimeError("failure recovery requires analyzer and repair planner")
        request = FailureAnalysisRequest.from_planner(
            planner,
            context,
            failure_source=failure_source,
            outcome=outcome,
            turn=turn,
        )
        if not request.has_failure:
            return None
        snapshot = self.failure_snapshot(planner)
        if request.fingerprint in self.failure_analysis_fingerprints:
            if not self.duplicate_failure_has_new_evidence(request.fingerprint, snapshot):
                if self.is_stalled_completion_gate_failure(
                    failure_source=failure_source,
                    outcome=outcome,
                ):
                    signal_payload = self.failure_replan_signals.get(request.fingerprint)
                    return ExecutionOutcome(
                        status=ExecutionOutcomeStatus.USER_INPUT_REQUIRED,
                        source="failure_analysis",
                        reason="Repeated completion/final review failure without new repair evidence.",
                        error_code=ErrorCode.REPAIR_BUDGET_EXCEEDED.value,
                        next_action="ask_user",
                        observation_summary=(
                            "Completion gate is still blocked after failure analysis and no new repair evidence."
                        ),
                        retry_allowed=False,
                        metadata={"replan_signal": signal_payload or {}},
                    )
                return None
            signal_payload = self.failure_replan_signals.get(request.fingerprint)
            if signal_payload is None:
                return None
            self.failure_analysis_snapshots[request.fingerprint] = snapshot
            decision = planner.replan(signal_payload)
            if nested_getattr(decision, "decision.value", default="") == "ask_user":
                return ExecutionOutcome(
                    status=ExecutionOutcomeStatus.USER_INPUT_REQUIRED,
                    source="failure_analysis",
                    reason=decision.reason,
                    error_code=ErrorCode.REPAIR_BUDGET_EXCEEDED.value,
                    next_action="ask_user",
                    observation_summary=decision.reason,
                    retry_allowed=False,
                    metadata={"replan_signal": signal_payload},
                )
            return None
        self.failure_analysis_fingerprints.add(request.fingerprint)
        analysis = self.failure_analyzer.analyze(request)
        repair_plan = self.repair_planner.plan(analysis, repair_policy=request.repair_policy)
        replan_signal = self.repair_planner.to_replan_signal(
            request=request,
            analysis=analysis,
            plan=repair_plan,
        )
        replan_signal_payload = replan_signal.to_dict()
        planner.record_failure_analysis(
            analysis,
            repair_plan,
            replan_signal=replan_signal_payload,
        )
        context.add_failure(
            {
                "failure_analysis": analysis.to_dict(),
                "repair_plan": repair_plan.to_dict(),
                "replan_signal": replan_signal_payload,
            }
        )
        self.failure_analysis_snapshots[request.fingerprint] = snapshot
        if repair_plan.needs_user_input or repair_plan.blocked_reason:
            return self.repair_planner.blocked_outcome(repair_plan)
        self.failure_replan_signals[request.fingerprint] = replan_signal_payload
        decision = planner.replan(replan_signal_payload)
        if nested_getattr(decision, "decision.value", default="") == "ask_user":
            return ExecutionOutcome(
                status=ExecutionOutcomeStatus.USER_INPUT_REQUIRED,
                source="failure_analysis",
                reason=decision.reason,
                error_code=ErrorCode.REPAIR_BUDGET_EXCEEDED.value,
                next_action="ask_user",
                observation_summary=decision.reason,
                retry_allowed=False,
                metadata={"repair_plan": repair_plan.to_dict(), "replan_signal": replan_signal_payload},
            )
        return None

    def should_analyze_outcome(self, planner: Planner, outcome: ExecutionOutcome) -> bool:
        if outcome.status != ExecutionOutcomeStatus.REPLAN_REQUIRED:
            return False
        if outcome.error_code in FAILURE_ANALYSIS_EXCLUDED_ERROR_CODES:
            return False
        if outcome.error_code == ErrorCode.COMPLETION_REJECTED.value:
            return self.should_escalate_completion_rejection(planner, outcome)
        return True

    def should_escalate_completion_rejection(
        self,
        planner: Planner,
        outcome: ExecutionOutcome,
    ) -> bool:
        missing = sorted(str(item) for item in outcome.missing_evidence)
        key = json.dumps({"missing": missing}, ensure_ascii=False, sort_keys=True)
        phase = nested_getattr(planner, "state.current_phase", default="")
        snapshot = self.evidence_snapshot(planner)
        previous = self.completion_rejection_state.get("latest")
        if not previous or previous.get("key") != key:
            self.completion_rejection_state["latest"] = {
                "key": key,
                "count": 1,
                "phase": phase,
                "snapshot": snapshot,
            }
            return False
        count = int(previous.get("count") or 0) + 1
        phase_stalled = previous.get("phase") == phase
        evidence_stalled = previous.get("snapshot") == snapshot
        self.completion_rejection_state["latest"] = {
            "key": key,
            "count": count,
            "phase": phase,
            "snapshot": snapshot,
        }
        return count >= 2 and phase_stalled and evidence_stalled

    @staticmethod
    def evidence_snapshot(planner: Planner) -> dict[str, int]:
        evidence = planner.evidence
        return {
            "inspected_files": len(evidence.inspected_files),
            "applied_changes": len(evidence.applied_changes),
            "command_results": len(evidence.command_results),
            "verification_results": len(evidence.verification_results),
            "tool_results": len(evidence.tool_results),
            "edit_results": len(evidence.edit_results),
            "review_results": len(evidence.review_results),
        }

    @staticmethod
    def failure_snapshot(planner: Planner) -> dict[str, int]:
        evidence = planner.evidence
        return {
            "failed_command_results": len(
                [
                    item
                    for item in evidence.command_results
                    if item.get("semantic_status") not in {None, "succeeded", "SUCCEEDED"}
                    or item.get("error_code")
                ]
            ),
            "failed_verification_results": len(
                [
                    item
                    for item in evidence.verification_results
                    if isinstance(item, dict)
                    and (
                        (item.get("completion_assessment") or {}).get("status")
                        in {"failed", "blocked", "needs_review"}
                        or any(
                            result.get("status") in {"failed", "blocked", "timeout", "flaky"}
                            for result in item.get("results") or []
                            if isinstance(result, dict)
                        )
                    )
                ]
            ),
            "failed_tool_results": len(
                [
                    item
                    for item in evidence.tool_results
                    if item.get("ok") is False or item.get("error_code")
                ]
            ),
            "failed_edit_results": len(
                [
                    item
                    for item in evidence.edit_results
                    if item.get("error_code") or item.get("status") in {"failed", "blocked"}
                ]
            ),
            "failed_review_results": len(
                [
                    item
                    for item in evidence.review_results
                    if isinstance(item.get("decision"), dict)
                    and item["decision"].get("action") in {
                        "repair",
                        "reject",
                        "needs_human_approval",
                    }
                ]
            ),
        }

    @staticmethod
    def is_stalled_completion_gate_failure(
        *,
        failure_source: str,
        outcome: ExecutionOutcome | None,
    ) -> bool:
        if failure_source in {"completion", "completion_review"}:
            return True
        return bool(
            outcome is not None
            and outcome.error_code
            in {
                ErrorCode.COMPLETION_REJECTED.value,
                ErrorCode.FINAL_REVIEW_REJECTED.value,
            }
        )

    def duplicate_failure_has_new_evidence(self, fingerprint: str, snapshot: dict[str, int]) -> bool:
        previous = self.failure_analysis_snapshots.get(fingerprint)
        if previous is None:
            return True
        return any(snapshot.get(key, 0) > previous.get(key, 0) for key in snapshot)

    @staticmethod
    def has_repairable_planner_failure(planner: Planner) -> bool:
        if planner.state is None:
            return False
        latest = planner.evidence.verification_results[-1] if planner.evidence.verification_results else {}
        assessment = latest.get("completion_assessment") if isinstance(latest, dict) else {}
        if isinstance(assessment, dict) and assessment.get("status") in {"ready", "ready_with_warnings"}:
            return False
        if isinstance(assessment, dict) and assessment.get("status") in {"failed", "blocked", "needs_review"}:
            return True
        for failure in planner.evidence.unresolved_failures[-5:]:
            if not isinstance(failure, dict):
                return True
            code = (
                failure.get("error_code")
                or (failure.get("execution_outcome") or {}).get("error_code")
                or failure.get("status")
            )
            if code not in FAILURE_ANALYSIS_EXCLUDED_ERROR_CODES:
                return True
        return False
