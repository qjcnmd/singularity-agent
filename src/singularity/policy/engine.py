from __future__ import annotations

from typing import Any

from singularity.observability.models import TraceEventType, TraceSeverity
from singularity.policy.audit import PolicyAuditWriter, redact_resource_identifier
from singularity.policy.config import PolicyConfig
from singularity.policy.models import (
    DecisionOutcome,
    PolicyDecision,
    PolicyRequest,
)
from singularity.policy.risk import RiskClassifier
from singularity.policy.rules import DefaultLocalPolicyRules


class PolicyEngine:
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
        self.trace = trace

    def evaluate(self, request: PolicyRequest) -> PolicyDecision:
        return self._decide(request)

    def enforce(self, request: PolicyRequest) -> PolicyDecision:
        self._emit_policy_trace(TraceEventType.POLICY_REQUESTED, request=request)
        decision = self._decide(request)
        self.audit.append(request=request, decision=decision)
        self._emit_policy_trace(
            TraceEventType.POLICY_DECIDED
            if decision.outcome == DecisionOutcome.ALLOW
            else TraceEventType.POLICY_BLOCKED,
            request=request,
            decision=decision,
        )
        return decision

    def _decide(self, request: PolicyRequest) -> PolicyDecision:
        risk = self.classifier.classify(request)
        return self.rules.decide(request, risk=risk, config=self.config)

    def _emit_policy_trace(
        self,
        event_type: TraceEventType,
        *,
        request: PolicyRequest,
        decision: PolicyDecision | None = None,
    ) -> None:
        if self.trace is None or not hasattr(self.trace, "emit"):
            return
        blocked = decision is not None and decision.outcome != DecisionOutcome.ALLOW
        self.trace.emit(
            event_type,
            component="policy",
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
                "permission_profile": (
                    self.config.permission_profile.profile.value
                    if self.config.permission_profile is not None
                    else None
                ),
            },
            ids={
                "session_id": request.session_id,
                "task_id": request.task_id,
                "phase_id": request.phase_id,
                "action_id": request.action_id,
                "policy_decision_id": decision.decision_id if decision else None,
            },
            severity=TraceSeverity.WARNING if blocked else TraceSeverity.INFO,
        )
