from __future__ import annotations

import json
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path

from singularity.policy.engine import PolicyRuntime
from singularity.policy.models import (
    ApprovalGrant,
    ApprovalRequirement,
    ApprovalScope,
    PolicyConstraints,
    PolicyDecision,
    PolicyRequest,
    PolicySubject,
    ResourceRef,
    stable_hash,
)


REQUEST_SCHEMA = "singularity.remote_approval_request/v1"
GRANT_SCHEMA = "singularity.remote_approval_grant/v1"


@dataclass(frozen=True)
class RemoteApprovalExport:
    path: Path
    request_id: str
    decision_id: str
    request_digest: str

    def to_dict(self) -> dict[str, object]:
        return {
            "path": str(self.path),
            "request_id": self.request_id,
            "decision_id": self.decision_id,
            "request_digest": self.request_digest,
        }


class RemoteApprovalRuntime:
    """File-backed remote approval adapter.

    This is a control-plane exchange format, not a network service. Operators
    can move request/grant JSON through any trusted channel, then import the
    scoped grant back into the local PolicyRuntime.
    """

    def __init__(self, workspace_root: Path | str, *, approval_dir: Path | None = None) -> None:
        self.workspace_root = Path(workspace_root).expanduser().resolve(strict=False)
        self.approval_dir = approval_dir or (
            self.workspace_root / ".singularity" / "remote" / "approvals"
        )

    def export_request(
        self,
        request: PolicyRequest,
        decision: PolicyDecision,
        output_path: Path | None = None,
    ) -> RemoteApprovalExport:
        request_payload = request.to_dict()
        decision_payload = decision.to_dict()
        digest = stable_hash({"request": request_payload, "decision": decision_payload})
        path = output_path or self.approval_dir / f"{request.request_id}.request.json"
        payload = {
            "schema_version": REQUEST_SCHEMA,
            "created_at": _now(),
            "workspace_root": str(self.workspace_root),
            "request": request_payload,
            "decision": decision_payload,
            "request_digest": digest,
        }
        _write_json(path, payload)
        return RemoteApprovalExport(
            path=path,
            request_id=request.request_id,
            decision_id=decision.decision_id,
            request_digest=digest,
        )

    def export_request_from_files(
        self,
        request_path: Path,
        decision_path: Path,
        *,
        output_path: Path | None = None,
    ) -> RemoteApprovalExport:
        request = _policy_request_from_dict(_read_json(request_path))
        decision = _policy_decision_from_dict(_read_json(decision_path))
        return self.export_request(request, decision, output_path=output_path)

    def import_grant(self, path: Path) -> ApprovalGrant:
        payload = _read_json(path)
        if payload.get("schema_version") != GRANT_SCHEMA:
            raise ValueError(f"Unsupported remote approval schema: {payload.get('schema_version')}")
        grant_payload = payload.get("grant")
        if not isinstance(grant_payload, dict):
            raise ValueError("Remote approval grant payload must contain a grant object.")
        grant = ApprovalGrant.from_dict(grant_payload)
        request_id = payload.get("request_id")
        decision_id = payload.get("decision_id")
        if request_id and grant.request_id != request_id:
            raise ValueError("Remote approval grant request_id does not match envelope.")
        if decision_id and grant.decision_id != decision_id:
            raise ValueError("Remote approval grant decision_id does not match envelope.")
        if not grant.approved_by.strip():
            raise ValueError("Remote approval grant must identify approved_by.")
        return grant

    def register_grant(self, path: Path, policy_runtime: PolicyRuntime) -> ApprovalGrant:
        grant = self.import_grant(path)
        policy_runtime.register_grant(grant)
        return grant


def _policy_request_from_dict(payload: dict[str, object]) -> PolicyRequest:
    return PolicyRequest(
        session_id=str(payload["session_id"]),
        task_id=str(payload["task_id"]),
        phase_id=str(payload["phase_id"]),
        action_id=str(payload["action_id"]),
        runtime=str(payload["runtime"]),
        operation=str(payload["operation"]),
        capability=str(payload["capability"]),
        subject=PolicySubject(**dict(payload.get("subject") or {})),
        resource=ResourceRef(**dict(payload.get("resource") or {})),
        reason=str(payload.get("reason") or ""),
        request_id=str(payload.get("request_id") or ""),
        proposed_by_model=bool(payload.get("proposed_by_model", False)),
        risk_tags=list(payload.get("risk_tags") or []),
        metadata=dict(payload.get("metadata") or {}),
        evidence_refs=list(payload.get("evidence_refs") or []),
        reversible=bool(payload.get("reversible", True)),
        requires_network=bool(payload.get("requires_network", False)),
        touches_workspace=bool(payload.get("touches_workspace", False)),
        touches_secrets=bool(payload.get("touches_secrets", False)),
        destructive=bool(payload.get("destructive", False)),
        long_running=bool(payload.get("long_running", False)),
        interactive=bool(payload.get("interactive", False)),
        workspace_root=payload.get("workspace_root"),
    )


def _policy_decision_from_dict(payload: dict[str, object]) -> PolicyDecision:
    required_approval = None
    raw_requirement = payload.get("required_approval")
    if isinstance(raw_requirement, dict):
        raw_scope = dict(raw_requirement.get("scope") or {})
        required_approval = ApprovalRequirement(
            message=str(raw_requirement.get("message") or ""),
            scope=ApprovalScope(
                capabilities=list(raw_scope.get("capabilities") or []),
                path_globs=list(raw_scope.get("path_globs") or []),
                command_patterns=list(raw_scope.get("command_patterns") or []),
                network_hosts=list(raw_scope.get("network_hosts") or []),
                max_duration_seconds=raw_scope.get("max_duration_seconds"),
                max_files=raw_scope.get("max_files"),
                session_only=bool(raw_scope.get("session_only", True)),
                single_use=bool(raw_scope.get("single_use", True)),
            ),
            review_kind=str(raw_requirement.get("review_kind") or "generic"),
            details=dict(raw_requirement.get("details") or {}),
        )
    constraints = PolicyConstraints(**dict(payload.get("constraints") or {}))
    return PolicyDecision(
        request_id=str(payload["request_id"]),
        outcome=str(payload["outcome"]),
        reason=str(payload.get("reason") or ""),
        risk_level=str(payload.get("risk_level") or "none"),
        risk_tags=list(payload.get("risk_tags") or []),
        user_message=str(payload.get("user_message") or ""),
        constraints=constraints,
        required_approval=required_approval,
        rule_ids=list(payload.get("rule_ids") or []),
        audit_severity=str(payload.get("audit_severity") or "info"),
        context_summary=str(payload.get("context_summary") or ""),
        decision_id=str(payload.get("decision_id") or ""),
        approval_grant_id=payload.get("approval_grant_id"),
    )


def _read_json(path: Path) -> dict[str, object]:
    payload = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(payload, dict):
        raise ValueError(f"Expected JSON object in {path}.")
    return payload


def _write_json(path: Path, payload: dict[str, object]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        json.dumps(payload, ensure_ascii=False, indent=2, sort_keys=True, default=str) + "\n",
        encoding="utf-8",
    )


def _now() -> str:
    return datetime.now(UTC).isoformat()
