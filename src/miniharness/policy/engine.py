from __future__ import annotations

from pathlib import Path

from miniharness.policy.audit import PolicyAuditWriter
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
    ) -> None:
        self.config = config or PolicyConfig()
        self.rules = rules or DefaultLocalPolicyRules()
        self.audit = audit_writer or PolicyAuditWriter(self.config)
        self.classifier = RiskClassifier(self.config.workspace_root)
        self._grants: list[ApprovalGrant] = []

    def evaluate(self, request: PolicyRequest) -> PolicyDecision:
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
        return decision

    def register_grant(self, grant: ApprovalGrant) -> None:
        self._grants.append(grant)

    def find_matching_grant(self, request: PolicyRequest) -> ApprovalGrant | None:
        workspace_root = request.workspace_root or self.config.workspace_root
        for grant in self._grants:
            if grant.matches(request, workspace_root=Path(workspace_root)):
                return grant
        return None
