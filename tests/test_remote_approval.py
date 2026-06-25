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
from singularity.policy.operator_key import (
    default_operator_key_path,
    generate_operator_key,
    load_operator_key,
    sign_grant,
)
from singularity.policy.remote import RemoteApprovalExchange, _operator_signature_payload


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


def _sign_grant_dict(grant_dict: dict, operator_key: bytes) -> str:
    """Sign a grant dict using the same canonical payload as production code."""
    return sign_grant(_operator_signature_payload(grant_dict), operator_key)


@pytest.fixture(autouse=True)
def operator_key() -> bytes:
    """Generate a temporary operator key under the isolated policy home.

    The conftest ``_isolate_policy_home`` fixture redirects
    ``SINGULARITY_POLICY_HOME`` to ``tmp_path``, so the key is generated at
    ``tmp_path/.singularity/policy/operator.pem`` and never touches the real
    home directory. All tests in this module exercise the signing/verification
    path, so the key is generated for every test. Tests that need the raw
    key bytes (for manual signing) can request this fixture by name.
    """
    key_path = default_operator_key_path()
    if not key_path.exists():
        generate_operator_key(key_path)
    return load_operator_key(key_path)


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


def test_remote_approval_rejects_tampered_request_digest(
    tmp_path: Path, operator_key: bytes
) -> None:
    # Remote grant integrity: a grant whose request_digest does not match the
    # request/decision payload must be rejected. The grant carries a valid
    # operator_signature so the test reaches the digest check.
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
    grant_dict = grant.to_dict()
    grant_dict["operator_signature"] = _sign_grant_dict(grant_dict, operator_key)
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
                "grant": grant_dict,
            },
            ensure_ascii=False,
        ),
        encoding="utf-8",
    )

    with pytest.raises(ValueError, match="request_digest does not match"):
        remote.import_grant(grant_path)


def test_remote_approval_rejects_missing_request_digest(
    tmp_path: Path, operator_key: bytes
) -> None:
    # Remote grant integrity: a grant without request_digest must be rejected.
    # The grant carries a valid operator_signature so the test reaches the
    # missing-digest check.
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
    grant_dict = grant.to_dict()
    grant_dict["operator_signature"] = _sign_grant_dict(grant_dict, operator_key)
    grant_file_payload = {
        "schema_version": "singularity.remote_approval_grant/v1",
        "request_id": request.request_id,
        "decision_id": decision.decision_id,
        "request": grant_payload["request"],
        "decision": grant_payload["decision"],
        "grant": grant_dict,
    }
    grant_path = tmp_path / "grant.json"
    grant_path.write_text(json.dumps(grant_file_payload, ensure_ascii=False), encoding="utf-8")

    with pytest.raises(ValueError, match="must include request_digest"):
        remote.import_grant(grant_path)


def test_remote_approval_rejects_grant_scope_exceeding_required_scope(
    tmp_path: Path, operator_key: bytes
) -> None:
    # Remote grant integrity: a grant with path_globs=["*"] must be rejected
    # when the decision's required approval scope only permits a single file.
    # The grant carries a valid operator_signature so the test reaches the
    # scope convergence check.
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
    grant_dict = forged_grant.to_dict()
    grant_dict["operator_signature"] = _sign_grant_dict(grant_dict, operator_key)
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
                "grant": grant_dict,
            },
            ensure_ascii=False,
        ),
        encoding="utf-8",
    )

    with pytest.raises(ValueError, match="path_globs exceed"):
        remote.import_grant(grant_path)


def test_remote_approval_rejects_grant_capabilities_exceeding_required_scope(
    tmp_path: Path, operator_key: bytes
) -> None:
    # Remote grant integrity: a grant whose capabilities exceed the required
    # scope must be rejected. The grant carries a valid operator_signature so
    # the test reaches the scope convergence check.
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
    grant_dict = forged_grant.to_dict()
    grant_dict["operator_signature"] = _sign_grant_dict(grant_dict, operator_key)
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
                "grant": grant_dict,
            },
            ensure_ascii=False,
        ),
        encoding="utf-8",
    )

    with pytest.raises(ValueError, match="capabilities exceed"):
        remote.import_grant(grant_path)


# ---------------------------------------------------------------------------
# Operator HMAC signature tests (defect C)
# ---------------------------------------------------------------------------


def test_remote_approval_rejects_missing_operator_signature(tmp_path: Path) -> None:
    # Defect C: a grant without operator_signature must be rejected before any
    # further validation. This prevents any process with write access to the
    # grant store from forging a valid grant.
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
    # Deliberately omit operator_signature from the grant payload.
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
                "grant": grant.to_dict(),
            },
            ensure_ascii=False,
        ),
        encoding="utf-8",
    )

    with pytest.raises(ValueError, match="must include operator_signature"):
        remote.import_grant(grant_path)


def test_remote_approval_rejects_tampered_operator_signature(tmp_path: Path) -> None:
    # Defect C: a grant with a tampered operator_signature must be rejected.
    # The signature is verified with constant-time comparison so a forged
    # signature cannot pass even if an attacker knows the canonical payload.
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
    grant_dict = grant.to_dict()
    # Tamper with the signature: use a valid-looking hex string that does not
    # match the recomputed HMAC.
    grant_dict["operator_signature"] = "a" * 64
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
                "grant": grant_dict,
            },
            ensure_ascii=False,
        ),
        encoding="utf-8",
    )

    with pytest.raises(ValueError, match="operator_signature verification failed"):
        remote.import_grant(grant_path)


def test_remote_approval_accepts_valid_operator_signature(tmp_path: Path) -> None:
    # Defect C: a grant with a correct operator_signature produced by
    # ``export_grant`` must be accepted and importable.
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
        reason="approved through file-backed remote review",
    )
    grant_path = remote.export_grant(exported.path, grant, output_path=tmp_path / "grant.json")

    imported = remote.import_grant(grant_path)
    assert imported.approved_by == "remote-reviewer"
    assert imported.decision_id == decision.decision_id
    assert imported.request_id == request.request_id

    # The imported grant should be usable by an ApprovalGate.
    approval_gate = _gate(tmp_path)
    remote.register_grant(grant_path, approval_gate)
    assert approval_gate.find_matching_grant(request) is not None


def test_remote_approval_ignores_explicit_grant_id(
    tmp_path: Path, operator_key: bytes
) -> None:
    # Defect C: import_grant must ignore any explicit grant_id in the payload
    # and always use the deterministic derivation from decision_id +
    # request_id + approved_by. This prevents a forged grant_id from
    # colliding with or overriding existing grants.
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
    grant_dict = grant.to_dict()
    # Inject an explicit, attacker-chosen grant_id and sign the payload.
    grant_dict["grant_id"] = "grant_attacker_chosen_id"
    grant_dict["operator_signature"] = _sign_grant_dict(grant_dict, operator_key)
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
                "grant": grant_dict,
            },
            ensure_ascii=False,
        ),
        encoding="utf-8",
    )

    imported = remote.import_grant(grant_path)
    # The explicit grant_id must be ignored; the deterministic id is derived
    # from decision_id + request_id + approved_by.
    assert imported.grant_id != "grant_attacker_chosen_id"
    assert imported.grant_id.startswith("grant_")
    # Re-importing the same payload must produce the same deterministic id.
    imported_again = remote.import_grant(grant_path)
    assert imported_again.grant_id == imported.grant_id


def test_export_grant_includes_operator_signature(tmp_path: Path) -> None:
    # Defect C: ``export_grant`` must attach ``operator_signature`` to the
    # grant payload so importers can verify grant authenticity.
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
    grant_path = remote.export_grant(exported.path, grant, output_path=tmp_path / "grant.json")

    payload = json.loads(grant_path.read_text(encoding="utf-8"))
    grant_payload = payload["grant"]
    assert "operator_signature" in grant_payload
    assert isinstance(grant_payload["operator_signature"], str)
    assert len(grant_payload["operator_signature"]) == 64  # sha256 hex digest

    # The signature must be verifiable against the canonical payload.
    operator_key = load_operator_key(default_operator_key_path())
    from singularity.policy.operator_key import verify_grant_signature

    assert verify_grant_signature(
        _operator_signature_payload(grant_payload),
        grant_payload["operator_signature"],
        operator_key,
    )
