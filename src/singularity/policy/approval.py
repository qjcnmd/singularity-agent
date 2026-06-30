from __future__ import annotations

import json
import os
from contextlib import contextmanager
from pathlib import Path
from typing import Any, Protocol, cast

from singularity.interaction import DecisionPrompt, InteractionController
from singularity.observability.models import TraceEventType, TraceSeverity
from singularity.policy.audit import redact_resource_identifier
from singularity.policy.config import PolicyConfig
from singularity.policy.exceptions import (
    ApprovalDenied,
    ApprovalRequired,
    PolicyAskUserRequired,
    PolicyDenied,
    PolicyEscalationRequired,
    SandboxRequired,
)
from singularity.policy.ledger import GrantConsumptionLedger
from singularity.policy.models import (
    ApprovalGrant,
    DecisionOutcome,
    PolicyDecision,
    PolicyRequest,
    approval_scope_for_request,
)


class _FcntlModule(Protocol):
    LOCK_EX: int
    LOCK_UN: int

    def flock(self, file_descriptor: int, operation: int) -> None:
        ...


class _MsvcrtModule(Protocol):
    LK_LOCK: int
    LK_UNLCK: int

    def locking(self, file_descriptor: int, mode: int, nbytes: int) -> None:
        ...


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
        self._grants_lock_path = _approval_grants_path(self.config).with_suffix(".lock")
        self._grants: list[ApprovalGrant] = self._load_grants()
        # Trust boundary: consumption truth lives in the append-only,
        # HMAC-chained GrantConsumptionLedger, never in a mutable field on
        # ApprovalGrant. The same operator key that signs remote grants also
        # signs ledger records, so the ledger inherits the operator key
        # configured on PolicyConfig.
        self._ledger = GrantConsumptionLedger(config, trace=trace)

    def is_grant_consumed(self, grant_id: str) -> bool:
        """Return True iff ``grant_id`` is recorded as consumed in the ledger."""
        return self._ledger.is_consumed(grant_id)

    def register_grant(self, grant: ApprovalGrant) -> None:
        with _file_lock(self._grants_lock_path):
            self._grants = self._load_grants_unlocked()
            # Grant identity: dedup by grant_id AND decision_id so a single
            # approval decision cannot be amplified into multiple consumable
            # grants. Repeated imports of the same grant (even without a
            # grant_id) resolve to the same deterministic ID and the same
            # decision_id, so they replace the prior entry instead of
            # appending a new one.
            existing = next(
                (
                    index
                    for index, candidate in enumerate(self._grants)
                    if candidate.grant_id == grant.grant_id
                    or candidate.decision_id == grant.decision_id
                ),
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

    def grants_store_path(self) -> Path:
        """Return the resolved path used to persist approval grants."""
        return _approval_grants_path(self.config)

    def is_grant_store_trusted(self, workspace_root: Path | str) -> bool:
        """Return True when the grant store lives outside the workspace.

        Trust boundary: grants persisted inside the model-writable workspace
        are considered untrusted because the model could forge them via shell
        writes. Only grants stored outside the workspace may be consumed
        automatically by ToolExecutor.
        """
        grant_path = _approval_grants_path(self.config).resolve(strict=False)
        root = Path(workspace_root).expanduser().resolve(strict=False)
        try:
            root_key = os.path.normcase(os.path.normpath(str(root)))
            path_key = os.path.normcase(os.path.normpath(str(grant_path)))
            return os.path.commonpath([root_key, path_key]) != root_key
        except (OSError, ValueError):
            return False

    def consume_matching_grant(self, request: PolicyRequest) -> ApprovalGrant | None:
        with _file_lock(self._grants_lock_path):
            self._grants = self._load_grants_unlocked()
            grant = self._find_matching_grant_unlocked(request)
            if grant is None:
                return None
            if self._ledger.is_consumed(grant.grant_id):
                return None
            self._ledger.consume(grant, request=request)
            return grant

    def consume_grant(self, grant: ApprovalGrant) -> ApprovalGrant | None:
        with _file_lock(self._grants_lock_path):
            self._grants = self._load_grants_unlocked()
            current = next(
                (
                    candidate
                    for candidate in self._grants
                    if candidate.grant_id == grant.grant_id
                ),
                None,
            )
            if current is None:
                self._grants.append(grant)
                current = grant
                self._persist_grants_unlocked()
            if self._ledger.is_consumed(current.grant_id):
                return None
            # No PolicyRequest is available at this entry point: bind the
            # consumption record to grant fields (grant_id, decision_id,
            # request_id, session_id). The request_digest still uniquely
            # binds the consumption event for replay detection.
            self._ledger.consume(current, request=None)
            return current

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
                summary="Review required but no InteractionController is configured.",
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

    def authorize(
        self,
        request: PolicyRequest,
        decision: PolicyDecision,
    ) -> ApprovalGrant | None:
        """Resolve and consume one grant at an authoritative execution boundary."""
        if decision.outcome == DecisionOutcome.ALLOW:
            return None
        if decision.outcome != DecisionOutcome.REQUIRE_REVIEW:
            self.resolve(request, decision)
            return None

        workspace_root = request.workspace_root or self.config.workspace_root
        if self.is_grant_store_trusted(workspace_root):
            existing = self.consume_matching_grant(request)
            if existing is not None:
                return existing

        grant = self.resolve(request, decision)
        if grant is None:
            raise ApprovalRequired(decision.reason)
        self.register_grant(grant)
        consumed = self.consume_grant(grant)
        if consumed is None:
            raise ApprovalRequired("Approval grant was already consumed.")
        return consumed

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

    def _find_matching_grant_unlocked(self, request: PolicyRequest) -> ApprovalGrant | None:
        workspace_root = request.workspace_root or self.config.workspace_root
        for grant in self._grants:
            if not grant.matches(request, workspace_root=Path(workspace_root)):
                continue
            # Trust boundary: a single_use grant whose consumption is recorded
            # in the append-only ledger is no longer a match for any request,
            # even if its scope/session/expiry still match. This keeps
            # find_matching_grant consistent with consume_matching_grant.
            if grant.single_use and self._ledger.is_consumed(grant.grant_id):
                continue
            return grant
        return None

    def _load_grants(self) -> list[ApprovalGrant]:
        with _file_lock(self._grants_lock_path):
            return self._load_grants_unlocked()

    def _load_grants_unlocked(self) -> list[ApprovalGrant]:
        path = _approval_grants_path(self.config)
        if not path.exists():
            return []
        grants: list[ApprovalGrant] = []
        for line in path.read_text(encoding="utf-8").splitlines():
            if line.strip():
                grants.append(ApprovalGrant.from_dict(json.loads(line)))
        return grants

    def _persist_grants_unlocked(self) -> None:
        path = _approval_grants_path(self.config)
        path.parent.mkdir(parents=True, exist_ok=True)
        text = "".join(
            json.dumps(grant.to_dict(), ensure_ascii=False, sort_keys=True) + "\n"
            for grant in self._grants
        )
        tmp_path = path.with_suffix(path.suffix + ".tmp")
        tmp_path.write_text(text, encoding="utf-8")
        tmp_path.replace(path)


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
        _lock_file_windows(handle)
        return
    _lock_file_posix(handle)


def _unlock_file(handle: Any) -> None:
    if os.name == "nt":
        _unlock_file_windows(handle)
        return
    _unlock_file_posix(handle)


def _lock_file_windows(handle: Any) -> None:
    import msvcrt

    windows_lock = cast(_MsvcrtModule, msvcrt)
    handle.seek(0)
    windows_lock.locking(handle.fileno(), windows_lock.LK_LOCK, 1)


def _unlock_file_windows(handle: Any) -> None:
    import msvcrt

    windows_lock = cast(_MsvcrtModule, msvcrt)
    handle.seek(0)
    windows_lock.locking(handle.fileno(), windows_lock.LK_UNLCK, 1)


def _lock_file_posix(handle: Any) -> None:
    import fcntl

    posix_lock = cast(_FcntlModule, fcntl)
    posix_lock.flock(handle.fileno(), posix_lock.LOCK_EX)


def _unlock_file_posix(handle: Any) -> None:
    import fcntl

    posix_lock = cast(_FcntlModule, fcntl)
    posix_lock.flock(handle.fileno(), posix_lock.LOCK_UN)


def _approval_grants_path(config: PolicyConfig) -> Path:
    if config.approval_grants_path is None:
        # Trust boundary: default grant store must live outside the
        # model-writable workspace so the model cannot forge grants via
        # shell writes.
        from singularity.policy.config import _default_policy_home
        return _default_policy_home() / ".singularity" / "policy" / "approval_grants.jsonl"
    return Path(config.approval_grants_path)
