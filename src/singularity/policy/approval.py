from __future__ import annotations

from typing import Any

from singularity.interaction import DecisionPrompt, InteractionController
from singularity.observability.models import TraceEventType, TraceSeverity
from singularity.policy.audit import redact_resource_identifier
from singularity.policy.config import ApprovalMode, PolicyConfig
from singularity.policy.exceptions import (
    ApprovalDenied,
    ApprovalRequired,
    PolicyAskUserRequired,
    PolicyDenied,
    PolicyEscalationRequired,
    SandboxRequired,
)
from singularity.policy.models import (
    ApprovalGrant,
    DecisionOutcome,
    PolicyDecision,
    PolicyRequest,
    approval_scope_for_request,
)


class ApprovalGate:
    def __init__(
        self,
        config: PolicyConfig,
        *,
        trace: Any | None = None,
        interaction: InteractionController | None = None,
    ) -> None:
        self.config = config
        self.trace = trace
        self.interaction = interaction

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
        if self.interaction is None:
            self._emit(
                TraceEventType.APPROVAL_DENIED,
                request,
                decision,
                summary=(
                    "Review required but approval mode is non_interactive."
                    if self.config.approval_mode == ApprovalMode.NON_INTERACTIVE
                    else "Review required but no InteractionController is configured."
                ),
                severity=TraceSeverity.WARNING,
            )
            raise ApprovalRequired(decision.reason)

        user_decision = self.interaction.request_decision(
            self._decision_prompt(request, decision)
        )
        answer = user_decision.decision.strip().lower()
        if answer in {"a", "approve", "approve once", "continue", "c"}:
            requirement = decision.required_approval
            grant = ApprovalGrant(
                decision_id=decision.decision_id,
                request_id=request.request_id,
                approved_by=user_decision.decided_by,
                session_id=request.session_id,
                scope=requirement.scope if requirement else approval_scope_for_request(request),
                single_use=True,
                reason=user_decision.reason or "approved once via InteractionController",
            )
            self._emit(
                TraceEventType.APPROVAL_GRANTED,
                request,
                decision,
                summary=grant.reason,
                approval_grant_id=grant.grant_id,
            )
            return grant
        if answer in {"revise", "v"}:
            self._emit(
                TraceEventType.APPROVAL_DENIED,
                request,
                decision,
                summary=user_decision.reason or "User requested goal revision.",
                severity=TraceSeverity.WARNING,
            )
            raise PolicyAskUserRequired(user_decision.reason or decision.reason)
        if user_decision.metadata.get("fail_closed"):
            self._emit(
                TraceEventType.APPROVAL_DENIED,
                request,
                decision,
                summary="Review required but InteractionController failed closed.",
                severity=TraceSeverity.WARNING,
            )
            raise ApprovalRequired(decision.reason)
        self._emit(
            TraceEventType.APPROVAL_DENIED,
            request,
            decision,
            summary=user_decision.reason or decision.reason,
            severity=TraceSeverity.WARNING,
        )
        raise ApprovalDenied(user_decision.reason or decision.reason)

    @staticmethod
    def _review_details(request: PolicyRequest, decision: PolicyDecision) -> dict[str, Any]:
        details: dict[str, Any] = {
            "action": request.reason,
            "operation": request.operation.value,
            "resource": redact_resource_identifier(request.resource.identifier),
            "risk_level": decision.risk_level.value,
            "risk_tags": [str(tag.value if hasattr(tag, "value") else tag) for tag in decision.risk_tags],
            "reason": decision.reason,
            "constraints": decision.constraints.to_dict(),
            "rollback_available": request.reversible,
        }
        if request.resource.resource_type == "command":
            details["command_preview"] = redact_resource_identifier(request.resource.identifier)
        if request.metadata.get("diff_summary"):
            details["diff_summary"] = request.metadata["diff_summary"]
        return details

    def _decision_prompt(self, request: PolicyRequest, decision: PolicyDecision) -> DecisionPrompt:
        details = self._review_details(request, decision)
        message = (
            decision.user_message
            or (decision.required_approval.message if decision.required_approval else "")
            or decision.reason
        )
        return DecisionPrompt(
            title="Approval required",
            message=message,
            choices=["approve", "reject", "revise", "continue", "abort"],
            recommended="reject",
            risk_level=decision.risk_level.value,
            metadata={
                "request": {
                    "request_id": request.request_id,
                    "session_id": request.session_id,
                    "task_id": request.task_id,
                    "phase_id": request.phase_id,
                    "action_id": request.action_id,
                },
                "decision": decision.to_dict(),
                "review": details,
            },
        )

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
            component="approval",
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
