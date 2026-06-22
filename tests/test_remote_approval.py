from __future__ import annotations

import json
from pathlib import Path

from singularity.policy import (
    ApprovalGrant,
    ApprovalScope,
    Capability,
    OperationKind,
    PolicyConfig,
    PolicyDecision,
    PolicyRequest,
    PolicyRuntime,
    PolicySubject,
    ResourceRef,
    RuntimeName,
)
from singularity.policy.remote import RemoteApprovalRuntime


def _request(tmp_path: Path) -> PolicyRequest:
    return PolicyRequest(
        session_id="session",
        task_id="task",
        phase_id="phase",
        action_id="action",
        runtime=RuntimeName.COMMAND,
        operation=OperationKind.EXECUTE_COMMAND,
        capability=Capability.EXECUTE_COMMAND,
        subject=PolicySubject(subject_type="runtime", name="CommandRuntime"),
        resource=ResourceRef(resource_type="command", identifier="python -m pytest tests"),
        reason="run tests",
        workspace_root=str(tmp_path),
    )


def test_remote_approval_exports_request_and_imports_scoped_grant(tmp_path: Path) -> None:
    request = _request(tmp_path)
    decision = PolicyDecision.review(
        request=request,
        reason="command requires review",
        message="Approve test command?",
    )
    remote = RemoteApprovalRuntime(tmp_path)

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
    grant_path = tmp_path / "grant.json"
    grant_path.write_text(
        json.dumps(
            {
                "schema_version": "singularity.remote_approval_grant/v1",
                "request_id": request.request_id,
                "decision_id": decision.decision_id,
                "grant": grant.to_dict(),
            },
            ensure_ascii=False,
        ),
        encoding="utf-8",
    )

    imported = remote.import_grant(grant_path)
    policy = PolicyRuntime(PolicyConfig(workspace_root=tmp_path))
    remote.register_grant(grant_path, policy)

    assert imported.approved_by == "remote-reviewer"
    assert policy.find_matching_grant(request) is not None
