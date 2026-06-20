from __future__ import annotations

from pathlib import Path
from typing import Any

from miniharness.observability.models import TraceEventType, TraceSeverity
from miniharness.policy.audit import PolicyAuditWriter
from miniharness.policy.audit import redact_resource_identifier
from miniharness.policy.config import ApprovalMode, PolicyConfig
from miniharness.policy.models import (
    ApprovalGrant,
    DecisionOutcome,
    PolicyDecision,
    PolicyRequest,
)
from miniharness.policy.risk import RiskClassifier
from miniharness.policy.rules import DefaultLocalPolicyRules


class PolicyRuntime:
    def __init__(
        self,
        config: PolicyConfig | None = None,
        *,
        rules: DefaultLocalPolicyRules | None = None,
        audit_writer: PolicyAuditWriter | None = None,
        trace: Any | None = None,
    ) -> None:
        self.config = config or PolicyConfig()
        self.rules = rules or DefaultLocalPolicyRules()
        self.audit = audit_writer or PolicyAuditWriter(self.config)
        self.classifier = RiskClassifier(self.config.workspace_root)
        self._grants: list[ApprovalGrant] = []
        self.trace = trace

    def evaluate(self, request: PolicyRequest) -> PolicyDecision:
        self._emit_policy_trace(TraceEventType.POLICY_REQUESTED, request=request)
        risk = self.classifier.classify(request)
        decision = self.rules.decide(request, risk=risk, config=self.config)
        if decision.outcome == DecisionOutcome.REQUIRE_REVIEW:
            grant = self.find_matching_grant(request)
            if grant is not None:
                decision = decision.model_copy_with(
                    outcome=DecisionOutcome.ALLOW,
                    reason="Action allowed by matching ApprovalGrant.",
                    approval_grant_id=grant.grant_id,
                    required_approval=None,
                )
        self.audit.append(request=request, decision=decision)
        self._emit_policy_trace(
            TraceEventType.POLICY_DECIDED
            if decision.outcome == DecisionOutcome.ALLOW
            else TraceEventType.POLICY_BLOCKED,
            request=request,
            decision=decision,
        )
        return decision

    def enforce(self, request: PolicyRequest) -> PolicyDecision:
        decision = self.evaluate(request)
        if (
            decision.outcome == DecisionOutcome.REQUIRE_REVIEW
            and self.config.approval_mode == ApprovalMode.NON_INTERACTIVE
        ):
            decision = decision.model_copy_with(
                outcome=DecisionOutcome.DENY,
                reason="Review required but approval mode is non_interactive.",
            )
            self.audit.append(request=request, decision=decision)
            self._emit_policy_trace(
                TraceEventType.POLICY_BLOCKED,
                request=request,
                decision=decision,
            )
            return decision
        if decision.outcome == DecisionOutcome.ALLOW and decision.approval_grant_id:
            grant = next(
                (
                    candidate
                    for candidate in self._grants
                    if candidate.grant_id == decision.approval_grant_id
                ),
                None,
            )
            if grant is not None:
                grant.consume()
                self.audit.append(
                    request=request,
                    decision=decision,
                    grant=grant,
                    user_decision="approved",
                )
                self._emit_policy_trace(
                    TraceEventType.APPROVAL_GRANTED,
                    request=request,
                    decision=decision,
                    approval_grant_id=grant.grant_id,
                )
        return decision

    def consume_grant(
        self,
        request: PolicyRequest,
        decision: PolicyDecision,
        grant: ApprovalGrant,
    ) -> PolicyDecision:
        allowed = decision.model_copy_with(
            outcome=DecisionOutcome.ALLOW,
            reason="Action allowed by matching ApprovalGrant.",
            approval_grant_id=grant.grant_id,
            required_approval=None,
        )
        grant.consume()
        self.audit.append(
            request=request,
            decision=allowed,
            grant=grant,
            user_decision="approved",
        )
        self._emit_policy_trace(
            TraceEventType.APPROVAL_GRANTED,
            request=request,
            decision=allowed,
            approval_grant_id=grant.grant_id,
        )
        return allowed

    def register_grant(self, grant: ApprovalGrant) -> None:
        self._grants.append(grant)

    def find_matching_grant(self, request: PolicyRequest) -> ApprovalGrant | None:
        workspace_root = request.workspace_root or self.config.workspace_root
        for grant in self._grants:
            if grant.matches(request, workspace_root=Path(workspace_root)):
                return grant
        return None

    def _emit_policy_trace(
        self,
        event_type: TraceEventType,
        *,
        request: PolicyRequest,
        decision: PolicyDecision | None = None,
        approval_grant_id: str | None = None,
    ) -> None:
        if self.trace is None or not hasattr(self.trace, "emit"):
            return
        blocked = decision is not None and decision.outcome != DecisionOutcome.ALLOW
        self.trace.emit(
            event_type,
            runtime="policy" if not event_type.value.startswith("approval.") else "approval",
            summary=(
                decision.reason
                if decision is not None
                else f"Policy requested for {request.operation.value}."
            ),
            payload={
                "request_id": request.request_id,
                "decision_id": decision.decision_id if decision else None,
                "operation": request.operation.value,
                "capability": request.capability.value,
                "resource": redact_resource_identifier(request.resource.identifier),
                "outcome": decision.outcome.value if decision else None,
                "risk_level": decision.risk_level.value if decision else None,
                "rule_ids": decision.rule_ids if decision else [],
                "approval_required": bool(
                    decision.required_approval if decision else False
                ),
            },
            ids={
                "session_id": request.session_id,
                "task_id": request.task_id,
                "phase_id": request.phase_id,
                "action_id": request.action_id,
                "policy_decision_id": decision.decision_id if decision else None,
                "approval_grant_id": approval_grant_id,
            },
            severity=TraceSeverity.WARNING if blocked else TraceSeverity.INFO,
        )
