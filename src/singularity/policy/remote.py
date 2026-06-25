from __future__ import annotations

import json
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

from singularity.policy.approval import ApprovalGate
from singularity.policy.models import (
    ApprovalGrant,
    ApprovalRequirement,
    ApprovalScope,
    DecisionOutcome,
    OperationKind,
    Capability,
    PolicyComponent,
    PolicyConstraints,
    PolicyDecision,
    PolicyRequest,
    RiskLevel,
    PolicySubject,
    ResourceRef,
    stable_hash,
)
from singularity.policy.operator_key import (
    load_operator_key,
    sign_grant,
    verify_grant_signature,
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


class RemoteApprovalExchange:
    """File-backed remote approval adapter.

    This is a control-plane exchange format, not a network service. Operators
    can move request/grant JSON through any trusted channel, then import the
    scoped grant back into the local ApprovalGate.
    """

    def __init__(
        self,
        workspace_root: Path | str,
        *,
        approval_dir: Path | None = None,
        operator_key_path: Path | None = None,
    ) -> None:
        self.workspace_root = Path(workspace_root).expanduser().resolve(strict=False)
        self.approval_dir = approval_dir or (
            self.workspace_root / ".singularity" / "remote" / "approvals"
        )
        self.operator_key_path = operator_key_path

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
        payload: dict[str, object] = {
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

    def export_grant(
        self,
        request_export_path: Path,
        grant: ApprovalGrant,
        *,
        output_path: Path | None = None,
    ) -> Path:
        """Build a grant payload from an exported request and a grant.

        Remote grant integrity and identity: the grant payload carries the
        original request/decision and ``request_digest`` so the importer can
        validate integrity and scope convergence. The grant is also signed
        with the operator key so importers can verify it was produced by a
        trusted operator and not forged by a process with write access to
        the grant store. Reviewers should use this helper to produce
        well-formed grant files.
        """
        request_export = _read_json(request_export_path)
        grant_dict = grant.to_dict()
        # Operator signature: bind the grant payload to the operator key so
        # importers can verify the grant was produced by a trusted operator.
        operator_key = load_operator_key(self.operator_key_path)
        signature_payload = _operator_signature_payload(grant_dict)
        grant_dict["operator_signature"] = sign_grant(signature_payload, operator_key)
        grant_payload: dict[str, object] = {
            "schema_version": GRANT_SCHEMA,
            "created_at": _now(),
            "request_id": request_export.get("request", {}).get("request_id", ""),
            "decision_id": request_export.get("decision", {}).get("decision_id", ""),
            "request": request_export.get("request"),
            "decision": request_export.get("decision"),
            "request_digest": request_export.get("request_digest"),
            "grant": grant_dict,
        }
        path = output_path or self.approval_dir / f"{grant.grant_id}.grant.json"
        _write_json(path, grant_payload)
        return path

    def import_grant(self, path: Path) -> ApprovalGrant:
        payload = _read_json(path)
        if payload.get("schema_version") != GRANT_SCHEMA:
            raise ValueError(f"Unsupported remote approval schema: {payload.get('schema_version')}")
        grant_payload = payload.get("grant")
        if not isinstance(grant_payload, dict):
            raise ValueError("Remote approval grant payload must contain a grant object.")

        # Operator signature: required for all imported grants. This binds
        # the approver identity to a cryptographic secret held outside the
        # workspace, preventing grant forgery by processes that can write
        # to the grant store. A missing or invalid signature is rejected
        # before any further validation.
        operator_signature = grant_payload.get("operator_signature")
        if not isinstance(operator_signature, str) or not operator_signature:
            raise ValueError("Remote approval grant must include operator_signature.")
        operator_key = load_operator_key(self.operator_key_path)
        signature_payload = _operator_signature_payload(grant_payload)
        if not verify_grant_signature(signature_payload, operator_signature, operator_key):
            raise ValueError("Remote approval grant operator_signature verification failed.")

        # Force ignore explicit grant_id: always use deterministic derivation
        # so a forged grant_id cannot collide with or override existing grants.
        # The deterministic id is derived from decision_id + request_id +
        # approved_by inside ``ApprovalGrant.from_dict``.
        grant_payload_for_import = dict(grant_payload)
        grant_payload_for_import.pop("grant_id", None)
        grant = ApprovalGrant.from_dict(grant_payload_for_import)

        request_id = payload.get("request_id")
        decision_id = payload.get("decision_id")
        if request_id and grant.request_id != request_id:
            raise ValueError("Remote approval grant request_id does not match envelope.")
        if decision_id and grant.decision_id != decision_id:
            raise ValueError("Remote approval grant decision_id does not match envelope.")
        if not grant.approved_by.strip():
            raise ValueError("Remote approval grant must identify approved_by.")

        # Remote grant integrity: validate request_digest to prevent tampering
        # with the request/decision payload. The digest must be present and
        # match the recomputed hash of the request and decision payloads.
        request_payload = payload.get("request")
        decision_payload = payload.get("decision")
        request_digest = payload.get("request_digest")
        if not request_digest:
            raise ValueError("Remote approval grant must include request_digest.")
        if not isinstance(request_payload, dict) or not isinstance(decision_payload, dict):
            raise ValueError("Remote approval grant must include request and decision payloads.")
        recomputed_digest = stable_hash(
            {"request": request_payload, "decision": decision_payload}
        )
        if recomputed_digest != request_digest:
            raise ValueError("Remote approval grant request_digest does not match payload.")

        # Remote grant integrity: validate scope convergence. The grant scope
        # must be a subset of the required approval scope from the decision
        # payload.
        _validate_scope_convergence(grant, decision_payload)

        return grant

    def register_grant(self, path: Path, approval_gate: ApprovalGate) -> ApprovalGrant:
        grant = self.import_grant(path)
        approval_gate.register_grant(grant)
        return grant


def _operator_signature_payload(grant_dict: dict[str, Any]) -> dict[str, Any]:
    """Build the canonical payload signed by the operator key.

    The signature covers the grant fields that bind the approver to the
    approval decision and scope. The ``operator_signature`` field itself
    is excluded so the signature is stable across export/import.
    """
    scope = grant_dict.get("scope")
    if not isinstance(scope, dict):
        scope = {}
    return {
        "decision_id": grant_dict.get("decision_id", ""),
        "request_id": grant_dict.get("request_id", ""),
        "approved_by": grant_dict.get("approved_by", ""),
        "scope": scope,
        "single_use": grant_dict.get("single_use", True),
        "session_only": scope.get("session_only", True),
    }


def _validate_scope_convergence(grant: ApprovalGrant, decision_payload: dict[str, object]) -> None:
    """Remote grant integrity: ensure the grant scope is a subset of the required approval scope.

    Without this check, a malicious reviewer could attach a wide-open scope
    (e.g. ``path_globs=["*"]``) to a grant that was only approved for a single
    file, amplifying a single approval into blanket access.

    Convergence is enforced across eight dimensions so the grant can never be
    wider than what the required approval permitted:

    - ``capabilities``: grant capabilities must be a subset of required.
    - ``path_globs``: grant path globs must be a subset of required.
    - ``command_patterns``: grant command patterns must be a subset of required.
    - ``network_hosts``: grant network hosts must be a subset of required.
    - ``single_use``: when required is single-use, the grant must also be.
    - ``session_only``: when required is session-only, the grant must also be.
    - ``max_duration_seconds``: grant must not exceed the required duration cap.
    - ``max_files``: grant must not exceed the required file count cap.
    """
    required_approval = decision_payload.get("required_approval")
    if not isinstance(required_approval, dict):
        return
    required_scope = required_approval.get("scope")
    if not isinstance(required_scope, dict):
        return

    required_capabilities = {
        str(item) for item in _list_payload(required_scope.get("capabilities"))
    }
    required_path_globs = {
        str(item) for item in _list_payload(required_scope.get("path_globs"))
    }
    required_command_patterns = {
        str(item) for item in _list_payload(required_scope.get("command_patterns"))
    }
    required_network_hosts = {
        str(item) for item in _list_payload(required_scope.get("network_hosts"))
    }

    grant_capabilities = {capability.value for capability in grant.scope.capabilities}
    if required_capabilities and not grant_capabilities.issubset(required_capabilities):
        raise ValueError(
            "Remote approval grant capabilities exceed the required approval scope."
        )
    if required_path_globs and not set(grant.scope.path_globs).issubset(required_path_globs):
        raise ValueError(
            "Remote approval grant path_globs exceed the required approval scope."
        )
    if required_command_patterns and not set(grant.scope.command_patterns).issubset(
        required_command_patterns
    ):
        raise ValueError(
            "Remote approval grant command_patterns exceed the required approval scope."
        )
    if required_network_hosts and not set(grant.scope.network_hosts).issubset(
        required_network_hosts
    ):
        raise ValueError(
            "Remote approval grant network_hosts exceed the required approval scope."
        )

    # Grant must be at least as restrictive as required: if required is
    # single_use, the grant must also be single_use.
    required_single_use = required_scope.get("single_use")
    if required_single_use is True and not grant.scope.single_use:
        raise ValueError(
            "Remote approval grant single_use must be True when required scope is single_use."
        )

    required_session_only = required_scope.get("session_only")
    if required_session_only is True and not grant.scope.session_only:
        raise ValueError(
            "Remote approval grant session_only must be True when required scope is session_only."
        )

    required_max_duration = required_scope.get("max_duration_seconds")
    if required_max_duration is not None:
        grant_max_duration = grant.scope.max_duration_seconds
        if grant_max_duration is None or grant_max_duration > required_max_duration:
            raise ValueError(
                "Remote approval grant max_duration_seconds exceeds the required approval scope."
            )

    required_max_files = required_scope.get("max_files")
    if required_max_files is not None:
        grant_max_files = grant.scope.max_files
        if grant_max_files is None or grant_max_files > required_max_files:
            raise ValueError(
                "Remote approval grant max_files exceeds the required approval scope."
            )


def _policy_request_from_dict(payload: dict[str, object]) -> PolicyRequest:
    subject_payload = payload.get("subject")
    resource_payload = payload.get("resource")
    metadata_payload = payload.get("metadata")
    return PolicyRequest(
        session_id=str(payload["session_id"]),
        task_id=str(payload["task_id"]),
        phase_id=str(payload["phase_id"]),
        action_id=str(payload["action_id"]),
        component=PolicyComponent(str(payload["component"])),
        operation=OperationKind(str(payload["operation"])),
        capability=Capability(str(payload["capability"])),
        subject=PolicySubject(**dict(subject_payload if isinstance(subject_payload, dict) else {})),
        resource=ResourceRef(**dict(resource_payload if isinstance(resource_payload, dict) else {})),
        reason=str(payload.get("reason") or ""),
        request_id=str(payload.get("request_id") or ""),
        proposed_by_model=bool(payload.get("proposed_by_model", False)),
        risk_tags=[str(item) for item in _list_payload(payload.get("risk_tags"))],
        metadata=dict(metadata_payload if isinstance(metadata_payload, dict) else {}),
        evidence_refs=[str(item) for item in _list_payload(payload.get("evidence_refs"))],
        reversible=bool(payload.get("reversible", True)),
        requires_network=bool(payload.get("requires_network", False)),
        touches_workspace=bool(payload.get("touches_workspace", False)),
        touches_secrets=bool(payload.get("touches_secrets", False)),
        destructive=bool(payload.get("destructive", False)),
        long_running=bool(payload.get("long_running", False)),
        interactive=bool(payload.get("interactive", False)),
        workspace_root=_optional_str(payload.get("workspace_root")),
    )


def _policy_decision_from_dict(payload: dict[str, object]) -> PolicyDecision:
    required_approval = None
    raw_requirement = payload.get("required_approval")
    if isinstance(raw_requirement, dict):
        raw_scope_value = raw_requirement.get("scope")
        raw_scope = dict(raw_scope_value if isinstance(raw_scope_value, dict) else {})
        details_value = raw_requirement.get("details")
        required_approval = ApprovalRequirement(
            message=str(raw_requirement.get("message") or ""),
            scope=ApprovalScope(
                capabilities=[Capability(str(item)) for item in _list_payload(raw_scope.get("capabilities"))],
                path_globs=[str(item) for item in _list_payload(raw_scope.get("path_globs"))],
                command_patterns=[str(item) for item in _list_payload(raw_scope.get("command_patterns"))],
                network_hosts=[str(item) for item in _list_payload(raw_scope.get("network_hosts"))],
                max_duration_seconds=_optional_int(raw_scope.get("max_duration_seconds")),
                max_files=_optional_int(raw_scope.get("max_files")),
                session_only=bool(raw_scope.get("session_only", True)),
                single_use=bool(raw_scope.get("single_use", True)),
            ),
            review_kind=str(raw_requirement.get("review_kind") or "generic"),
            details=dict(details_value if isinstance(details_value, dict) else {}),
        )
    constraints_payload = payload.get("constraints")
    constraints = PolicyConstraints(**dict(constraints_payload if isinstance(constraints_payload, dict) else {}))
    return PolicyDecision(
        request_id=str(payload["request_id"]),
        outcome=DecisionOutcome(str(payload["outcome"])),
        reason=str(payload.get("reason") or ""),
        risk_level=RiskLevel(str(payload.get("risk_level") or "none")),
        risk_tags=[str(item) for item in _list_payload(payload.get("risk_tags"))],
        user_message=str(payload.get("user_message") or ""),
        constraints=constraints,
        required_approval=required_approval,
        rule_ids=[str(item) for item in _list_payload(payload.get("rule_ids"))],
        audit_severity=str(payload.get("audit_severity") or "info"),
        context_summary=str(payload.get("context_summary") or ""),
        decision_id=str(payload.get("decision_id") or ""),
        approval_grant_id=_optional_str(payload.get("approval_grant_id")),
    )


def _list_payload(value: object) -> list[object]:
    return list(value) if isinstance(value, list) else []


def _optional_str(value: object) -> str | None:
    return str(value) if value is not None else None


def _optional_int(value: object) -> int | None:
    if value is None:
        return None
    if isinstance(value, bool):
        return int(value)
    if isinstance(value, (int, float, str)):
        return int(value)
    return None


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
