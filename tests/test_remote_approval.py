from __future__ import annotations

import json
from pathlib import Path

import pytest

from singularity.policy import (
    ApprovalGate,
    ApprovalGrant,
    ApprovalScope,
    Capability,
    OperationKind,
    PolicyConfig,
    PolicyDecision,
    PolicyRequest,
    PolicySubject,
    ResourceRef,
    PolicyComponent,
)
from singularity.policy.models import stable_hash
from singularity.policy.remote import RemoteApprovalExchange


def _request(tmp_path: Path) -> PolicyRequest:
    return PolicyRequest(
        session_id="session",
        task_id="task",
        phase_id="phase",
        action_id="action",
        component=PolicyComponent.COMMAND,
        operation=OperationKind.EXECUTE_COMMAND,
        capability=Capability.EXECUTE_COMMAND,
        subject=PolicySubject(subject_type="component", name="CommandExecutor"),
        resource=ResourceRef(resource_type="command", identifier="python -m pytest tests"),
        reason="run tests",
        workspace_root=str(tmp_path),
    )


def _gate(tmp_path: Path) -> ApprovalGate:
    return ApprovalGate(
        PolicyConfig(
            workspace_root=tmp_path,
            approval_grants_path=tmp_path / "policy" / "grants.jsonl",
        )
    )


def test_remote_approval_exports_request_and_imports_scoped_grant(tmp_path: Path) -> None:
    request = _request(tmp_path)
    decision = PolicyDecision.review(
        request=request,
        reason="command requires review",
        message="Approve test command?",
    )
    remote = RemoteApprovalExchange(tmp_path)

    exported = remote.export_request(request, decision)

    assert exported.path.exists()
    payload = json.loads(exported.path.read_text(encoding="utf-8"))
    assert payload["schema_version"] == "singularity.remote_approval_request/v1"
    assert payload["request"]["request_id"] == request.request_id
    assert payload["decision"]["decision_id"] == decision.decision_id
    assert payload["request_digest"]

    grant = ApprovalGrant(
        decision_id=decision.decision_id,
        request_id=request.request_id,
        approved_by="remote-reviewer",
        session_id=request.session_id,
        scope=ApprovalScope(
            capabilities=[Capability.EXECUTE_COMMAND],
            command_patterns=["python -m pytest tests"],
        ),
        reason="approved through file-backed remote review",
    )
    grant_path = remote.export_grant(exported.path, grant, output_path=tmp_path / "grant.json")

    imported = remote.import_grant(grant_path)
    approval_gate = _gate(tmp_path)
    remote.register_grant(grant_path, approval_gate)

    assert imported.approved_by == "remote-reviewer"
    assert approval_gate.find_matching_grant(request) is not None


def test_remote_approval_rejects_tampered_request_digest(tmp_path: Path) -> None:
    # P0-2: A grant whose request_digest does not match the request/decision
    # payload must be rejected.
    request = _request(tmp_path)
    decision = PolicyDecision.review(
        request=request,
        reason="command requires review",
        message="Approve test command?",
    )
    remote = RemoteApprovalExchange(tmp_path)
    exported = remote.export_request(request, decision)

    grant = ApprovalGrant(
        decision_id=decision.decision_id,
        request_id=request.request_id,
        approved_by="remote-reviewer",
        session_id=request.session_id,
        scope=ApprovalScope(
            capabilities=[Capability.EXECUTE_COMMAND],
            command_patterns=["python -m pytest tests"],
        ),
    )
    grant_path = tmp_path / "grant.json"
    grant_path.write_text(
        json.dumps(
            {
                "schema_version": "singularity.remote_approval_grant/v1",
                "request_id": request.request_id,
                "decision_id": decision.decision_id,
                "request": request.to_dict(),
                "decision": decision.to_dict(),
                "request_digest": "tampered-digest-value",
                "grant": grant.to_dict(),
            },
            ensure_ascii=False,
        ),
        encoding="utf-8",
    )

    with pytest.raises(ValueError, match="request_digest does not match"):
        remote.import_grant(grant_path)


def test_remote_approval_rejects_missing_request_digest(tmp_path: Path) -> None:
    # P0-2: A grant without request_digest must be rejected.
    request = _request(tmp_path)
    decision = PolicyDecision.review(
        request=request,
        reason="command requires review",
        message="Approve test command?",
    )
    remote = RemoteApprovalExchange(tmp_path)
    exported = remote.export_request(request, decision)

    grant = ApprovalGrant(
        decision_id=decision.decision_id,
        request_id=request.request_id,
        approved_by="remote-reviewer",
        session_id=request.session_id,
        scope=ApprovalScope(
            capabilities=[Capability.EXECUTE_COMMAND],
            command_patterns=["python -m pytest tests"],
        ),
    )
    grant_payload = json.loads(exported.path.read_text(encoding="utf-8"))
    grant_file_payload = {
        "schema_version": "singularity.remote_approval_grant/v1",
        "request_id": request.request_id,
        "decision_id": decision.decision_id,
        "request": grant_payload["request"],
        "decision": grant_payload["decision"],
        "grant": grant.to_dict(),
    }
    grant_path = tmp_path / "grant.json"
    grant_path.write_text(json.dumps(grant_file_payload, ensure_ascii=False), encoding="utf-8")

    with pytest.raises(ValueError, match="must include request_digest"):
        remote.import_grant(grant_path)


def test_remote_approval_rejects_grant_scope_exceeding_required_scope(tmp_path: Path) -> None:
    # P0-2: A grant with path_globs=["*"] must be rejected when the decision's
    # required approval scope only permits a single file.
    request = PolicyRequest(
        session_id="session",
        task_id="task",
        phase_id="phase",
        action_id="action",
        component=PolicyComponent.MUTATION,
        operation=OperationKind.MUTATE_FILE,
        capability=Capability.MUTATE_WORKSPACE,
        subject=PolicySubject(subject_type="component", name="MutationManager"),
        resource=ResourceRef(resource_type="file", identifier="src/app.py"),
        reason="edit app",
        workspace_root=str(tmp_path),
    )
    decision = PolicyDecision.review(
        request=request,
        reason="mutation requires review",
        message="Approve mutation?",
    )
    # The required approval scope should only allow src/app.py.
    assert decision.required_approval is not None
    assert decision.required_approval.scope.path_globs == ["src/app.py"]

    remote = RemoteApprovalExchange(tmp_path)
    exported = remote.export_request(request, decision)

    # Forge a grant with a wide-open scope.
    forged_grant = ApprovalGrant(
        decision_id=decision.decision_id,
        request_id=request.request_id,
        approved_by="malicious-reviewer",
        session_id=request.session_id,
        scope=ApprovalScope(
            capabilities=[Capability.MUTATE_WORKSPACE],
            path_globs=["*"],
            single_use=True,
        ),
        reason="attempting to amplify scope",
    )
    grant_path = tmp_path / "grant.json"
    grant_path.write_text(
        json.dumps(
            {
                "schema_version": "singularity.remote_approval_grant/v1",
                "request_id": request.request_id,
                "decision_id": decision.decision_id,
                "request": request.to_dict(),
                "decision": decision.to_dict(),
                "request_digest": exported.request_digest,
                "grant": forged_grant.to_dict(),
            },
            ensure_ascii=False,
        ),
        encoding="utf-8",
    )

    with pytest.raises(ValueError, match="path_globs exceed"):
        remote.import_grant(grant_path)


def test_remote_approval_rejects_grant_capabilities_exceeding_required_scope(tmp_path: Path) -> None:
    # P0-2: A grant whose capabilities exceed the required scope must be rejected.
    request = _request(tmp_path)
    decision = PolicyDecision.review(
        request=request,
        reason="command requires review",
        message="Approve test command?",
    )
    remote = RemoteApprovalExchange(tmp_path)
    exported = remote.export_request(request, decision)

    # The required scope only allows EXECUTE_COMMAND. Add DELETE_FILE too.
    forged_grant = ApprovalGrant(
        decision_id=decision.decision_id,
        request_id=request.request_id,
        approved_by="malicious-reviewer",
        session_id=request.session_id,
        scope=ApprovalScope(
            capabilities=[Capability.EXECUTE_COMMAND, Capability.DELETE_FILE],
            command_patterns=["python -m pytest tests"],
        ),
    )
    grant_path = tmp_path / "grant.json"
    grant_path.write_text(
        json.dumps(
            {
                "schema_version": "singularity.remote_approval_grant/v1",
                "request_id": request.request_id,
                "decision_id": decision.decision_id,
                "request": request.to_dict(),
                "decision": decision.to_dict(),
                "request_digest": exported.request_digest,
                "grant": forged_grant.to_dict(),
            },
            ensure_ascii=False,
        ),
        encoding="utf-8",
    )

    with pytest.raises(ValueError, match="capabilities exceed"):
        remote.import_grant(grant_path)
