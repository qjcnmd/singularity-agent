from __future__ import annotations

import json
import os
from contextlib import contextmanager
from pathlib import Path
from typing import Any

from singularity.observability.models import TraceEventType, TraceSeverity
from singularity.policy.audit import PolicyAuditWriter
from singularity.policy.audit import redact_resource_identifier
from singularity.policy.config import ApprovalMode, PolicyConfig
from singularity.policy.models import (
    ApprovalGrant,
    DecisionOutcome,
    PolicyDecision,
    PolicyRequest,
)
from singularity.policy.risk import RiskClassifier
from singularity.policy.rules import DefaultLocalPolicyRules


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
        self._grants_lock_path = Path(self.config.approval_grants_path).with_suffix(".lock")
        self._grants: list[ApprovalGrant] = self._load_grants()
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
        self._emit_policy_trace(TraceEventType.POLICY_REQUESTED, request=request)
        risk = self.classifier.classify(request)
        decision = self.rules.decide(request, risk=risk, config=self.config)
        if decision.outcome == DecisionOutcome.REQUIRE_REVIEW:
            grant = self._consume_matching_grant(request)
            if grant is not None:
                decision = decision.model_copy_with(
                    outcome=DecisionOutcome.ALLOW,
                    reason="Action allowed by matching ApprovalGrant.",
                    approval_grant_id=grant.grant_id,
                    required_approval=None,
                )
                self.audit.append(request=request, decision=decision)
                self.audit.append(
                    request=request,
                    decision=decision,
                    grant=grant,
                    user_decision="approved",
                )
                self._emit_policy_trace(
                    TraceEventType.POLICY_DECIDED,
                    request=request,
                    decision=decision,
                )
                self._emit_policy_trace(
                    TraceEventType.APPROVAL_GRANTED,
                    request=request,
                    decision=decision,
                    approval_grant_id=grant.grant_id,
                )
                return decision
        self.audit.append(request=request, decision=decision)
        self._emit_policy_trace(
            TraceEventType.POLICY_DECIDED
            if decision.outcome == DecisionOutcome.ALLOW
            else TraceEventType.POLICY_BLOCKED,
            request=request,
            decision=decision,
        )
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
        return decision

    def consume_grant(
        self,
        request: PolicyRequest,
        decision: PolicyDecision,
        grant: ApprovalGrant,
    ) -> PolicyDecision:
        consumed = self._consume_grant_object(grant)
        if consumed is None:
            return decision
        allowed = decision.model_copy_with(
            outcome=DecisionOutcome.ALLOW,
            reason="Action allowed by matching ApprovalGrant.",
            approval_grant_id=consumed.grant_id,
            required_approval=None,
        )
        self.audit.append(
            request=request,
            decision=allowed,
            grant=consumed,
            user_decision="approved",
        )
        self._emit_policy_trace(
            TraceEventType.APPROVAL_GRANTED,
            request=request,
            decision=allowed,
            approval_grant_id=consumed.grant_id,
        )
        return allowed

    def register_grant(self, grant: ApprovalGrant) -> None:
        with _file_lock(self._grants_lock_path):
            self._grants = self._load_grants_unlocked()
            existing = next(
                (index for index, candidate in enumerate(self._grants) if candidate.grant_id == grant.grant_id),
                None,
            )
            if existing is None:
                self._grants.append(grant)
            else:
                self._grants[existing] = grant
            self._persist_grants_unlocked()

    def find_matching_grant(self, request: PolicyRequest) -> ApprovalGrant | None:
        with _file_lock(self._grants_lock_path):
            self._grants = self._load_grants_unlocked()
            return self._find_matching_grant_unlocked(request)

    def _find_matching_grant_unlocked(self, request: PolicyRequest) -> ApprovalGrant | None:
        workspace_root = request.workspace_root or self.config.workspace_root
        for grant in self._grants:
            if grant.matches(request, workspace_root=Path(workspace_root)):
                return grant
        return None

    def _load_grants(self) -> list[ApprovalGrant]:
        with _file_lock(self._grants_lock_path):
            return self._load_grants_unlocked()

    def _load_grants_unlocked(self) -> list[ApprovalGrant]:
        path = Path(self.config.approval_grants_path)
        if not path.exists():
            return []
        grants: list[ApprovalGrant] = []
        for line in path.read_text(encoding="utf-8").splitlines():
            if not line.strip():
                continue
            grants.append(ApprovalGrant.from_dict(json.loads(line)))
        return grants

    def _persist_grants(self) -> None:
        with _file_lock(self._grants_lock_path):
            self._persist_grants_unlocked()

    def _persist_grants_unlocked(self) -> None:
        path = Path(self.config.approval_grants_path)
        path.parent.mkdir(parents=True, exist_ok=True)
        text = "".join(
            json.dumps(grant.to_dict(), ensure_ascii=False, sort_keys=True) + "\n"
            for grant in self._grants
        )
        tmp_path = path.with_suffix(path.suffix + ".tmp")
        tmp_path.write_text(text, encoding="utf-8")
        tmp_path.replace(path)

    def _consume_matching_grant(self, request: PolicyRequest) -> ApprovalGrant | None:
        with _file_lock(self._grants_lock_path):
            self._grants = self._load_grants_unlocked()
            grant = self._find_matching_grant_unlocked(request)
            if grant is None:
                return None
            grant.consume()
            self._persist_grants_unlocked()
            return grant

    def _consume_grant_object(self, grant: ApprovalGrant) -> ApprovalGrant | None:
        with _file_lock(self._grants_lock_path):
            self._grants = self._load_grants_unlocked()
            current = next(
                (candidate for candidate in self._grants if candidate.grant_id == grant.grant_id),
                None,
            )
            if current is None:
                self._grants.append(grant)
                current = grant
            if current.consumed:
                return None
            current.consume()
            self._persist_grants_unlocked()
            return current

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


@contextmanager
def _file_lock(path: Path):
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a+b") as handle:
        _lock_file(handle)
        try:
            yield
        finally:
            _unlock_file(handle)


def _lock_file(handle: Any) -> None:
    if os.name == "nt":
        import msvcrt

        handle.seek(0)
        msvcrt.locking(handle.fileno(), msvcrt.LK_LOCK, 1)
        return
    import fcntl

    fcntl.flock(handle.fileno(), fcntl.LOCK_EX)


def _unlock_file(handle: Any) -> None:
    if os.name == "nt":
        import msvcrt

        handle.seek(0)
        msvcrt.locking(handle.fileno(), msvcrt.LK_UNLCK, 1)
        return
    import fcntl

    fcntl.flock(handle.fileno(), fcntl.LOCK_UN)
