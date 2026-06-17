from __future__ import annotations

import json
from typing import Any

from miniharness.observability.models import TraceEventType, TraceSeverity
from miniharness.policy.audit import redact_resource_identifier
from miniharness.policy.config import ApprovalMode, PolicyConfig
from miniharness.policy.exceptions import (
    ApprovalDenied,
    ApprovalRequired,
    PolicyAskUserRequired,
    PolicyDenied,
    PolicyEscalationRequired,
    SandboxRequired,
)
from miniharness.policy.models import (
    ApprovalGrant,
    DecisionOutcome,
    PolicyDecision,
    PolicyRequest,
    approval_scope_for_request,
)


class ApprovalGate:
    def __init__(self, config: PolicyConfig, *, trace: Any | None = None) -> None:
        self.config = config
        self.trace = trace

    def resolve(
        self,
        request: PolicyRequest,
        decision: PolicyDecision,
    ) -> ApprovalGrant | None:
        if decision.outcome == DecisionOutcome.ALLOW:
            return None
        self._emit(
            TraceEventType.APPROVAL_REQUESTED
            if decision.outcome == DecisionOutcome.REQUIRE_REVIEW
            else TraceEventType.APPROVAL_DENIED,
            request,
            decision,
            summary=decision.reason,
            severity=TraceSeverity.WARNING,
        )
        if decision.outcome == DecisionOutcome.DENY:
            raise PolicyDenied(decision.reason)
        if decision.outcome == DecisionOutcome.SANDBOX_REQUIRED:
            raise SandboxRequired(decision.reason)
        if decision.outcome == DecisionOutcome.ASK_USER:
            raise PolicyAskUserRequired(decision.reason)
        if decision.outcome == DecisionOutcome.ESCALATE:
            raise PolicyEscalationRequired(decision.reason)
        if decision.outcome != DecisionOutcome.REQUIRE_REVIEW:
            raise PolicyDenied(decision.reason)
        if self.config.approval_mode == ApprovalMode.NON_INTERACTIVE:
            self._emit(
                TraceEventType.APPROVAL_DENIED,
                request,
                decision,
                summary="Review required but approval mode is non_interactive.",
                severity=TraceSeverity.WARNING,
            )
            raise ApprovalRequired(decision.reason)

        while True:
            self._print_review(request, decision)
            answer = input("[a]pprove once, [d]eny, [v]iew details: ").strip().lower()
            if answer in {"a", "approve", "approve once"}:
                requirement = decision.required_approval
                grant = ApprovalGrant(
                    decision_id=decision.decision_id,
                    request_id=request.request_id,
                    approved_by="local-cli-user",
                    scope=requirement.scope if requirement else approval_scope_for_request(request),
                    single_use=True,
                    reason="approved once via local CLI",
                )
                self._emit(
                    TraceEventType.APPROVAL_GRANTED,
                    request,
                    decision,
                    summary=grant.reason,
                    approval_grant_id=grant.grant_id,
                )
                return grant
            if answer in {"d", "deny", "n", "no"}:
                self._emit(
                    TraceEventType.APPROVAL_DENIED,
                    request,
                    decision,
                    summary=decision.reason,
                    severity=TraceSeverity.WARNING,
                )
                raise ApprovalDenied(decision.reason)
            if answer in {"v", "view", "details"}:
                print(json.dumps(decision.to_dict(), ensure_ascii=False, indent=2))

    @staticmethod
    def _print_review(request: PolicyRequest, decision: PolicyDecision) -> None:
        details: dict[str, Any] = {
            "action": request.reason,
            "operation": request.operation.value,
            "resource": request.resource.identifier,
            "risk_level": decision.risk_level.value,
            "risk_tags": [str(tag.value if hasattr(tag, "value") else tag) for tag in decision.risk_tags],
            "reason": decision.reason,
            "constraints": decision.constraints.to_dict(),
            "rollback_available": request.reversible,
        }
        if request.resource.resource_type == "command":
            details["command_preview"] = request.resource.identifier
        if request.metadata.get("diff_summary"):
            details["diff_summary"] = request.metadata["diff_summary"]
        print(json.dumps(details, ensure_ascii=False, indent=2))

    def _emit(
        self,
        event_type: TraceEventType,
        request: PolicyRequest,
        decision: PolicyDecision,
        *,
        summary: str,
        approval_grant_id: str | None = None,
        severity: TraceSeverity = TraceSeverity.INFO,
    ) -> None:
        if self.trace is None or not hasattr(self.trace, "emit"):
            return
        self.trace.emit(
            event_type,
            runtime="approval",
            summary=summary,
            payload={
                "request_id": request.request_id,
                "decision_id": decision.decision_id,
                "operation": request.operation.value,
                "resource": redact_resource_identifier(request.resource.identifier),
                "outcome": decision.outcome.value,
                "approval_grant_id": approval_grant_id,
            },
            ids={
                "session_id": request.session_id,
                "task_id": request.task_id,
                "phase_id": request.phase_id,
                "action_id": request.action_id,
                "policy_decision_id": decision.decision_id,
                "approval_grant_id": approval_grant_id,
            },
            severity=severity,
        )
